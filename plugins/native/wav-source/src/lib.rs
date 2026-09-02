//! wav-source —— 把一个 stereo WAV 播成两列的口（`l` / `r`）。
//!
//! 为什么存在：**示波器音乐**——两路音频驱动示波器的 X-Y，屏上是画不是波形。
//! `example/oscilloscope-music.sh` 用它放 Primer（Revision 2025 Wild 冠军）。
//!
//! 三件事跟契约有关，都不是随手定的：
//!
//! 1. **不声明 `declared_rate_hz`**：文件的采样率是运行期才知道的，而声明的率必须
//!    是编译期常量。所以每帧带 `sfrate01` 注解说实际率（同 sig-gen）。
//!
//!    **不声明率 ≠ 没有率**——第一帧的注解一到口就有率了，示波器四种时钟都建得
//!    起来。我先前写过"所以时钟只能 native"，是**在一个为真的前提后面顺手接了一
//!    个没验证的推论**，sigflow-core 在 sig-gen（同一形状）上四种组合全跑通，当场
//!    否掉。真正的代价只有一条：**率是第一帧才知道的观测**，所以还没开播时示波器
//!    停在"在等口出第一帧"，看着像坏了。
//! 2. **只收 stereo**：X-Y 要两路不同的信号，单声道画出来是一条对角线。别的声道数
//!    当场报错并说清怎么转，不猜、不复制一路凑数。
//! 3. **循环回到开头不设不连续标记**：那个标记说的是"采样轴断了/丢了拍"，而循环时
//!    时间连着走、一拍没丢，断的是**内容**。给它安一个意思相近但不对的标记，下游会
//!    照着切分段——那是拿一个维度的东西去说另一个维度的事（同 `z_dev` 的 NaN 那笔账）。
//!
//! 出帧节奏按墙钟（同 sig-gen）：每个 tick 只发到"此刻该存在"的样本数，
//! `sample_index` / `t0_ns` 是真时间。整个 data 段读进内存后**按帧解码**，所以内存
//! ≈ 文件大小，而不是解码后的 f32 大小。
//!
//! 契约在同目录的 `manifest.toml`。

use sigflow_plugin_sdk::{
    encode_rate_annotation, export_plugin, mono_ns, Frame, FrameOut, ParamValue, Plugin,
    PluginManifest, ProcessOutcome,
};
use sigflow_types::frame::RATE_ANNOTATION_SCHEMA;

/// 口是两列，静态声明。
const CHANNELS: usize = 2;
/// 帧尾留给 sfrate01 注解的字节。
const ANNOTATION_RESERVE: usize = 64;

/// data 段里一个样本的存法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enc {
    I16,
    I24,
    I32,
    F32,
}

impl Enc {
    fn bytes(self) -> usize {
        match self {
            Enc::I16 => 2,
            Enc::I24 => 3,
            Enc::I32 | Enc::F32 => 4,
        }
    }

    /// 一个样本 → 归一化到 ±1 的 f32。
    #[inline(always)]
    fn read(self, b: &[u8]) -> f32 {
        match self {
            Enc::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            // 24 位：补最低字节再当 i32 读，右移回去 = 符号扩展
            Enc::I24 => {
                (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0
            }
            Enc::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            Enc::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        }
    }
}

/// 读进内存的一个 WAV：原字节 + 怎么解。
///
/// `Debug` 只印形状不印 `data`——一个 WAV 的 data 段几十兆，测试失败时把它打出来
/// 等于把输出淹掉，而形状（率 / 拍数 / 编码）才是失败时想看的。
struct Wav {
    data: Vec<u8>,
    enc: Enc,
    fs_hz: f64,
    /// 总拍数（每拍两个样本）。
    frames: u64,
    /// 一拍多少字节。
    stride: usize,
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl std::fmt::Debug for Wav {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wav")
            .field("enc", &self.enc)
            .field("fs_hz", &self.fs_hz)
            .field("frames", &self.frames)
            .field("data_bytes", &self.data.len())
            .finish()
    }
}

impl Wav {
    fn load(path: &str) -> Result<Wav, String> {
        let raw = std::fs::read(path).map_err(|e| format!("读不了 {path}：{e}"))?;
        Wav::parse(&raw).map_err(|e| format!("{path}：{e}"))
    }

