//! sig-gen —— 示波器验收与压测用的测试源。
//!
//! 正弦 / 方波 / 三角 / 锯齿 / 脉冲 / 噪声，fs 到 100 MS/s，1..16 通道，f32 或 i16
//! （两个口，`dtype` 参数选哪个在发——口的 dtype 是静态声明，不能运行时改）。
//!
//! 出帧节奏按墙钟（同 sine-generator）：每个 tick 只发到"此刻该存在"的样本数，
//! `sample_index` / `t0_ns` 是真时间，真时钟事件（触发）才对得上。改 fs = 新采样
//! 轴：重新锚定并在下一帧带不连续标记；每一帧带 `sfrate01` 注解说实际率（fs 是
//! 参数，不能声明 `declared_rate_hz`）。
//!
//! 100 MS/s 一核要跑得动：相位是 u64 定点（一圈 = 2^64），每样本一次加法；正弦查
//! 4096 点表加线性插值，其余波形直接由相位算，噪声 xorshift64*。样本直接写进输出
//! 缓冲（对齐时按 f32/i16 切片写，不对齐回落到逐字节）。
//!
//! Contract lives in `manifest.toml` next to the dylib.

use sigflow_plugin_sdk::{
    encode_rate_annotation, mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest,
    ProcessOutcome,
};
use sigflow_types::frame::RATE_ANNOTATION_SCHEMA;

const MAX_CHANNELS: usize = 16;
const LUT_BITS: u32 = 12;
const LUT_SIZE: usize = 1 << LUT_BITS;
/// 帧尾要留给 sfrate01 注解的字节（8 字节 schema + JSON 体）。
const ANNOTATION_RESERVE: usize = 64;
/// 口在 manifest 里的顺序：outputs[0] = f32，outputs[1] = i16。
const PORT_F32: usize = 0;
const PORT_I16: usize = 1;

/// 伏 → i16 码：削顶到 ±1 V，四舍五入（半值远离零）；不走 libm 的 round。
#[inline(always)]
fn to_i16(v: f64) -> i16 {
    let x = v.clamp(-1.0, 1.0) * 32767.0;
    (x + if x >= 0.0 { 0.5 } else { -0.5 }) as i16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wave {
    Sine,
    Square,
    Triangle,
    Ramp,
    Pulse,
    Noise,
}

impl Wave {
    fn parse(s: &str) -> Option<Wave> {
        match s.trim() {
            "sine" => Some(Wave::Sine),
            "square" => Some(Wave::Square),
            "triangle" => Some(Wave::Triangle),
            "ramp" | "saw" | "sawtooth" => Some(Wave::Ramp),
            "pulse" => Some(Wave::Pulse),
            "noise" => Some(Wave::Noise),
            _ => None,
        }
    }
}

/// 解析逗号分隔的波形表；任何一个名字不认识就整条拒（返回 None），不悄悄换成
/// 别的波形。
fn parse_waveforms(s: &str) -> Option<Vec<Wave>> {
    let v: Option<Vec<Wave>> = s
        .split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .map(Wave::parse)
        .collect();
    v.filter(|v| !v.is_empty())
}

/// 一个口的流状态。两个口各一份——波形参数共用，序号轴与相位各走各的，这样
/// `dtype = "both"` 时两个口可以有不同的率。
///
/// **两个口在绝对时间上是同一根信号**：`start()` 时两条流锚在同一时刻、同一相位，
/// 相位每拍推进 `freq/fs`，所以流 st 的样本 n 的相位 = 起始相位 + freq × (t − 锚点)
/// ——与 fs 无关。示波器把两个口重采样到同一根时钟上之后，同一拍上两路该读到同一
/// 个值：多源验收就拿这个当已知真值。
struct Stream {
    fs_hz: f64,
    seq: u64,
    /// 下一个要发的样本的绝对序号。
    index: u64,
    /// 采样轴锚点：`t(index) = anchor_t0_ns + (index − anchor_index) / fs`。改 fs 时重锚。
    anchor_index: u64,
    anchor_t0_ns: i64,
    /// 每通道相位（一圈 = 2^64）。
    phase: [u64; MAX_CHANNELS],
    /// inject_gap：下一帧前跳过这么多拍。
    pending_gap: u64,
    /// 下一帧带不连续标记（gap 0 = 新采样轴 / 布局变了，不是丢样本）。
    pending_disc: bool,
    pending_glitch: bool,
    /// 缓存的 sfrate01 注解体（fs 变了重算）。
    rate_annotation: Vec<u8>,
}

impl Stream {
    fn new(fs_hz: f64) -> Stream {
        Stream {
            fs_hz,
            seq: 0,
            index: 0,
            anchor_index: 0,
            anchor_t0_ns: 0,
            phase: [0; MAX_CHANNELS],
            pending_gap: 0,
            pending_disc: false,
            pending_glitch: false,
            rate_annotation: encode_rate_annotation(fs_hz),
        }
    }
}

/// 哪几个口在发。口的 dtype 是静态声明，不能运行时改，所以"发什么类型"只能是
/// "发哪个口"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Emit {
    F32,
    I16,
    /// 两个口同时发，各自一个率（f32 走 `fs_hz`、i16 走 `fs2_hz`）——多源示波器
    /// 的验收源：一个节点两个不同率的口进同一台示波器。
    Both,
}

