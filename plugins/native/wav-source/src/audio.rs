//! audio —— 把播出去的样本同时送到系统默认输出设备（cpal）。
//!
//! **声卡不当时钟。** 口的出帧节奏按墙钟（同 sig-gen），示波器画的是那条轴；声音
//! 只是跟着听。所以设备从一个环里取样本，环由 `process()` 每 tick 推进：环空了出
//! 静音（暂停 / 放完 / 引擎卡了一下），环太长就丢最旧的（声音追上画面，而不是越拖
//! 越远）。两边时钟有漂移（DAC 晶振 vs 墙钟，量级 100 ppm），所以取样前先攒一段
//! 缓冲（`PRIME_S`）：攒够才开始出声，欠载了就重新攒——代价是几十毫秒延迟，换来
//! 不会因为 DAC 略快而贴着零欠载、一直咔嗒。
//!
//! **率不一定对得上。** 设备开得成文件的率就原样走（多数 DAC 认 44.1/48/96/192k，
//! speed = 1 时一拍不动）；开不成就用设备缺省的率，在回调里线性插值重采样。`speed`
//! 改了也是同一条路：源率 = fs × speed，比值一改就跟着变——慢放音调随之降，像磁带，
//! 这是对的：耳朵听到的和眼睛看到的是同一条轨迹。
//!
//! 重采样器（[`Resampler`]）不碰 cpal，单测直接喂环。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

/// 攒够这么长才开始出声（欠载后也重新攒）。
const PRIME_S: f64 = 0.06;
/// 环里最多攒这么长；超过就丢旧的、只留 `KEEP_S`。
const CAP_S: f64 = 0.25;
const KEEP_S: f64 = 0.12;

/// 回调与生产方共享的一份。
struct Shared {
    /// 交织的 (l, r)，源率。
    ring: Mutex<VecDeque<f32>>,
    /// 每个设备拍要吃多少个源拍（f64 的位模式）。
    ratio: AtomicU64,
    /// 音量（f32 的位模式）。
    volume: AtomicU32,
    /// 攒够多少个源拍才开始出声。
    prime_frames: AtomicUsize,
    /// 生产方要求清掉环和插值状态（跳转 / 换文件）。
    flush: AtomicBool,
    /// 回调拿不到样本的拍数。暂停时也会涨，所以是观测不是告警。
    underruns: AtomicU64,
}

/// 线性插值重采样：上一拍、当前拍、相位。
pub struct Resampler {
    prev: [f32; 2],
    cur: [f32; 2],
    /// 输出拍落在 prev→cur 之间的位置；≥ 1 就该往前吃源拍。
    phase: f64,
    /// 攒够了没：没攒够只出静音。
    primed: bool,
}

impl Default for Resampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Resampler {
    pub fn new() -> Self {
        Resampler {
            prev: [0.0; 2],
            cur: [0.0; 2],
            phase: 1.0,
            primed: false,
        }
    }

    pub fn reset(&mut self) {
        *self = Resampler::new();
    }

    /// 从 `ring` 取源拍、按 `ratio`（源拍 / 设备拍）重采样填满 `out`（`channels` 路
    /// 交织：立体声填左右，单声道混成一路，更多路只填前两路其余置零）。
    /// 环里不足 `prime` 拍就整段静音不动环（攒缓冲）。返回欠了几拍。
    pub fn render<T: Sample + FromSample<f32>>(
        &mut self,
        ring: &mut VecDeque<f32>,
        out: &mut [T],
        channels: usize,
        ratio: f64,
        volume: f32,
        prime: usize,
    ) -> u64 {
        let channels = channels.max(1);
        if !self.primed {
            if ring.len() < prime * 2 {
                for s in out.iter_mut() {
                    *s = T::from_sample(0.0f32);
                }
                return 0;
            }
            self.primed = true;
        }
        let mut missed = 0u64;
        for frame in out.chunks_mut(channels) {
            while self.phase >= 1.0 {
                self.phase -= 1.0;
                self.prev = self.cur;
                if ring.len() >= 2 {
                    let l = ring.pop_front().unwrap_or(0.0);
                    let r = ring.pop_front().unwrap_or(0.0);
                    self.cur = [l, r];
                } else {
                    self.cur = [0.0, 0.0];
                    missed += 1;
                }
            }
            let t = self.phase as f32;
            let l = (self.prev[0] + (self.cur[0] - self.prev[0]) * t) * volume;
            let r = (self.prev[1] + (self.cur[1] - self.prev[1]) * t) * volume;
            if channels == 1 {
                frame[0] = T::from_sample((l + r) * 0.5);
            } else {
                for (i, s) in frame.iter_mut().enumerate() {
                    *s = T::from_sample(match i {
                        0 => l,
                        1 => r,
                        _ => 0.0,
                    });
                }
            }
            self.phase += ratio;
        }
        if missed > 0 {
            // 欠载：下次攒够再出，别贴着零一路咔嗒
            self.primed = false;
        }
        missed
    }
}