    fn parse(raw: &[u8]) -> Result<Wav, String> {
        if raw.len() < 12 || &raw[0..4] != b"RIFF" || &raw[8..12] != b"WAVE" {
            return Err("不是 RIFF/WAVE 文件（要 WAV；FLAC/MP3 先转：ffmpeg -i 输入 输出.wav）"
                .to_string());
        }
        let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format, channels, fs, bits)
        let mut data: Option<(usize, usize)> = None; // (start, len)
        let mut p = 12usize;
        while p + 8 <= raw.len() {
            let id = &raw[p..p + 4];
            let sz = u32le(raw, p + 4) as usize;
            let body = p + 8;
            // 块长可能撒谎（截断的文件）：按实际剩余截断，别 panic
            let end = body.saturating_add(sz).min(raw.len());
            if id == b"fmt " && end - body >= 16 {
                let mut format = u16le(raw, body);
                let channels = u16le(raw, body + 2);
                let fs = u32le(raw, body + 4);
                let bits = u16le(raw, body + 14);
                // WAVE_FORMAT_EXTENSIBLE：真正的格式在子格式 GUID 的头两字节
                if format == 0xFFFE && end - body >= 26 {
                    format = u16le(raw, body + 24);
                }
                fmt = Some((format, channels, fs, bits));
            } else if id == b"data" {
                data = Some((body, end - body));
            }
            // 块按偶数对齐
            p = body + sz + (sz & 1);
        }
        let (format, channels, fs, bits) = fmt.ok_or("没有 fmt 块")?;
        let (start, len) = data.ok_or("没有 data 块")?;
        if channels as usize != CHANNELS {
            return Err(format!(
                "这个文件有 {channels} 个声道，这个源只收 stereo（2 个）——X-Y 要两路不同的\
                 信号，单声道画出来是一条对角线。转一下：ffmpeg -i 输入 -ac 2 输出.wav"
            ));
        }
        let enc = match (format, bits) {
            (1, 16) => Enc::I16,
            (1, 24) => Enc::I24,
            (1, 32) => Enc::I32,
            (3, 32) => Enc::F32,
            (1, b) => return Err(format!("不认识的位深 {b}（PCM 只认 16 / 24 / 32）")),
            (3, b) => return Err(format!("浮点 WAV 只认 32 位，这个是 {b}")),
            (f, b) => return Err(format!("不认识的编码（format = {f}, bits = {b}）；\
                                          压缩过的先转：ffmpeg -i 输入 输出.wav")),
        };
        if !(fs > 0) {
            return Err("采样率是 0".to_string());
        }
        let stride = enc.bytes() * CHANNELS;
        let frames = (len / stride) as u64;
        if frames == 0 {
            return Err("data 块里一拍都没有".to_string());
        }
        Ok(Wav {
            data: raw[start..start + frames as usize * stride].to_vec(),
            enc,
            fs_hz: fs as f64,
            frames,
            stride,
        })
    }

    /// 第 `frame` 拍、第 `ch` 路。调用方保证 `frame < self.frames`。
    #[inline(always)]
    fn at(&self, frame: u64, ch: usize) -> f32 {
        let off = frame as usize * self.stride + ch * self.enc.bytes();
        self.enc.read(&self.data[off..])
    }
}

pub struct WavSource {
    path: String,
    loop_play: bool,
    gain: f64,
    speed: f64,

    wav: Option<Wav>,
    /// 加载失败的原因——留到 `start()` 报，参数是热改的，改错了不该当场把节点弄死。
    load_err: Option<String>,
    /// 参数变了要重新加载。
    reload: bool,
    /// 放完了（不循环）——只报一次。
    finished: bool,

    seq: u64,
    /// 已发的拍数（输出轴，只增）。
    index: u64,
    /// 文件里的读头（拍）。
    cursor: u64,
    anchor_index: u64,
    anchor_t0_ns: i64,
    fs_out: f64,
    pending_disc: bool,
    rate_annotation: Vec<u8>,
}