impl Emit {
    /// 这个模式下在发的口（manifest 顺序：0 = f32、1 = i16）。
    fn ports(self) -> &'static [usize] {
        match self {
            Emit::F32 => &[PORT_F32],
            Emit::I16 => &[PORT_I16],
            Emit::Both => &[PORT_F32, PORT_I16],
        }
    }
}

pub struct SigGen {
    // --- 参数 ---
    fs_hz: f64,
    /// i16 口的率；0 = 跟 `fs_hz`。只有 `dtype = "both"` 时才有独立的意义。
    fs2_hz: f64,
    channels: usize,
    emit: Emit,
    waves: Vec<Wave>,
    freq_hz: f64,
    amplitude_v: f64,
    dc_offset_v: f64,
    duty: f64,
    phase_step_deg: f64,
    noise_v: f64,
    gap_samples: u64,
    glitch_v: f64,

    // --- 流状态（两个口各一份）---
    streams: [Stream; 2],
    rng: u64,
    lut: Box<[f32; LUT_SIZE + 1]>,
}

impl SigGen {
    fn phase_inc(&self, st: usize) -> u64 {
        // freq/fs 圈每样本 × 2^64；freq ≥ fs 时混叠是使用者的事，这里只做取模。
        let turns = (self.freq_hz / self.streams[st].fs_hz).rem_euclid(1.0);
        (turns * 18_446_744_073_709_551_616.0) as u64
    }

    /// 参数说的第 `st` 个口的率：i16 口的 `fs2_hz` 给了就用它，没给就跟 `fs_hz`。
    fn target_fs(&self, st: usize) -> f64 {
        if st == PORT_I16 && self.fs2_hz > 0.0 {
            self.fs2_hz
        } else {
            self.fs_hz
        }
    }

    fn channel_offset(&self, ch: usize) -> u64 {
        let turns = (self.phase_step_deg * ch as f64 / 360.0).rem_euclid(1.0);
        (turns * 18_446_744_073_709_551_616.0) as u64
    }

    fn reset_phases(&mut self) {
        for ch in 0..MAX_CHANNELS {
            let p = self.channel_offset(ch);
            for st in &mut self.streams {
                st.phase[ch] = p;
            }
        }
    }

    fn wave_of(&self, ch: usize) -> Wave {
        self.waves[ch % self.waves.len()]
    }

    #[inline(always)]
    fn next_noise(rng: &mut u64) -> f64 {
        // xorshift64*：够快、够白，测试源不需要更好的。
        let mut x = *rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *rng = x;
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        // 高 53 位 → [0, 1) → [−1, 1)
        ((r >> 11) as f64) * (2.0 / 9_007_199_254_740_992.0) - 1.0
    }