/// 一条打开的输出流。丢掉它就停。
pub struct AudioOut {
    _stream: cpal::Stream,
    shared: Arc<Shared>,
    /// 设备实际跑的率。
    pub device_rate_hz: f64,
    pub device_name: String,
}

impl AudioOut {
    /// 开系统默认输出设备。先试文件的率，开不成退到设备缺省的率（回调里重采样）。
    pub fn open(fs_hz: f64) -> Result<AudioOut, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "没有默认输出设备".to_string())?;
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "?".to_string());
        let want = fs_hz.round().clamp(1.0, u32::MAX as f64) as u32;
        // 优先：文件的率 + 双声道 + f32
        let score = |c: &cpal::SupportedStreamConfig| -> u8 {
            u8::from(c.channels() == 2) * 2 + u8::from(c.sample_format() == SampleFormat::F32)
        };
        let mut best: Option<cpal::SupportedStreamConfig> = None;
        if let Ok(ranges) = device.supported_output_configs() {
            for r in ranges {
                if r.channels() == 0 {
                    continue;
                }
                if let Some(c) = r.try_with_sample_rate(want) {
                    if best.as_ref().is_none_or(|b| score(&c) > score(b)) {
                        best = Some(c);
                    }
                }
            }
        }
        let cfg = match best {
            Some(c) => c,
            None => device
                .default_output_config()
                .map_err(|e| format!("{device_name}：拿不到缺省输出配置：{e}"))?,
        };
        let device_rate_hz = cfg.sample_rate() as f64;
        let channels = cfg.channels() as usize;
        let format = cfg.sample_format();
        let shared = Arc::new(Shared {
            ring: Mutex::new(VecDeque::with_capacity((fs_hz * CAP_S) as usize * 2 + 4096)),
            ratio: AtomicU64::new((fs_hz / device_rate_hz).to_bits()),
            volume: AtomicU32::new(1.0f32.to_bits()),
            prime_frames: AtomicUsize::new((fs_hz * PRIME_S) as usize),
            flush: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
        });
        let stream_cfg: cpal::StreamConfig = cfg.config();
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, stream_cfg, channels, shared.clone()),
            SampleFormat::I16 => build::<i16>(&device, stream_cfg, channels, shared.clone()),
            SampleFormat::U16 => build::<u16>(&device, stream_cfg, channels, shared.clone()),
            SampleFormat::I32 => build::<i32>(&device, stream_cfg, channels, shared.clone()),
            other => return Err(format!("{device_name}：不认识的样本格式 {other:?}")),
        }
        .map_err(|e| format!("{device_name}：{e}"))?;
        stream
            .play()
            .map_err(|e| format!("{device_name}：起不来输出流：{e}"))?;
        Ok(AudioOut {
            _stream: stream,
            shared,
            device_rate_hz,
            device_name,
        })
    }

    /// 推一段交织样本（源率）。`fs_out` 是此刻的源率，环的上限按它换算。
    pub fn push(&self, interleaved: &[f32], fs_out: f64) {
        let cap = ((fs_out * CAP_S) as usize).max(1024) * 2;
        let keep = ((fs_out * KEEP_S) as usize).max(512) * 2;
        let mut ring = self.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
        ring.extend(interleaved.iter().copied());
        if ring.len() > cap {
            // 攒太多了（引擎卡过一下之后一口气补发）：丢旧的，声音追上画面
            let drop = ring.len() - keep;
            ring.drain(..drop);
        }
    }

    /// 源率变了（换文件 / 改 speed）：比值和攒缓冲的长度都跟着变。
    pub fn set_source_rate(&self, fs_out: f64) {
        if fs_out > 0.0 {
            self.shared
                .ratio
                .store((fs_out / self.device_rate_hz).to_bits(), Ordering::Relaxed);
            self.shared
                .prime_frames
                .store((fs_out * PRIME_S) as usize, Ordering::Relaxed);
        }
    }

    pub fn set_volume(&self, v: f32) {
        self.shared
            .volume
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// 跳转 / 换文件：把还没放出去的丢掉，声音立刻跟到新位置。
    pub fn flush(&self) {
        self.shared.flush.store(true, Ordering::Release);
    }

    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    cfg: cpal::StreamConfig,
    channels: usize,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String> {
    let mut rs = Resampler::new();
    device
        .build_output_stream(
            cfg,
            move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                let ratio = f64::from_bits(shared.ratio.load(Ordering::Relaxed));
                let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
                let prime = shared.prime_frames.load(Ordering::Relaxed);
                // 拿不到锁（生产方正在推）就这一段静音——回调不许等
                let Ok(mut ring) = shared.ring.try_lock() else {
                    for s in out.iter_mut() {
                        *s = T::from_sample(0.0f32);
                    }
                    return;
                };
                if shared.flush.swap(false, Ordering::AcqRel) {
                    ring.clear();
                    rs.reset();
                }
                let missed = rs.render(&mut ring, out, channels, ratio, volume, prime);
                if missed > 0 {
                    shared.underruns.fetch_add(missed, Ordering::Relaxed);
                }
            },
            |e| eprintln!("wav_source: 输出流出错：{e}"),
            None,
        )
        .map_err(|e| format!("建不了输出流：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_of(frames: &[(f32, f32)]) -> VecDeque<f32> {
        frames.iter().flat_map(|&(l, r)| [l, r]).collect()
    }

    #[test]
    fn 比值为一_原样出_只慢一拍() {
        let mut ring = ring_of(&[(0.1, -0.1), (0.2, -0.2), (0.3, -0.3), (0.4, -0.4)]);
        let mut rs = Resampler::new();
        let mut out = [0.0f32; 8];
        let missed = rs.render(&mut ring, &mut out, 2, 1.0, 1.0, 0);
        assert_eq!(missed, 0);
        // 插值器是 prev→cur 的，第一拍出的是初始的 prev = 0，之后逐拍原样
        assert_eq!(&out[0..2], &[0.0, 0.0]);
        assert!((out[2] - 0.1).abs() < 1e-6 && (out[3] + 0.1).abs() < 1e-6);
        assert!((out[4] - 0.2).abs() < 1e-6);
        assert!((out[6] - 0.3).abs() < 1e-6);
        assert_eq!(ring.len(), 0, "四拍该全吃掉");
    }

    #[test]
    fn 比值为二_每个设备拍吃两个源拍() {
        let src: Vec<(f32, f32)> = (0..8).map(|i| (i as f32, -(i as f32))).collect();
        let mut ring = ring_of(&src);
        let mut rs = Resampler::new();
        let mut out = [0.0f32; 8];
        rs.render(&mut ring, &mut out, 2, 2.0, 1.0, 0);
        // 第一拍 prev=0；之后相位落在整数上，出的是 1, 3, 5（跨过 0/2/4）
        assert!((out[2] - 1.0).abs() < 1e-6, "{out:?}");
        assert!((out[4] - 3.0).abs() < 1e-6, "{out:?}");
        assert!((out[6] - 5.0).abs() < 1e-6, "{out:?}");
    }

    #[test]
    fn 比值为半_中点是线性插值() {
        let mut ring = ring_of(&[(0.0, 0.0), (1.0, -1.0), (2.0, -2.0)]);
        let mut rs = Resampler::new();
        let mut out = [0.0f32; 8];
        rs.render(&mut ring, &mut out, 2, 0.5, 1.0, 0);
        // 出：prev(0) 、0→1 的中点 ... 具体：拍0 = prev 初值 0；拍1 = 0.5·(0→?)
        // 相位序列：1.0(吃 0.0) → 0 → 0.5 → 1.0(吃 1.0) → 0 → 0.5 → 1.0(吃 2.0)
        // 输出：t=0 → prev; t=0.5 → 中点
        assert!((out[4] - 0.0).abs() < 1e-6, "{out:?}"); // 第 2 个设备拍：prev=0.0, t=0
        assert!((out[6] - 0.5).abs() < 1e-6, "{out:?}"); // 第 3 个：0.0→1.0 的一半
        assert!((out[7] + 0.5).abs() < 1e-6, "{out:?}");
    }

    #[test]
    fn 环空了出零并报欠载_攒够了才再出() {
        let mut ring = ring_of(&[(0.5, 0.5)]);
        let mut rs = Resampler::new();
        let mut out = [1.0f32; 6];
        let missed = rs.render(&mut ring, &mut out, 2, 1.0, 1.0, 0);
        assert!(missed >= 1, "该报欠载");
        assert_eq!(out[4], 0.0, "欠的拍出零");
        // 欠载后要重新攒：不足 prime 就整段静音、环不动
        ring.extend([0.3, 0.3]);
        let mut out = [1.0f32; 4];
        rs.render(&mut ring, &mut out, 2, 1.0, 1.0, 4);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(ring.len(), 2, "没攒够不许动环");
    }

    #[test]
    fn 单声道混合_多声道只填前两路_音量生效() {
        let mut ring = ring_of(&[(1.0, 0.0), (1.0, 0.0), (1.0, 0.0)]);
        let mut rs = Resampler::new();
        let mut out = [9.0f32; 3];
        rs.render(&mut ring, &mut out, 1, 1.0, 0.5, 0);
        assert!(
            (out[1] - 0.25).abs() < 1e-6,
            "单声道 = (l+r)/2 × 音量：{out:?}"
        );

        let mut ring = ring_of(&[(1.0, -1.0), (1.0, -1.0)]);
        let mut rs = Resampler::new();
        let mut out = [9.0f32; 8];
        rs.render(&mut ring, &mut out, 4, 1.0, 1.0, 0);
        assert_eq!(&out[4..8], &[1.0, -1.0, 0.0, 0.0], "{out:?}");
    }

    #[test]
    fn 整数样本格式走_from_sample() {
        let mut ring = ring_of(&[(1.0, -1.0), (1.0, -1.0)]);
        let mut rs = Resampler::new();
        let mut out = [0i16; 4];
        rs.render(&mut ring, &mut out, 2, 1.0, 1.0, 0);
        assert_eq!(out[2], i16::MAX);
        assert_eq!(out[3], i16::MIN);
    }
}

/// 真设备冒烟：`cargo test -p sigflow-plugin-wav-source -- --ignored --nocapture 真设备`。
/// 开系统默认输出、推半秒 440 Hz——单测默认不开声卡（CI 没有），要听就手动跑。
#[cfg(test)]
mod device_smoke {
    use super::*;

    #[test]
    #[ignore]
    fn 真设备_开得起来_出半秒_440() {
        let fs = 48_000.0;
        let a = AudioOut::open(fs).expect("开默认输出设备");
        eprintln!("设备 {}：{} Hz", a.device_name, a.device_rate_hz);
        a.set_source_rate(fs);
        a.set_volume(0.3);
        let mut buf = Vec::new();
        for i in 0..(fs as usize / 2) {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / fs as f32).sin();
            buf.push(v);
            buf.push(v);
        }
        // 分十段推：模拟每 tick 推一点
        for chunk in buf.chunks(buf.len() / 10) {
            a.push(chunk, fs);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        eprintln!("欠载 {} 拍（起播攒缓冲之前的不算）", a.underruns());
    }
}