impl WavSource {
    fn apply_path(&mut self) {
        self.reload = false;
        self.finished = false;
        self.cursor = 0;
        if self.path.trim().is_empty() {
            self.wav = None;
            self.load_err = None;
            return;
        }
        match Wav::load(self.path.trim()) {
            Ok(w) => {
                self.wav = Some(w);
                self.load_err = None;
            }
            Err(e) => {
                self.wav = None;
                self.load_err = Some(e);
            }
        }
        self.retune();
    }

    /// 率变了 = 新的采样轴：重新锚定，下一帧带不连续标记。
    fn retune(&mut self) {
        let fs = self.wav.as_ref().map(|w| w.fs_hz).unwrap_or(0.0) * self.speed;
        if fs <= 0.0 || (fs - self.fs_out).abs() < f64::EPSILON {
            return;
        }
        self.fs_out = fs;
        self.anchor_index = self.index;
        self.anchor_t0_ns = mono_ns();
        self.pending_disc = true;
        self.rate_annotation = encode_rate_annotation(fs);
    }

    fn fill(&mut self, buf: &mut [u8], scans: usize) -> usize {
        let Some(w) = self.wav.as_ref() else {
            return 0;
        };
        let g = self.gain as f32;
        let mut n = 0usize;
        for s in 0..scans {
            let f = self.cursor + s as u64;
            let f = if f < w.frames { f } else { f % w.frames };
            for ch in 0..CHANNELS {
                let v = (w.at(f, ch) * g).clamp(-1.0, 1.0);
                let at = n;
                buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
                n += 4;
            }
        }
        n
    }
}