    /// 生成 `scans` 拍到 `out`（口 `st` 的 dtype），推进那条流的相位。返回写入字节。
    ///
    /// 按通道各跑一个紧循环（stride 写）：波形分支在循环外定死，正弦查表 /
    /// 方波比较 / 三角与锯齿的乘加都能被编译器单独优化；按样本切换波形的写法
    /// 每个样本一次分支预测失败，100 MS/s × 4 通道就跑不动了。
    fn fill(&mut self, st: usize, out: &mut [u8], scans: usize) -> usize {
        let ch = self.channels;
        let inc = self.phase_inc(st);
        let duty_p = if self.duty >= 1.0 {
            u64::MAX
        } else {
            (self.duty.max(0.0) * 18_446_744_073_709_551_616.0) as u64
        };
        let amp = self.amplitude_v;
        let dc = self.dc_offset_v;
        let noise = self.noise_v;
        let glitch = self.streams[st].pending_glitch.then_some(self.glitch_v);
        self.streams[st].pending_glitch = false;
        let n_vals = scans * ch;

        let bytes = if st == PORT_I16 {
            let nbytes = n_vals * 2;
            let buf = &mut out[..nbytes];
            // SAFETY: i16 has no invalid bit patterns; align_to_mut keeps the
            // slice bounds. An unaligned buffer falls back to a scratch pass.
            let (pre, mid, _) = unsafe { buf.align_to_mut::<i16>() };
            if pre.is_empty() && mid.len() == n_vals {
                for c in 0..ch {
                    let wave = self.wave_of(c);
                    let p0 = self.streams[st].phase[c];
                    self.streams[st].phase[c] = self.run_channel(
                        wave,
                        p0,
                        inc,
                        duty_p,
                        dc,
                        amp,
                        noise,
                        scans,
                        |s, v| mid[s * ch + c] = to_i16(v),
                    );
                }
                if let Some(g) = glitch {
                    mid[0] = to_i16(g);
                }
            } else {
                let mut tmp = vec![0i16; n_vals];
                for c in 0..ch {
                    let wave = self.wave_of(c);
                    let p0 = self.streams[st].phase[c];
                    self.streams[st].phase[c] = self.run_channel(
                        wave,
                        p0,
                        inc,
                        duty_p,
                        dc,
                        amp,
                        noise,
                        scans,
                        |s, v| tmp[s * ch + c] = to_i16(v),
                    );
                }
                if let Some(g) = glitch {
                    tmp[0] = to_i16(g);
                }
                for (i, v) in tmp.iter().enumerate() {
                    buf[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
                }
            }
            nbytes
        } else {
            let nbytes = n_vals * 4;
            let buf = &mut out[..nbytes];
            // SAFETY: as above for f32.
            let (pre, mid, _) = unsafe { buf.align_to_mut::<f32>() };
            if pre.is_empty() && mid.len() == n_vals {
                for c in 0..ch {
                    let wave = self.wave_of(c);
                    let p0 = self.streams[st].phase[c];
                    self.streams[st].phase[c] = self.run_channel(
                        wave,
                        p0,
                        inc,
                        duty_p,
                        dc,
                        amp,
                        noise,
                        scans,
                        |s, v| mid[s * ch + c] = v as f32,
                    );
                }
                if let Some(g) = glitch {
                    mid[0] = g as f32;
                }
            } else {
                let mut tmp = vec![0f32; n_vals];
                for c in 0..ch {
                    let wave = self.wave_of(c);
                    let p0 = self.streams[st].phase[c];
                    self.streams[st].phase[c] = self.run_channel(
                        wave,
                        p0,
                        inc,
                        duty_p,
                        dc,
                        amp,
                        noise,
                        scans,
                        |s, v| tmp[s * ch + c] = v as f32,
                    );
                }
                if let Some(g) = glitch {
                    tmp[0] = g as f32;
                }
                for (i, v) in tmp.iter().enumerate() {
                    buf[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            nbytes
        };
        bytes
    }

    /// 一个通道的紧循环：从相位 `p0` 起走 `scans` 拍，每拍把值交给 `put(s, v)`；
    /// 返回走完后的相位。波形分支在循环外。
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn run_channel(
        &mut self,
        wave: Wave,
        p0: u64,
        inc: u64,
        duty_p: u64,
        dc: f64,
        amp: f64,
        noise: f64,
        scans: usize,
        mut put: impl FnMut(usize, f64),
    ) -> u64 {
        let mut p = p0;
        let mut rng = self.rng;
        macro_rules! run {
            ($shape:expr) => {{
                if noise > 0.0 {
                    for s in 0..scans {
                        let v = dc + amp * $shape(p) + noise * Self::next_noise(&mut rng);
                        p = p.wrapping_add(inc);
                        put(s, v);
                    }
                } else {
                    for s in 0..scans {
                        let v = dc + amp * $shape(p);
                        p = p.wrapping_add(inc);
                        put(s, v);
                    }
                }
            }};
        }
        match wave {
            Wave::Sine => {
                let lut = &*self.lut;
                run!(|p: u64| {
                    let idx = (p >> (64 - LUT_BITS)) as usize;
                    let frac = ((p >> (64 - LUT_BITS - 16)) & 0xFFFF) as f32 * (1.0 / 65536.0);
                    let a = lut[idx];
                    let b = lut[idx + 1];
                    (a + (b - a) * frac) as f64
                })
            }
            Wave::Square => run!(|p: u64| if p < duty_p { 1.0 } else { -1.0 }),
            Wave::Pulse => run!(|p: u64| if p < duty_p { 1.0 } else { 0.0 }),
            Wave::Triangle => run!(|p: u64| {
                let x = ((p >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0);
                1.0 - 4.0 * (x - 0.5).abs()
            }),
            Wave::Ramp => run!(|p: u64| {
                let x = ((p >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0);
                2.0 * x - 1.0
            }),
            Wave::Noise => {
                // 噪声波形本身就是随机数；noise_v 再叠一层也无妨。
                for s in 0..scans {
                    let mut v = dc + amp * Self::next_noise(&mut rng);
                    if noise > 0.0 {
                        v += noise * Self::next_noise(&mut rng);
                    }
                    p = p.wrapping_add(inc);
                    put(s, v);
                }
            }
        }
        self.rng = rng;
        p
    }

    /// 改某条流的 fs：新采样轴——以"现在"重新锚定，下一帧带不连续标记（gap 0）。
    fn set_fs_at(&mut self, st: usize, fs: f64, now_ns: i64) {
        let fs = fs.clamp(1e3, 1e8);
        let s = &mut self.streams[st];
        if fs == s.fs_hz {
            return;
        }
        s.anchor_index = s.index;
        s.anchor_t0_ns = now_ns;
        s.fs_hz = fs;
        s.pending_disc = true;
        s.rate_annotation = encode_rate_annotation(fs);
    }

    /// 参数变了之后把两条流的率对齐到参数说的值。
    fn retune_streams(&mut self, now_ns: i64) {
        for st in 0..self.streams.len() {
            let fs = self.target_fs(st);
            self.set_fs_at(st, fs, now_ns);
        }
    }

    /// 一个 tick：每个在发的口都发到 `now_ns` 该存在的样本数（受输出缓冲容量限制）。
    fn generate(&mut self, now_ns: i64, outputs: &mut [FrameOut]) -> ProcessOutcome {
        for &port in self.emit.ports() {
            self.generate_port(port, now_ns, outputs);
        }
        ProcessOutcome::Ok
    }

    /// 一个口的一个 tick。
    fn generate_port(&mut self, port: usize, now_ns: i64, outputs: &mut [FrameOut]) {
        let Some(out) = outputs.get_mut(port) else {
            return;
        };
        let elem = if port == PORT_I16 { 2 } else { 4 };
        let bytes_per_scan = self.channels * elem;
        let cap_scans = out.capacity().saturating_sub(ANNOTATION_RESERVE) / bytes_per_scan;

        let mut gap = 0u64;
        if self.streams[port].pending_gap > 0 {
            // 像真丢样本：序号跳过去，时间照走，帧头说明缺了几拍。
            gap = self.streams[port].pending_gap;
            self.streams[port].index += gap;
            self.streams[port].pending_gap = 0;
        }

        let st = &self.streams[port];
        let elapsed = (now_ns - st.anchor_t0_ns).max(0) as f64 / 1e9;
        let target = st.anchor_index + (elapsed * st.fs_hz) as u64;
        let due = target.saturating_sub(st.index) as usize;
        let scans = due.min(cap_scans);
        if scans == 0 {
            // 这个 tick 没有到期的样本——不发空帧（壳体会跳过），序号不动。
            return;
        }

        let (index, seq, anchor_index, anchor_t0_ns, fs_hz, disc) =
            (st.index, st.seq, st.anchor_index, st.anchor_t0_ns, st.fs_hz, st.pending_disc);
        let written = {
            let buf = out.buffer_mut();
            self.fill(port, buf, scans)
        };
        out.set_written(written);
        out.header.seq = seq;
        out.header.sample_index = index;
        out.header.n_samples = scans as u32;
        out.header.t0_ns = anchor_t0_ns + (((index - anchor_index) as f64 / fs_hz) * 1e9) as i64;
        if gap > 0 || disc {
            out.header.set_discontinuity(gap);
            self.streams[port].pending_disc = false;
        }
        if !out.set_annotation(RATE_ANNOTATION_SCHEMA, &self.streams[port].rate_annotation) {
            // 只有容量算错才会到这里；注解是契约的一部分，宁可这一帧不发。
            out.set_written(0);
            return;
        }

        self.streams[port].seq += 1;
        self.streams[port].index += scans as u64;
    }
}

impl Plugin for SigGen {
    fn new(_manifest: &PluginManifest) -> Self {
        let mut lut = Box::new([0f32; LUT_SIZE + 1]);
        for (i, v) in lut.iter_mut().enumerate() {
            *v = (2.0 * std::f64::consts::PI * i as f64 / LUT_SIZE as f64).sin() as f32;
        }
        let mut g = SigGen {
            fs_hz: 1_000_000.0,
            fs2_hz: 0.0,
            channels: 1,
            emit: Emit::F32,
            waves: vec![Wave::Sine],
            freq_hz: 1000.0,
            amplitude_v: 0.5,
            dc_offset_v: 0.0,
            duty: 0.5,
            phase_step_deg: 90.0,
            noise_v: 0.0,
            gap_samples: 1000,
            glitch_v: 1.0,
            streams: [Stream::new(1_000_000.0), Stream::new(1_000_000.0)],
            rng: 0x9E37_79B9_7F4A_7C15,
            lut,
        };
        g.reset_phases();
        g
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("dtype", ParamValue::String(s)) | ("dtype", ParamValue::Enum(s)) => match s.trim() {
                "f32" => self.emit = Emit::F32,
                "i16" => self.emit = Emit::I16,
                "both" => self.emit = Emit::Both,
                // 不认识的名字不改——静默换成别的口就是发着发着换了一路
                _ => {}
            },
            ("fs_hz", ParamValue::F64(v)) => {
                self.fs_hz = v.clamp(1e3, 1e8);
                self.retune_streams(mono_ns());
            }
            ("fs2_hz", ParamValue::F64(v)) => {
                // 0 = 跟 fs_hz；别的值只作用在 i16 口上
                self.fs2_hz = if *v <= 0.0 { 0.0 } else { v.clamp(1e3, 1e8) };
                self.retune_streams(mono_ns());
            }
            ("channels", ParamValue::U32(v)) => {
                let n = (*v as usize).clamp(1, MAX_CHANNELS);
                if n != self.channels {
                    self.channels = n;
                    // 帧布局变了：下一帧带断，让消费方重展布局。
                    for st in &mut self.streams {
                        st.pending_disc = true;
                    }
                }
            }
            ("waveforms", ParamValue::String(s)) | ("waveforms", ParamValue::Enum(s)) => {
                if let Some(w) = parse_waveforms(s) {
                    self.waves = w;
                }
            }
            ("freq_hz", ParamValue::F64(v)) => self.freq_hz = v.clamp(0.001, 5e7),
            ("amplitude_v", ParamValue::F64(v)) => self.amplitude_v = v.clamp(0.0, 1.0),
            ("dc_offset_v", ParamValue::F64(v)) => self.dc_offset_v = v.clamp(-1.0, 1.0),
            ("duty", ParamValue::F64(v)) => self.duty = v.clamp(0.0, 1.0),
            ("phase_step_deg", ParamValue::F64(v)) => {
                self.phase_step_deg = v.clamp(0.0, 360.0);
            }
            ("noise_v", ParamValue::F64(v)) => self.noise_v = v.clamp(0.0, 1.0),
            ("gap_samples", ParamValue::U32(v)) => self.gap_samples = u64::from((*v).max(1)),
            ("glitch_v", ParamValue::F64(v)) => self.glitch_v = v.clamp(-1.0, 1.0),
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        // 两条流锚在同一时刻、同一相位：绝对时间上它们是同一根信号（见 [`Stream`]）
        let now = mono_ns();
        for st in 0..self.streams.len() {
            let fs = self.target_fs(st);
            self.streams[st] = Stream::new(fs.clamp(1e3, 1e8));
            self.streams[st].anchor_t0_ns = now;
        }
        self.reset_phases();
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        self.generate(mono_ns(), outputs)
    }

    fn invoke_action(&mut self, id: &str) {
        match id {
            "inject_gap" => {
                for st in &mut self.streams {
                    st.pending_gap = self.gap_samples;
                }
            }
            "inject_glitch" => {
                for st in &mut self.streams {
                    st.pending_glitch = true;
                }
            }
            "reset_phase" => self.reset_phases(),
            _ => {}
        }
    }
}

sigflow_plugin_sdk::export_plugin!(SigGen);

#[cfg(test)]
mod tests {
    use super::*;
    use sigflow_types::frame::{decode_rate_annotation, FrameHeader, FLAG_DISCONTINUITY};

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest.toml should parse")
    }

    fn gen() -> SigGen {
        let mut g = SigGen::new(&manifest());
        for st in &mut g.streams {
            st.anchor_t0_ns = 0;
        }
        g
    }

    /// 跑一个 tick（时刻 `now_ns`），返回 (header, 样本字节, 注解字节)。
    fn tick(g: &mut SigGen, now_ns: i64, cap: usize) -> Option<(FrameHeader, Vec<u8>, Vec<u8>)> {
        let mut b0 = vec![0u8; cap];
        let mut b1 = vec![0u8; cap];
        let mut outs = [FrameOut::new(&mut b0), FrameOut::new(&mut b1)];
        g.generate(now_ns, &mut outs);
        let port = if g.emit == Emit::I16 { PORT_I16 } else { PORT_F32 };
        let other = 1 - port;
        if g.emit != Emit::Both {
            assert_eq!(outs[other].written(), 0, "另一个口不该发");
        }
        let o = &outs[port];
        if o.written() == 0 {
            return None;
        }
        let h = o.header;
        let w = o.written();
        let buf = if port == 0 { &b0 } else { &b1 };
        let (samples, ann) = h.split_payload(&buf[..w]);
        Some((h, samples.to_vec(), ann.unwrap_or(&[]).to_vec()))
    }

    fn f32s(b: &[u8]) -> Vec<f32> {
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    fn i16s(b: &[u8]) -> Vec<i16> {
        b.chunks_exact(2)
            .map(|c| i16::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    /// `dtype = "both"`：两个口同时发、各自一个率，而且在**绝对时间**上是同一根
    /// 信号——多源示波器验收拿这个当已知真值（同一拍上两路该读到同一个值）。
    #[test]
    fn both_两个口同时发_各自一个率_绝对时间上同一根信号() {
        let mut g = gen();
        g.set_param("dtype", &ParamValue::String("both".into()));
        g.set_param("fs_hz", &ParamValue::F64(1_000_000.0));
        g.set_param("fs2_hz", &ParamValue::F64(250_000.0));
        g.set_param("freq_hz", &ParamValue::F64(1_000.0));
        g.set_param("amplitude_v", &ParamValue::F64(0.9));
        // set_param 会以 mono_ns() 重锚；测试要可预期的锚点，锚回 0（start() 在真机上
        // 做的是同一件事：两条流同一时刻、同一相位）
        for st in &mut g.streams {
            st.anchor_t0_ns = 0;
            st.anchor_index = 0;
            st.index = 0;
            st.pending_disc = false;
        }
        g.reset_phases();

        let mut b0 = vec![0u8; 1 << 20];
        let mut b1 = vec![0u8; 1 << 20];
        let mut outs = [FrameOut::new(&mut b0), FrameOut::new(&mut b1)];
        g.generate(10_000_000, &mut outs);
        let (w0, w1) = (outs[PORT_F32].written(), outs[PORT_I16].written());
        assert!(w0 > 0 && w1 > 0, "两个口都要发（{w0}, {w1}）");
        let (h0, h1) = (outs[PORT_F32].header, outs[PORT_I16].header);
        assert_eq!(h0.n_samples, 10_000, "f32 口 1 MS/s × 10 ms");
        assert_eq!(h1.n_samples, 2_500, "i16 口 250 kS/s × 10 ms");
        assert_eq!((h0.sample_index, h1.sample_index), (0, 0), "两条各走各的序号轴");
        assert_eq!((h0.t0_ns, h1.t0_ns), (0, 0));

        let (s0, a0) = h0.split_payload(&b0[..w0]);
        let (s1, a1) = h1.split_payload(&b1[..w1]);
        assert_eq!(decode_rate_annotation(a0.unwrap()), Some(1_000_000.0), "每个口的率注解说自己的率");
        assert_eq!(decode_rate_annotation(a1.unwrap()), Some(250_000.0));

        // 绝对时间对齐：f32 的第 4k 拍与 i16 的第 k 拍是同一时刻
        let (v0, v1) = (f32s(s0), i16s(s1));
        let mut worst = 0.0f64;
        for k in 0..v1.len() {
            let a = v0[4 * k] as f64;
            let b = v1[k] as f64 / 32768.0;
            worst = worst.max((a - b).abs());
        }
        assert!(worst < 2e-3, "两个口在同一时刻不是同一个值：差 {worst}");
        assert!(v0.iter().any(|x| x.abs() > 0.5), "别是一条零线");
    }

    #[test]
    fn manifest_shape() {
        let m = manifest();
        assert_eq!(m.name, "sigflow.core.sig_gen");
        assert_eq!(m.ports.len(), 2);
        assert_eq!(m.ports[PORT_F32].id, "f32");
        assert_eq!(m.ports[PORT_I16].id, "i16");
        for p in &m.ports {
            p.validate().unwrap();
            assert!(p.declared_rate_hz.is_none(), "fs 是参数，不能声明率");
            assert!(p.negotiable.is_none(), "协商是二期");
            assert_eq!(p.columns.len(), 1);
            assert!(p.column_groups.as_ref().unwrap().repeat_by.is_some());
        }
        assert_eq!(
            m.ports[PORT_I16].semantic_type.dtype().unwrap().as_str(),
            "i16"
        );
        assert!((m.ports[PORT_I16].columns[0].scale.unwrap() - 1.0 / 32768.0).abs() < 1e-15);
        assert_eq!(m.actions.len(), 3);
    }

    #[test]
    fn 按墙钟出样本_序号与t0是真时间_每帧带率注解() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 1e6, 0);
        // 1 ms 后该有 1000 拍
        let (h, s, ann) = tick(&mut g, 1_000_000, 1 << 16).unwrap();
        assert_eq!(h.n_samples, 1000);
        assert_eq!(h.sample_index, 0);
        assert_eq!(h.t0_ns, 0);
        assert_eq!(f32s(&s).len(), 1000);
        assert_eq!(
            decode_rate_annotation(&ann[8..]),
            Some(1e6),
            "注解体在 8 字节 schema 之后"
        );
        // 没到期：不发
        assert!(tick(&mut g, 1_000_000, 1 << 16).is_none());
        // 再 0.5 ms
        let (h, _, _) = tick(&mut g, 1_500_000, 1 << 16).unwrap();
        assert_eq!(h.sample_index, 1000);
        assert_eq!(h.n_samples, 500);
        assert_eq!(h.t0_ns, 1_000_000);
        assert_eq!(h.seq, 1);
    }

    #[test]
    fn 正弦值对_1khz_at_48k() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 48_000.0, 0);
        g.set_param("freq_hz", &ParamValue::F64(1000.0));
        g.set_param("amplitude_v", &ParamValue::F64(1.0));
        let (_, s, _) = tick(&mut g, 1_000_000_000, 1 << 16).unwrap();
        let v = f32s(&s);
        assert!(v.len() >= 48);
        assert!(v[0].abs() < 1e-3, "sin(0) = 0, got {}", v[0]);
        assert!(
            (v[12] - 1.0).abs() < 2e-3,
            "四分之一周期 ≈ 1, got {}",
            v[12]
        );
        assert!((v[24]).abs() < 2e-3, "半周期 ≈ 0, got {}", v[24]);
        assert!(
            (v[36] + 1.0).abs() < 2e-3,
            "四分之三周期 ≈ −1, got {}",
            v[36]
        );
        for (i, x) in v.iter().enumerate().take(4800) {
            let want = (2.0 * std::f64::consts::PI * i as f64 / 48.0).sin();
            assert!((*x as f64 - want).abs() < 2e-3, "sample {i}: {x} vs {want}");
        }
    }

    #[test]
    fn i16_口按满量程编码_削顶() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 48_000.0, 0);
        g.set_param("dtype", &ParamValue::String("i16".into()));
        g.set_param("waveforms", &ParamValue::String("square".into()));
        g.set_param("amplitude_v", &ParamValue::F64(0.5));
        let (_, s, _) = tick(&mut g, 1_000_000_000, 1 << 16).unwrap();
        let v = i16s(&s);
        assert_eq!(v[0], 16384, "0.5 V × 32767 四舍五入");
        g.set_param("dc_offset_v", &ParamValue::F64(0.9));
        let (_, s, _) = tick(&mut g, 2_000_000_000, 1 << 16).unwrap();
        let v = i16s(&s);
        assert!(v.iter().any(|&x| x == 32767), "超过 1 V 削顶到 32767");
    }

    #[test]
    fn 多通道交织_相位按步进错开() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 48_000.0, 0);
        g.set_param("channels", &ParamValue::U32(2));
        g.set_param("freq_hz", &ParamValue::F64(1000.0));
        g.set_param("amplitude_v", &ParamValue::F64(1.0));
        g.set_param("phase_step_deg", &ParamValue::F64(90.0));
        g.reset_phases();
        g.streams[PORT_F32].pending_disc = false; // 改通道数的那次断这里不看，只看相位
        let (h, s, _) = tick(&mut g, 100_000_000, 1 << 16).unwrap();
        let v = f32s(&s);
        assert_eq!(v.len(), h.n_samples as usize * 2);
        assert!(v[0].abs() < 1e-3, "ch0 sin(0)");
        assert!(
            (v[1] - 1.0).abs() < 2e-3,
            "ch1 领先 90° = cos(0) = 1, got {}",
            v[1]
        );
        // 改通道数：下一帧带断（布局变了）
        assert_eq!(h.flags & FLAG_DISCONTINUITY, 0);
        g.set_param("channels", &ParamValue::U32(3));
        let (h, s, _) = tick(&mut g, 200_000_000, 1 << 16).unwrap();
        assert_ne!(h.flags & FLAG_DISCONTINUITY, 0);
        assert_eq!(h.gap_samples, 0, "不是丢样本，是布局变了");
        assert_eq!(f32s(&s).len(), h.n_samples as usize * 3);
    }