impl Plugin for WavSource {
    fn new(_manifest: &PluginManifest) -> Self {
        WavSource {
            path: String::new(),
            loop_play: true,
            gain: 1.0,
            speed: 1.0,
            wav: None,
            load_err: None,
            reload: false,
            finished: false,
            seq: 0,
            index: 0,
            cursor: 0,
            anchor_index: 0,
            anchor_t0_ns: 0,
            fs_out: 0.0,
            pending_disc: false,
            rate_annotation: Vec::new(),
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("path", ParamValue::String(s)) => {
                if s.trim() != self.path.trim() {
                    self.path = s.clone();
                    self.reload = true;
                }
            }
            ("loop_play", ParamValue::Bool(b)) => self.loop_play = *b,
            ("gain", ParamValue::F64(v)) => self.gain = *v,
            ("speed", ParamValue::F64(v)) => {
                if *v > 0.0 && (*v - self.speed).abs() > f64::EPSILON {
                    self.speed = *v;
                    self.retune();
                }
            }
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        self.apply_path();
        self.index = 0;
        self.seq = 0;
        self.anchor_index = 0;
        self.anchor_t0_ns = mono_ns();
        // 加载失败在这里报：这时候人正按下 Run，看得见
        if let Some(e) = &self.load_err {
            return ProcessOutcome::Fault { reason: e.clone() };
        }
        if let Some(w) = &self.wav {
            self.fs_out = w.fs_hz * self.speed;
            self.rate_annotation = encode_rate_annotation(self.fs_out);
        }
        self.pending_disc = false;
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        if self.reload {
            self.apply_path();
            if let Some(e) = &self.load_err {
                return ProcessOutcome::Fault { reason: e.clone() };
            }
            if let Some(w) = &self.wav {
                self.fs_out = w.fs_hz * self.speed;
                self.rate_annotation = encode_rate_annotation(self.fs_out);
                self.anchor_index = self.index;
                self.anchor_t0_ns = mono_ns();
                self.pending_disc = true;
            }
        }
        let (Some(out), Some(w)) = (outputs.get_mut(0), self.wav.as_ref()) else {
            return ProcessOutcome::Ok;
        };
        if self.finished || self.fs_out <= 0.0 {
            return ProcessOutcome::Ok;
        }
        let frames_total = w.frames;
        let cap = out.capacity().saturating_sub(ANNOTATION_RESERVE) / (CHANNELS * 4);
        let elapsed = (mono_ns() - self.anchor_t0_ns).max(0) as f64 / 1e9;
        let target = self.anchor_index + (elapsed * self.fs_out) as u64;
        let mut scans = target.saturating_sub(self.index).min(cap as u64) as usize;
        if !self.loop_play {
            // 不循环：放到文件尾就停，别把开头又接上去
            scans = scans.min(frames_total.saturating_sub(self.cursor) as usize);
        }
        if scans == 0 {
            if !self.loop_play && self.cursor >= frames_total {
                self.finished = true;
            }
            return ProcessOutcome::Ok;
        }

        let (index, seq, anchor_index, anchor_t0_ns, fs, disc) = (
            self.index,
            self.seq,
            self.anchor_index,
            self.anchor_t0_ns,
            self.fs_out,
            self.pending_disc,
        );
        let written = {
            let buf = out.buffer_mut();
            self.fill(buf, scans)
        };
        out.set_written(written);
        out.header.seq = seq;
        out.header.sample_index = index;
        out.header.n_samples = scans as u32;
        out.header.t0_ns =
            anchor_t0_ns + (((index - anchor_index) as f64 / fs) * 1e9) as i64;
        // 循环回到开头**不设**不连续标记：时间连着走、一拍没丢，断的是内容不是采样轴。
        if disc {
            out.header.set_discontinuity(0);
            self.pending_disc = false;
        }
        if !out.set_annotation(RATE_ANNOTATION_SCHEMA, &self.rate_annotation) {
            // 只有容量算错才会到这里；率注解是契约的一部分，宁可这一帧不发
            out.set_written(0);
            return ProcessOutcome::Ok;
        }
        self.seq += 1;
        self.index += scans as u64;
        self.cursor = (self.cursor + scans as u64) % frames_total.max(1);
        ProcessOutcome::Ok
    }

    fn invoke_action(&mut self, id: &str) {
        if id == "restart" {
            // 读头回到开头，但**输出轴不回退**：seq / sample_index 只增，
            // 否则下游会被告知时间倒流了。
            self.cursor = 0;
            self.finished = false;
        }
    }
}

export_plugin!(WavSource);

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个 WAV：`fmt` 是 (format, bits)，`frames` 是每拍两个样本的原始字节。
    fn wav(format: u16, bits: u16, channels: u16, fs: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&0u32.to_le_bytes()); // 长度：解析器不看
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&format.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&fs.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // byte rate
        v.extend_from_slice(&0u16.to_le_bytes()); // block align
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn 十六位_立体声_解出来的值和采样率都对() {
        let mut body = Vec::new();
        for (l, r) in [(0i16, 32767i16), (-32768, 1000), (16384, -16384)] {
            body.extend_from_slice(&l.to_le_bytes());
            body.extend_from_slice(&r.to_le_bytes());
        }
        let w = Wav::parse(&wav(1, 16, 2, 48000, &body)).unwrap();
        assert_eq!(w.fs_hz, 48000.0);
        assert_eq!(w.frames, 3);
        assert_eq!(w.at(0, 0), 0.0);
        assert!((w.at(0, 1) - 0.999969).abs() < 1e-5);
        assert_eq!(w.at(1, 0), -1.0);
        assert!((w.at(2, 0) - 0.5).abs() < 1e-6);
        assert!((w.at(2, 1) + 0.5).abs() < 1e-6);
    }

    #[test]
    fn 二十四位与三十二位浮点都认_而且符号扩展是对的() {
        // 24 位：-1 应该是 0xFFFFFF
        let body: Vec<u8> = vec![0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x40];
        let w = Wav::parse(&wav(1, 24, 2, 96000, &body)).unwrap();
        assert_eq!(w.frames, 1);
        assert!(w.at(0, 0) < 0.0, "负数被读成了正数：符号没扩展");
        assert!((w.at(0, 0) + 1.0 / 8_388_608.0).abs() < 1e-9);
        assert!((w.at(0, 1) - 0.5).abs() < 1e-6);

        let mut fb = Vec::new();
        fb.extend_from_slice(&0.25f32.to_le_bytes());
        fb.extend_from_slice(&(-0.75f32).to_le_bytes());
        let w = Wav::parse(&wav(3, 32, 2, 44100, &fb)).unwrap();
        assert_eq!(w.at(0, 0), 0.25);
        assert_eq!(w.at(0, 1), -0.75);
    }

    #[test]
    fn 单声道要当场拒_并且说清怎么转() {
        let e = Wav::parse(&wav(1, 16, 1, 48000, &[0, 0, 0, 0])).unwrap_err();
        assert!(e.contains("1 个声道"), "{e}");
        assert!(e.contains("对角线"), "要说清为什么不是随口拒：{e}");
        assert!(e.contains("ffmpeg"), "要给出路：{e}");
    }

    #[test]
    fn 不是_wav_的_不猜() {
        let e = Wav::parse(b"fLaC\0\0\0\0whatever").unwrap_err();
        assert!(e.contains("RIFF/WAVE") && e.contains("ffmpeg"), "{e}");
        assert!(Wav::parse(&[]).is_err());
        // 块长撒谎（截断）不许 panic
        let mut bad = wav(1, 16, 2, 48000, &[1, 2, 3, 4]);
        let n = bad.len();
        bad[n - 8..n - 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let _ = Wav::parse(&bad);
    }

    #[test]
    fn 循环回到开头_内容接上而输出轴不断() {
        let mut body = Vec::new();
        for i in 0..4i16 {
            body.extend_from_slice(&(i * 1000).to_le_bytes());
            body.extend_from_slice(&(-i * 1000).to_le_bytes());
        }
        let w = Wav::parse(&wav(1, 16, 2, 8000, &body)).unwrap();
        assert_eq!(w.frames, 4);

        let mut p = WavSource::new(&manifest());
        p.wav = Some(w);
        p.fs_out = 8000.0;
        p.gain = 1.0;
        p.cursor = 3; // 从最后一拍开始，跨过循环点
        let mut buf = vec![0u8; 1024];
        let n = p.fill(&mut buf, 3);
        assert_eq!(n, 3 * CHANNELS * 4);
        let val = |k: usize| f32::from_le_bytes(buf[k * 4..k * 4 + 4].try_into().unwrap());
        // 第 3 拍、然后绕回第 0、第 1 拍
        assert!((val(0) - 3000.0 / 32768.0).abs() < 1e-6);
        assert_eq!(val(2), 0.0);
        assert!((val(4) - 1000.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn 增益削顶而不是绕回() {
        let mut body = Vec::new();
        body.extend_from_slice(&20000i16.to_le_bytes());
        body.extend_from_slice(&(-20000i16).to_le_bytes());
        let w = Wav::parse(&wav(1, 16, 2, 8000, &body)).unwrap();
        let mut p = WavSource::new(&manifest());
        p.wav = Some(w);
        p.gain = 4.0;
        let mut buf = vec![0u8; 64];
        p.fill(&mut buf, 1);
        let val = |k: usize| f32::from_le_bytes(buf[k * 4..k * 4 + 4].try_into().unwrap());
        assert_eq!(val(0), 1.0, "该削到 +1，不该绕回负数");
        assert_eq!(val(1), -1.0);
    }

    fn manifest() -> PluginManifest {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/manifest.toml"))
            .expect("manifest.toml");
        toml::from_str(&text).expect("manifest.toml 解析")
    }

    #[test]
    fn manifest_的形状_两列_不声明率() {
        let m = manifest();
        let p = &m.ports[0];
        assert_eq!(p.columns.len(), CHANNELS);
        assert_eq!(p.columns[0].id, "l");
        assert_eq!(p.columns[1].id, "r");
        assert!(
            p.declared_rate_hz.is_none(),
            "文件的率是运行期的，不能声明——每帧带 sfrate01"
        );
        assert!(p.validate().is_ok(), "{:?}", p.validate());
    }
}