    #[test]
    fn 波形形状() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 1000.0, 0);
        g.set_param("freq_hz", &ParamValue::F64(10.0)); // 100 拍一周期
        g.set_param("amplitude_v", &ParamValue::F64(1.0));
        g.set_param("duty", &ParamValue::F64(0.3));
        for (name, check) in [
            (
                "square",
                (|v: &[f32]| {
                    assert_eq!(v.iter().take(30).filter(|x| **x == 1.0).count(), 30);
                    assert_eq!(
                        v.iter().skip(30).take(70).filter(|x| **x == -1.0).count(),
                        70
                    );
                }) as fn(&[f32]),
            ),
            ("pulse", |v| {
                assert_eq!(v.iter().take(30).filter(|x| **x == 1.0).count(), 30);
                assert_eq!(
                    v.iter().skip(30).take(70).filter(|x| **x == 0.0).count(),
                    70
                );
            }),
            ("triangle", |v| {
                assert!((v[0] + 1.0).abs() < 1e-3);
                assert!((v[50] - 1.0).abs() < 0.05);
                assert!(v[25] > v[10] && v[10] > v[0], "上升段单调");
                assert!(v[75] < v[60], "下降段单调");
            }),
            ("ramp", |v| {
                assert!((v[0] + 1.0).abs() < 1e-3);
                assert!(v[99] > 0.95);
                assert!(v[50].abs() < 0.03);
                assert!(v[1] > v[0] && v[98] > v[97]);
            }),
            ("noise", |v| {
                assert!(v.iter().all(|x| (-1.0..=1.0).contains(x)));
                let mean: f32 = v.iter().sum::<f32>() / v.len() as f32;
                assert!(mean.abs() < 0.1, "均值 ≈ 0, got {mean}");
                assert!(v.windows(2).any(|w| w[0] != w[1]));
            }),
        ] {
            g.set_param("waveforms", &ParamValue::String(name.into()));
            g.reset_phases();
            g.streams[PORT_F32].index = 0;
            g.streams[PORT_F32].anchor_index = 0;
            let (_, s, _) = tick(&mut g, 1_000_000_000, 1 << 16).unwrap();
            let v = f32s(&s);
            assert!(v.len() >= 1000, "{name}");
            check(&v);
        }
        // 不认识的波形名：整条拒，保持原样
        g.set_param("waveforms", &ParamValue::String("sine,bogus".into()));
        assert_eq!(g.waves, vec![Wave::Noise]);
        g.set_param("waveforms", &ParamValue::String("sine, square".into()));
        assert_eq!(g.waves, vec![Wave::Sine, Wave::Square]);
    }

    #[test]
    fn inject_gap_像真丢样本_序号跳_帧头带断与gap() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 1e6, 0);
        let (h, _, _) = tick(&mut g, 1_000_000, 1 << 16).unwrap();
        assert_eq!(h.n_samples, 1000);
        g.set_param("gap_samples", &ParamValue::U32(300));
        g.invoke_action("inject_gap");
        let (h, _, _) = tick(&mut g, 2_000_000, 1 << 16).unwrap();
        assert_ne!(h.flags & FLAG_DISCONTINUITY, 0);
        assert_eq!(h.gap_samples, 300);
        assert_eq!(h.sample_index, 1300, "跳过的序号不发");
        assert_eq!(
            h.n_samples, 700,
            "时间照走：到 2 ms 该有 2000 拍，跳了 300 剩 700"
        );
        assert_eq!(h.t0_ns, 1_300_000, "t0 按序号算，跳过的时间真的过去了");
        // 之后正常
        let (h, _, _) = tick(&mut g, 3_000_000, 1 << 16).unwrap();
        assert_eq!(h.flags & FLAG_DISCONTINUITY, 0);
        assert_eq!(h.sample_index, 2000);
    }

    #[test]
    fn 改fs是新采样轴_重锚_带断_注解变() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 1e6, 0);
        let _ = tick(&mut g, 1_000_000, 1 << 16).unwrap(); // index 1000
        g.set_fs_at(PORT_F32, 2e6, 1_000_000);
        let (h, _, ann) = tick(&mut g, 1_500_000, 1 << 16).unwrap();
        assert_ne!(h.flags & FLAG_DISCONTINUITY, 0);
        assert_eq!(h.gap_samples, 0);
        assert_eq!(h.sample_index, 1000, "序号不跳，轴换了");
        assert_eq!(h.n_samples, 1000, "0.5 ms @ 2 MHz");
        assert_eq!(h.t0_ns, 1_000_000, "新锚点");
        assert_eq!(decode_rate_annotation(&ann[8..]), Some(2e6));
        let (h, _, _) = tick(&mut g, 2_000_000, 1 << 16).unwrap();
        assert_eq!(h.t0_ns, 1_500_000);
        assert_eq!(h.flags & FLAG_DISCONTINUITY, 0);
    }

    #[test]
    fn inject_glitch_下一帧第一拍_ch0() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 1e6, 0);
        g.set_param("channels", &ParamValue::U32(2));
        g.set_param("amplitude_v", &ParamValue::F64(0.1));
        g.set_param("glitch_v", &ParamValue::F64(0.9));
        g.invoke_action("inject_glitch");
        let (_, s, _) = tick(&mut g, 1_000_000, 1 << 16).unwrap();
        let v = f32s(&s);
        assert_eq!(v[0], 0.9);
        assert!(v[1].abs() <= 0.1 && v[2].abs() <= 0.1, "只有 ch0 的第一拍");
        let (_, s, _) = tick(&mut g, 2_000_000, 1 << 16).unwrap();
        assert!(f32s(&s)[0].abs() <= 0.1, "只一帧");
    }

    #[test]
    fn 容量限制_追赶不越界_注解总在() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 1e8, 0);
        g.set_param("channels", &ParamValue::U32(4));
        // 1 ms @ 100 MS/s × 4 ch × 4 B = 1.6 MB 该到期；给 64 KiB 的缓冲只能装一部分
        let (h, s, ann) = tick(&mut g, 1_000_000, 1 << 16).unwrap();
        assert_eq!(h.n_samples as usize, ((1 << 16) - ANNOTATION_RESERVE) / 16);
        assert_eq!(s.len(), h.n_samples as usize * 16);
        assert!(!ann.is_empty());
        let (h2, _, _) = tick(&mut g, 1_000_000, 1 << 16).unwrap();
        assert_eq!(
            h2.sample_index, h.n_samples as u64,
            "同一时刻再来一次：接着追"
        );
    }

    #[test]
    fn 未对齐的缓冲照样写对() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 48_000.0, 0);
        g.set_param("waveforms", &ParamValue::String("square".into()));
        g.set_param("amplitude_v", &ParamValue::F64(1.0));
        let mut raw = vec![0u8; 4096 + 1];
        let mut other = [0u8; 8];
        let mut outs = [FrameOut::new(&mut raw[1..]), FrameOut::new(&mut other[..])];
        g.generate(1_000_000_000, &mut outs);
        let w = outs[0].written();
        assert!(w > 0);
        let h = outs[0].header;
        let (samples, _) = h.split_payload(&raw[1..1 + w]);
        assert!(f32s(samples).iter().take(10).all(|x| *x == 1.0));
    }

    /// 纯生成的吞吐（不含壳体的发布拷贝与示波器入环）；
    /// `cargo test --release -- --ignored throughput --nocapture`。
    fn bench(label: &str, i16: bool, ch: u32, waves: &str) -> f64 {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 1e8, 0);
        g.set_param(
            "dtype",
            &ParamValue::String(if i16 { "i16" } else { "f32" }.into()),
        );
        g.set_param("channels", &ParamValue::U32(ch));
        g.set_param("waveforms", &ParamValue::String(waves.into()));
        let cap = 2 << 20;
        let mut b0 = vec![0u8; cap];
        let mut b1 = vec![0u8; cap];
        let t = std::time::Instant::now();
        let mut total = 0u64;
        let mut now = 0i64;
        while total < 100_000_000 {
            now += 1_000_000;
            let mut outs = [FrameOut::new(&mut b0), FrameOut::new(&mut b1)];
            g.generate(now, &mut outs);
            total += u64::from(outs[if i16 { PORT_I16 } else { PORT_F32 }].header.n_samples);
        }
        let dt = t.elapsed().as_secs_f64();
        let ms_per_s = total as f64 / dt / 1e6;
        eprintln!(
            "{label}: {total} scans × {ch} ch in {dt:.3} s = {ms_per_s:.1} MS/s per channel, {:.0} M samples/s aggregate",
            ms_per_s * ch as f64
        );
        ms_per_s
    }

    #[test]
    #[ignore]
    fn throughput_f32_4ch_sine() {
        // 验收 3 的组合：4 ch f32 100 MS/s（正弦是最贵的波形：查表 + 插值）
        assert!(bench("f32 4ch sine", false, 4, "sine") >= 100.0);
    }

    #[test]
    #[ignore]
    fn throughput_i16_1ch_sine() {
        // 验收 1 的组合：1 ch i16 100 MS/s
        assert!(bench("i16 1ch sine", true, 1, "sine") >= 100.0);
    }

    #[test]
    #[ignore]
    fn throughput_100ms_per_s_i16_4ch() {
        let mut g = gen();
        g.set_fs_at(PORT_F32, 1e8, 0);
        g.set_param("dtype", &ParamValue::String("i16".into()));
        g.set_param("channels", &ParamValue::U32(4));
        g.set_param(
            "waveforms",
            &ParamValue::String("sine,square,triangle,noise".into()),
        );
        let cap = 2 << 20;
        let mut b0 = vec![0u8; cap];
        let mut b1 = vec![0u8; cap];
        let t = std::time::Instant::now();
        let mut total = 0u64;
        let mut now = 0i64;
        while total < 100_000_000 {
            now += 1_000_000;
            let mut outs = [FrameOut::new(&mut b0), FrameOut::new(&mut b1)];
            g.generate(now, &mut outs);
            total += u64::from(outs[PORT_I16].header.n_samples);
        }
        let dt = t.elapsed().as_secs_f64();
        eprintln!(
            "generated {total} scans × 4 ch i16 in {dt:.3} s = {:.1} MS/s",
            total as f64 / dt / 1e6
        );
        assert!(dt < 1.0, "1 s 的样本要在 1 s 内生成完（实际 {dt:.3} s）");
    }
}
