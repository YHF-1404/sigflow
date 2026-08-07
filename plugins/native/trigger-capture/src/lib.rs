//! Trigger-capture plugin: time-domain windowed capture (a digital-storage-
//! oscilloscope external trigger, upgraded by self-anchored frames).
//!
//! `signal` (consumer) carries a continuous multichannel interleaved f32
//! stream; `trigger` (consumer) carries event markers whose *event time* is the
//! frame's `header.t0_ns`. The plugin keeps the recent signal in a ring buffer
//! and, when a trigger fires, locates the window around the trigger's event
//! time and emits it on `capture` (producer).
//!
//! Why event-time (not arrival-time) alignment: each buffered sample maps to an
//! absolute time via its frame's `t0_ns` + `sample_index` and the stream rate,
//! and the trigger carries its own absolute `t0_ns`. Matching the two is immune
//! to transport/scheduling jitter — the captured window is anchored to the real
//! physical event, not to when its trigger message happened to arrive.
//!
//! Geometry (channels, sample rate) is **inferred** from the signal frames
//! (channels = bytes / (n_samples·4); fs from consecutive `t0_ns`/`sample_index`),
//! so they are not parameters and cannot be misconfigured.
//!
//! The captured window can exceed the shell's per-frame buffer (64 KiB), so it
//! is snapshotted out of the ring and **streamed in chunks** as contiguous
//! frames carrying the captured samples' original `sample_index`/`t0_ns`; the
//! first chunk sets `FLAG_DISCONTINUITY` to mark the capture boundary, with
//! `gap_samples` reporting any missing pre-trigger front (partial capture).
//! The first chunk also carries the **capture-window annotation**
//! (`{"window_rows":N,"trig_row":K,"fs_hz":F}`, capture convention) — downstream
//! reassemblers complete windows without duplicating `window_samples`, and
//! trigger consumers read the event row from the source of truth instead of
//! re-detecting it off the marker channel.
//!
//! Failure modes (see `record_trigger`): the event is older than anything
//! buffered → capture dropped + warning; the pre-trigger front was evicted
//! while waiting → partial window emitted + flagged.

use std::collections::VecDeque;

use sigflow_plugin_sdk::{
    decode_rate_annotation, encode_capture_window_annotation, Frame, FrameHeader, FrameOut,
    ParamValue, Plugin, PluginManifest, ProcessOutcome, CAPTURE_WINDOW_ANNOTATION_SCHEMA,
    RATE_ANNOTATION_SCHEMA,
};

/// Bytes per f32 sample.
const F32: usize = 4;
/// 首 chunk 末尾给 capture-window 注解预留的字节数（schema 8B + JSON 体）。
const ANNOTATION_RESERVE: usize = 64;
/// How many recent trigger skews to keep for the auto-size percentile.
const SKEW_WINDOW: usize = 256;

/// Capture state machine.
enum Capture {
    /// No trigger latched.
    Idle,
    /// Trigger latched; waiting for the post-trigger samples to arrive.
    Pending {
        /// Per-channel index of the window's first sample (may be < oldest if
        /// the front has already rolled out, or negative-relative if very early).
        win_start: i64,
        /// Per-channel index one past the window's last sample.
        win_end: i64,
        /// Per-channel index of the trigger event itself (where the marker
        /// channel steps low→high).
        k_t: i64,
    },
    /// Streaming a snapshotted window out in chunks.
    Emitting {
        /// Interleaved f32 snapshot of the captured window.
        buf: Vec<f32>,
        /// Per-channel index (in the original stream) of the snapshot's sample 0.
        start_index: u64,
        /// Absolute t0 (ns) of the snapshot's sample 0.
        start_t0_ns: i64,
        /// Per-channel samples already emitted.
        emitted: u64,
        /// Per-channel samples missing from the front (front eviction); else 0.
        front_gap: u64,
        /// Per-channel index of the trigger event (marker step point).
        k_t: i64,
    },
}

pub struct TriggerCapture {
    // --- Parameters ---
    window_samples: u64, // per channel
    trigger_pos: f64,    // 0..1
    latency_margin_ms: f64,
    auto_size: bool,
    /// auto_size 自适应余量的硬顶（ms）：skew 统计再大，环缓也不越此线。
    max_margin_ms: f64,
    trigger_channel: bool, // append a low→high marker channel at the trigger
    trigger_level: f64,    // high level of the marker channel (e.g. 3.3)

    // --- Inferred stream geometry ---
    channels: usize, // 0 until first signal frame
    fs: f64,         // per-channel Hz; 0 until declared/inferred
    /// 源侧带内速率宣告（RATE 注解，如 sdc-usb）；有则恒优先于帧头推断。
    fs_declared: Option<f64>,
    // Anchor mapping absolute time <-> per-channel index (from a signal frame).
    anchor_t0_ns: i64,
    anchor_index: u64,
    have_anchor: bool,
    prev_idx: Option<(u64, i64)>, // (sample_index, t0_ns) of the last signal frame

    // --- Ring of interleaved f32 samples ---
    ring: VecDeque<f32>,
    oldest_index: u64, // per-channel index of the ring's first sample

    // --- Capture state ---
    cap: Capture,
    last_trigger: Option<(u64, i64)>, // (seq, t0) to de-dupe repeated frames
    skews: VecDeque<i64>,             // recent (newest_data − k_T), per-channel samples

    // --- Diagnostics ---
    captures: u64,
    partial_captures: u64,
    missed_captures: u64,
    dropped_triggers: u64,
    torn_holes: u64,
    torn_hole_samples: u64,
}

impl TriggerCapture {
    /// Per-channel sample count currently in the ring.
    fn ring_samples(&self) -> u64 {
        if self.channels == 0 {
            0
        } else {
            (self.ring.len() / self.channels) as u64
        }
    }

    /// Per-channel index one past the newest buffered sample.
    fn newest_index(&self) -> u64 {
        self.oldest_index + self.ring_samples()
    }

    /// Absolute time (ns) of a per-channel sample index.
    fn time_of(&self, index: i64) -> i64 {
        if self.fs <= 0.0 {
            return self.anchor_t0_ns;
        }
        self.anchor_t0_ns + (((index - self.anchor_index as i64) as f64 / self.fs) * 1e9) as i64
    }

    /// Per-channel sample index nearest an absolute time (ns).
    fn index_of_time(&self, t_ns: i64) -> i64 {
        if self.fs <= 0.0 {
            return self.anchor_index as i64;
        }
        self.anchor_index as i64
            + (((t_ns - self.anchor_t0_ns) as f64 * self.fs) / 1e9).round() as i64
    }

    /// Target per-channel ring capacity = window + headroom for trigger-vs-data
    /// skew. Headroom is the manual margin, optionally raised to the observed
    /// skew p99 when auto-sizing.
    fn capacity_samples(&self) -> u64 {
        if self.fs <= 0.0 {
            return self.window_samples.saturating_mul(2);
        }
        let mut margin = (self.latency_margin_ms / 1000.0 * self.fs).round().max(0.0) as u64;
        if self.auto_size {
            // 自适应部分带硬顶：skew 统计被异常值（如陈年补传触发）毒化时，
            // 环缓最多长到 max_margin_ms——没有这道闸，真机上一条 2.8h 前的
            // 旧触发曾把容量撑到 6.5 亿样本，34 分钟吃满 2GB 被 OOM 杀。
            let auto_cap = (self.max_margin_ms / 1000.0 * self.fs).round().max(0.0) as u64;
            margin = margin.max(self.skew_p99().min(auto_cap));
        }
        self.window_samples.saturating_add(margin)
    }

    /// 99th percentile of recent positive skews (per-channel samples). Positive
    /// skew = trigger lagged the data, which is what eats pre-trigger headroom.
    fn skew_p99(&self) -> u64 {
        if self.skews.is_empty() {
            return 0;
        }
        let mut v: Vec<i64> = self.skews.iter().map(|&s| s.max(0)).collect();
        v.sort_unstable();
        let idx = (((v.len() - 1) as f64) * 0.99).round() as usize;
        v[idx].max(0) as u64
    }

    /// Drop the oldest samples until the ring fits the current capacity.
    fn evict_to_capacity(&mut self) {
        if self.channels == 0 {
            return;
        }
        let cap = self.capacity_samples();
        while self.ring_samples() > cap {
            for _ in 0..self.channels {
                self.ring.pop_front();
            }
            self.oldest_index += 1;
        }
    }

    /// Forget all buffered data and re-anchor to a fresh frame (used on the
    /// first frame and on a stream discontinuity).
    fn reset_stream_anchor(&mut self, sample_index: u64, t0_ns: i64) {
        self.ring.clear();
        self.oldest_index = sample_index;
        self.anchor_index = sample_index;
        self.anchor_t0_ns = t0_ns;
        self.have_anchor = true;
        // A pending capture is anchored to data that no longer exists.
        if matches!(self.cap, Capture::Pending { .. }) {
            eprintln!("trigger_capture: stream discontinuity — pending capture aborted");
            self.cap = Capture::Idle;
            self.missed_captures += 1;
        }
    }

    /// Ingest one signal frame: infer geometry, append samples, evict.
    fn ingest_signal(&mut self, h: &FrameHeader, data: &[u8]) {
        let n = h.n_samples as usize;
        if n == 0 || data.len() < n * F32 {
            return;
        }

        // Infer channels from the interleaved layout.
        let inferred_ch = data.len() / (n * F32);
        if inferred_ch == 0 {
            return;
        }
        if self.channels == 0 {
            self.channels = inferred_ch;
        } else if inferred_ch != self.channels {
            // Geometry changed under us — restart cleanly.
            eprintln!(
                "trigger_capture: RESET(channels {} → {inferred_ch}) idx={} n={} len={} \
                 flags={:#x} t0={}",
                self.channels, h.sample_index, n, data.len(), h.flags, h.t0_ns
            );
            self.channels = inferred_ch;
            self.reset_stream_anchor(h.sample_index, h.t0_ns);
        }

        // Infer fs from consecutive frames (exact, since the source derives
        // t0 from fs). Valid across gaps too (idx and t0 jump together).
        // fs: update only from contiguity-clean pairs. Gap frames carry a
        // *rounded* sample-count estimate in their index jump, so their pair
        // ratio jitters by tens of ppm — multiplied by the time→index lever
        // that was observed to displace triggers by whole seconds on aged
        // anchors. Clean pairs are exact (the source derives t0 from fs).
        if let Some(declared) = self.fs_declared {
            self.fs = declared; // 源侧宣告优先（与该流 t0 轴自洽由源保证）
        } else if let Some((pidx, pt0)) = self.prev_idx {
            if !h.is_discontinuity() && h.sample_index > pidx && h.t0_ns > pt0 {
                let cand = (h.sample_index - pidx) as f64 * 1e9 / (h.t0_ns - pt0) as f64;
                if self.fs > 0.0 && (cand / self.fs - 1.0).abs() > 1e-4 {
                    eprintln!(
                        "trigger_capture: fs step {:.4} → {:.4} from pair ({pidx},{pt0}) → \
                         ({},{}) n={} flags={:#x}",
                        self.fs, cand, h.sample_index, h.t0_ns, h.n_samples, h.flags
                    );
                }
                self.fs = cand;
            }
        }
        self.prev_idx = Some((h.sample_index, h.t0_ns));

        // Anchor / contiguity. Small FORWARD holes — whether honest acquisition
        // loss (FLAG_DISCONTINUITY + gap_samples, e.g. the SDC gateway's ADC
        // dropping 36–131-sample runs while it relays a pen window over RF) or
        // silent transport tearing (latest-wins drains under load) — are FILLED
        // with the last seen values so the axis stays contiguous. Wiping the
        // ring for them would starve the late triggers this ring exists to
        // serve (observed live: the gaps cluster exactly while a trigger is in
        // flight, killing most captures). Only a real stream break — a rewind
        // or a jump beyond ~1s — re-anchors and drops history.
        if !self.have_anchor || self.ring.is_empty() {
            self.reset_stream_anchor(h.sample_index, h.t0_ns);
        } else if h.sample_index != self.newest_index() {
            let newest = self.newest_index();
            let max_hole = self.fs.max(1.0) as u64;
            if h.sample_index > newest && h.sample_index - newest <= max_hole {
                let hole = (h.sample_index - newest) as usize;
                let fill: Vec<f32> = self
                    .ring
                    .iter()
                    .rev()
                    .take(self.channels)
                    .rev()
                    .copied()
                    .collect();
                for _ in 0..hole {
                    for &v in &fill {
                        self.ring.push_back(v);
                    }
                }
                self.torn_holes += 1;
                self.torn_hole_samples += hole as u64;
                eprintln!(
                    "trigger_capture: hole — {} samples/ch missing before index {} \
                     ({}; filled with last values; holes={}, hole_samples={})",
                    hole,
                    h.sample_index,
                    if h.is_discontinuity() {
                        "flagged acquisition gap"
                    } else {
                        "unflagged transport tear"
                    },
                    self.torn_holes,
                    self.torn_hole_samples
                );
            } else {
                eprintln!(
                    "trigger_capture: RESET stream index {} → {} (flags={:#x}) — re-anchoring, \
                     buffer dropped",
                    newest, h.sample_index, h.flags
                );
                self.reset_stream_anchor(h.sample_index, h.t0_ns);
            }
        }
        // A DISCONTINUITY flag on a contiguous frame (epoch marker, gap already
        // accounted in sample_index) needs no action: the axis is intact.

        // Append this frame's interleaved samples.
        let take = n * self.channels;
        for chunk in data[..take * F32].chunks_exact(F32) {
            self.ring
                .push_back(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        self.evict_to_capacity();

        // Keep the time↔index anchor on the newest frame: the conversion lever
        // stays one frame long instead of growing with session age (an aged
        // anchor amplifies ppm-level fs jitter into second-level trigger
        // displacement — observed live at ~40 min anchor age). Backward
        // extrapolation over the buffered window is bounded by ring depth, so
        // the residual error is microseconds.
        self.anchor_index = h.sample_index;
        self.anchor_t0_ns = h.t0_ns;
    }

    /// Latch a trigger fired at event time `t_ns` by mapping it onto the
    /// stream's sample axis through this plugin's anchor + inferred rate.
    /// Prefer [`Self::record_trigger_at`] when the marker already carries a
    /// stream-axis index (`FLAG_TRIG_INDEXED`): the time round trip re-derives
    /// the index against an anchor that ages and a per-frame rate estimate
    /// that steps when the source's clock recovery updates — an error lever
    /// proportional to anchor age (observed as occasional multi-ms outliers).
    fn record_trigger(&mut self, t_ns: i64) {
        if self.fs <= 0.0 || !self.have_anchor || self.channels == 0 {
            eprintln!("trigger_capture: trigger before the stream was established — dropped");
            self.missed_captures += 1;
            return;
        }
        let k_t = self.index_of_time(t_ns);
        eprintln!(
            "trigger_capture: trigger t={} → k_t={} (anchor_idx={} anchor_t0={} fs={:.4} \
             newest={} oldest={})",
            t_ns,
            k_t,
            self.anchor_index,
            self.anchor_t0_ns,
            self.fs,
            self.newest_index(),
            self.oldest_index
        );
        self.record_trigger_at(k_t);
    }

    /// Latch a trigger at per-channel stream index `k_t` (exact, no time
    /// conversion). Sets state (Pending) or records a miss/drop.
    fn record_trigger_at(&mut self, k_t: i64) {
        if self.fs <= 0.0 || !self.have_anchor || self.channels == 0 {
            eprintln!("trigger_capture: trigger before the stream was established — dropped");
            self.missed_captures += 1;
            return;
        }
        if !matches!(self.cap, Capture::Idle) {
            // Holdoff: a capture is already pending or streaming out.
            self.dropped_triggers += 1;
            return;
        }

        let pre = (self.trigger_pos * self.window_samples as f64).round() as i64;
        let win_start = k_t - pre;
        let win_end = win_start + self.window_samples as i64;

        if k_t < self.oldest_index as i64 {
            // The event itself has already rolled out of the ring. 这类事件
            // **不进 skew 统计**：它不是链路抖动而是陈年事件（笔断链后补传
            // 的旧窗见过 2.8h 前的），为它自适应扩容毫无意义且会毒化 p99
            // （真机 OOM 事故根因）。边界迟到但仍部分可捕的触发照常入账。
            eprintln!(
                "trigger_capture: trigger event is older than the buffer (event idx {}, oldest {}) \
                 — capture dropped; raise latency_margin_ms or enable auto_size",
                k_t, self.oldest_index
            );
            self.missed_captures += 1;
            return;
        }

        // Skew = how far the event sits in the buffered past (positive = the
        // trigger lagged the data; this is what eats pre-trigger headroom).
        let skew = self.newest_index() as i64 - k_t;
        if self.skews.len() == SKEW_WINDOW {
            self.skews.pop_front();
        }
        self.skews.push_back(skew);

        self.cap = Capture::Pending { win_start, win_end, k_t };
    }

    /// Snapshot `[start, end)` (per-channel) out of the ring into an interleaved
    /// f32 Vec. Caller guarantees `oldest <= start < end <= newest`.
    fn snapshot(&self, start: u64, end: u64) -> Vec<f32> {
        let off = (start - self.oldest_index) as usize * self.channels;
        let count = (end - start) as usize * self.channels;
        self.ring.iter().skip(off).take(count).copied().collect()
    }

    /// Advance the capture state machine and, when emitting, fill `out` with one
    /// chunk. Returns true if `out` was written.
    fn drive(&mut self, out: &mut FrameOut) -> bool {
        // Pending → Emitting once the post-trigger samples have arrived.
        if let Capture::Pending { win_start, win_end, k_t } = self.cap {
            if self.newest_index() as i64 >= win_end {
                let actual_start = win_start.max(self.oldest_index as i64);
                let front_gap = (actual_start - win_start).max(0) as u64;
                let start = actual_start as u64;
                let end = win_end as u64;
                let buf = self.snapshot(start, end);
                let start_t0_ns = self.time_of(actual_start);
                self.captures += 1;
                let n = end - start;
                if front_gap > 0 {
                    self.partial_captures += 1;
                    eprintln!(
                        "trigger_capture: PARTIAL capture #{} — {} samples/ch from index {} \
                         (t0={} ns), {} pre-trigger samples missing (raise latency_margin_ms \
                         or enable auto_size)",
                        self.captures, n, start, start_t0_ns, front_gap
                    );
                } else {
                    eprintln!(
                        "trigger_capture: capture #{} — {} samples/ch from index {} (t0={} ns)",
                        self.captures, n, start, start_t0_ns
                    );
                }
                self.cap = Capture::Emitting {
                    buf,
                    start_index: start,
                    start_t0_ns,
                    emitted: 0,
                    front_gap,
                    k_t,
                };
            }
        }

        // Emit one chunk of the snapshot.
        if let Capture::Emitting {
            buf,
            start_index,
            start_t0_ns,
            emitted,
            front_gap,
            k_t,
        } = &mut self.cap
        {
            let ch = self.channels.max(1);
            // When enabled, append one extra "trigger marker" channel per sample:
            // low (0.0) before the trigger sample, high (trigger_level, e.g. 3.3)
            // from the trigger sample onward — so the capture shows WHERE the
            // event landed. n_samples stays per-channel, so the downstream
            // monitor auto-derives the extra channel from the wider rows.
            let marker_on = self.trigger_channel;
            let level = self.trigger_level as f32;
            let ch_out = if marker_on { ch + 1 } else { ch };
            let total = (buf.len() / ch) as u64;
            let remaining = total - *emitted;
            // 首 chunk 末尾要挂 capture-window 注解——把预留量从可用容量里扣掉
            let reserve = if *emitted == 0 { ANNOTATION_RESERVE } else { 0 };
            let out_cap_samples = ((out.capacity() - reserve) / (ch_out * F32)) as u64;
            let chunk = remaining.min(out_cap_samples);
            if chunk == 0 {
                // Output buffer can't hold even one multichannel sample.
                return false;
            }

            let dst = out.buffer_mut();
            for s in 0..chunk as usize {
                let src_base = (*emitted as usize + s) * ch;
                let dst_base = (s * ch_out) * F32;
                for c in 0..ch {
                    let v = buf[src_base + c];
                    let di = dst_base + c * F32;
                    dst[di..di + F32].copy_from_slice(&v.to_le_bytes());
                }
                if marker_on {
                    let abs_idx = *start_index as i64 + *emitted as i64 + s as i64;
                    let m = if abs_idx >= *k_t { level } else { 0.0 };
                    let di = dst_base + ch * F32;
                    dst[di..di + F32].copy_from_slice(&m.to_le_bytes());
                }
            }
            out.set_written(chunk as usize * ch_out * F32);

            out.header.sample_index = *start_index + *emitted;
            out.header.n_samples = chunk as u32;
            out.header.t0_ns = if self.fs > 0.0 {
                *start_t0_ns + ((*emitted as f64 / self.fs) * 1e9) as i64
            } else {
                *start_t0_ns
            };
            // The first chunk marks a new capture boundary; gap_samples reports
            // the missing pre-trigger front (0 when the capture was complete).
            // It also carries the capture-window annotation: whole-window rows
            // + the trigger event's row (both已按 partial 截短修正)。
            if *emitted == 0 {
                out.header.set_discontinuity(*front_gap);
                let trig_row = (*k_t - *start_index as i64).max(0) as u64;
                let fs_pub = (self.fs > 0.0).then_some(self.fs);
                out.set_annotation(
                    CAPTURE_WINDOW_ANNOTATION_SCHEMA,
                    &encode_capture_window_annotation(total, trig_row, fs_pub),
                );
            }

            *emitted += chunk;
            if *emitted >= total {
                self.cap = Capture::Idle;
            }
            return true;
        }

        false
    }
}

impl Plugin for TriggerCapture {
    fn new(_manifest: &PluginManifest) -> Self {
        TriggerCapture {
            window_samples: 2048,
            trigger_pos: 0.3,
            latency_margin_ms: 50.0,
            auto_size: false,
            max_margin_ms: 30_000.0,
            trigger_channel: true,
            trigger_level: 3.3,
            channels: 0,
            fs: 0.0,
            fs_declared: None,
            anchor_t0_ns: 0,
            anchor_index: 0,
            have_anchor: false,
            prev_idx: None,
            ring: VecDeque::new(),
            oldest_index: 0,
            cap: Capture::Idle,
            last_trigger: None,
            skews: VecDeque::new(),
            captures: 0,
            partial_captures: 0,
            missed_captures: 0,
            dropped_triggers: 0,
            torn_holes: 0,
            torn_hole_samples: 0,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("window_samples", ParamValue::U32(v)) => {
                self.window_samples = (*v as u64).clamp(16, 1_000_000);
            }
            ("trigger_pos", ParamValue::F64(v)) => self.trigger_pos = v.clamp(0.0, 1.0),
            ("latency_margin_ms", ParamValue::F64(v)) => {
                self.latency_margin_ms = v.clamp(0.0, 10_000.0);
            }
            ("auto_size", ParamValue::Bool(v)) => self.auto_size = *v,
            ("max_margin_ms", ParamValue::F64(v)) => {
                self.max_margin_ms = v.clamp(1000.0, 600_000.0);
            }
            ("trigger_channel", ParamValue::Bool(v)) => self.trigger_channel = *v,
            ("trigger_level", ParamValue::F64(v)) => self.trigger_level = *v,
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        // Fresh stream: forget everything (geometry is re-inferred).
        self.channels = 0;
        self.fs = 0.0;
        self.have_anchor = false;
        self.prev_idx = None;
        self.ring.clear();
        self.oldest_index = 0;
        self.cap = Capture::Idle;
        self.last_trigger = None;
        self.skews.clear();
        ProcessOutcome::Ok
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        // Port order matches the manifest: 0 = signal, 1 = trigger.
        if let Some(sig) = inputs.first() {
            // 源侧带内速率宣告（如 sdc-usb 冻结时钟映射的斜率）：时钟裁决
            // 权在源节点——宣告值直接覆盖本地帧头推断。
            if let Some(fs) = sig
                .annotation()
                .and_then(sigflow_plugin_sdk::parse_annotation)
                .filter(|(schema, _)| *schema == RATE_ANNOTATION_SCHEMA)
                .and_then(|(_, body)| decode_rate_annotation(body))
            {
                self.fs_declared = Some(fs);
            }
            // 样本区剥掉注解尾巴再入环（annotation 不是信号）。
            self.ingest_signal(&sig.header, sig.samples());
        }

        // A trigger marker is present when its event time is set and the frame
        // is not the same one we already handled (last-wins can repeat). Empty
        // placeholder frames (a tick with no trigger) have t0_ns == 0.
        if let Some(trig) = inputs.get(1) {
            let h = &trig.header;
            if h.t0_ns != 0 && self.last_trigger != Some((h.seq, h.t0_ns)) {
                self.last_trigger = Some((h.seq, h.t0_ns));
                if h.is_trig_indexed() {
                    // Marker carries its stream-axis sample index: align by
                    // index directly, skipping the time→index conversion.
                    self.record_trigger_at(h.sample_index as i64);
                } else {
                    self.record_trigger(h.t0_ns);
                }
            }
        }

        if let Some(out) = outputs.first_mut() {
            if !self.drive(out) {
                // Nothing to emit this tick.
                out.set_written(0);
                out.header.n_samples = 0;
            }
        }
        ProcessOutcome::Ok
    }

    fn invoke_action(&mut self, id: &str) {
        if id == "reset" {
            self.ring.clear();
            self.have_anchor = false;
            self.prev_idx = None;
            self.cap = Capture::Idle;
            self.last_trigger = None;
        }
    }
}

sigflow_plugin_sdk::export_plugin!(TriggerCapture);

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 1000.0; // 1 kHz/ch keeps the math tidy: 1 sample = 1 ms
    const CH: usize = 2;

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    fn new_plugin(window: u32, pos: f64, margin_ms: f64) -> TriggerCapture {
        let mut p = TriggerCapture::new(&manifest());
        p.set_param("window_samples", &ParamValue::U32(window));
        p.set_param("trigger_pos", &ParamValue::F64(pos));
        p.set_param("latency_margin_ms", &ParamValue::F64(margin_ms));
        p
    }

    /// A signal frame: `n` per-channel samples starting at `idx`, value of each
    /// sample = its per-channel index (so we can assert exact contents).
    fn signal_frame(idx: u64, n: u64) -> (FrameHeader, Vec<u8>) {
        let mut h = FrameHeader::ZERO;
        h.sample_index = idx;
        h.n_samples = n as u32;
        h.t0_ns = (idx as f64 / FS * 1e9) as i64;
        let mut data = Vec::with_capacity((n as usize) * CH * F32);
        for s in idx..idx + n {
            for c in 0..CH {
                let v = (s as f32) + (c as f32) * 0.001;
                data.extend_from_slice(&v.to_le_bytes());
            }
        }
        (h, data)
    }

    fn trigger_frame(seq: u64, t_ns: i64) -> (FrameHeader, Vec<u8>) {
        let mut h = FrameHeader::ZERO;
        h.seq = seq;
        h.t0_ns = t_ns;
        (h, Vec::new())
    }

    /// Drive one process() with optional signal and trigger frames; collect the
    /// emitted chunk (if any) as decoded per-channel sample-0 values + header.
    fn step(
        p: &mut TriggerCapture,
        sig: Option<&(FrameHeader, Vec<u8>)>,
        trig: Option<&(FrameHeader, Vec<u8>)>,
    ) -> Option<(FrameHeader, Vec<f32>)> {
        let empty = (FrameHeader::ZERO, Vec::new());
        let s = sig.unwrap_or(&empty);
        let t = trig.unwrap_or(&empty);
        let inputs = [
            Frame::new(s.0, &s.1),
            Frame::new(t.0, &t.1),
        ];
        let mut buf = vec![0u8; 64 * 1024];
        let (written, header) = {
            let mut outs = [FrameOut::new(&mut buf)];
            p.process(&inputs, &mut outs);
            (outs[0].written(), outs[0].header)
        };
        if written == 0 {
            return None;
        }
        // 样本区 = written 去掉注解尾巴（Frame::samples 同款语义）
        let sample_bytes = written - header.annotation_len as usize;
        let vals: Vec<f32> = buf[..sample_bytes]
            .chunks_exact(F32)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Some((header, vals))
    }

    /// step + 解出首 chunk 的 capture-window 注解 (window_rows, trig_row)。
    fn step_annot(
        p: &mut TriggerCapture,
        sig: Option<&(FrameHeader, Vec<u8>)>,
    ) -> Option<(FrameHeader, Vec<f32>, Option<(u64, u64, Option<f64>)>)> {
        let empty = (FrameHeader::ZERO, Vec::new());
        let s = sig.unwrap_or(&empty);
        let inputs = [Frame::new(s.0, &s.1), Frame::new(FrameHeader::ZERO, &[])];
        let mut buf = vec![0u8; 64 * 1024];
        let (written, header) = {
            let mut outs = [FrameOut::new(&mut buf)];
            p.process(&inputs, &mut outs);
            (outs[0].written(), outs[0].header)
        };
        if written == 0 {
            return None;
        }
        let sample_bytes = written - header.annotation_len as usize;
        let vals: Vec<f32> = buf[..sample_bytes]
            .chunks_exact(F32)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let annot = Frame::new(header, &buf[..written])
            .annotation()
            .and_then(sigflow_plugin_sdk::parse_annotation)
            .filter(|(schema, _)| *schema == CAPTURE_WINDOW_ANNOTATION_SCHEMA)
            .and_then(|(_, body)| sigflow_plugin_sdk::decode_capture_window_annotation(body));
        Some((header, vals, annot))
    }

    fn feed(p: &mut TriggerCapture, from: u64, frames: u64, n: u64) {
        for i in 0..frames {
            let f = signal_frame(from + i * n, n);
            assert!(step(p, Some(&f), None).is_none());
        }
    }

    #[test]
    fn captures_window_around_trigger_event_time() {
        // window 100, pos 0.3 → 30 pre, 70 post. Trigger at per-channel index 50.
        let mut p = new_plugin(100, 0.3, 1000.0);
        feed(&mut p, 0, 10, 10); // indices 0..100 buffered, fs/channels inferred
        assert_eq!(p.channels, CH);
        assert!((p.fs - FS).abs() < 1.0);

        // Trigger at the time of per-channel index 50.
        let t = trigger_frame(1, (50.0 / FS * 1e9) as i64);
        // Post-trigger (50..120) not fully buffered yet (newest=100) → pending.
        assert!(step(&mut p, None, Some(&t)).is_none());
        assert_eq!(p.captures, 0);

        // Feed more until win_end (50-30=20 start, +100 = index 120) is buffered.
        // Collect all emitted chunks. The marker channel is appended by default,
        // so each emitted row is CH+1 wide: channel 0 first, marker last.
        let row = CH + 1;
        let mut got: Vec<f32> = Vec::new();
        let mut marker: Vec<f32> = Vec::new();
        let mut first_header: Option<FrameHeader> = None;
        let mut first_annotation: Option<(u64, u64, Option<f64>)> = None;
        for i in 10..16 {
            let f = signal_frame(i * 10, 10);
            if let Some((h, vals, annot)) = step_annot(&mut p, Some(&f)) {
                if first_header.is_none() {
                    first_annotation = annot;
                }
                first_header.get_or_insert(h);
                got.extend(vals.iter().step_by(row).copied()); // channel 0
                marker.extend(vals.iter().skip(CH).step_by(row).copied()); // marker channel
            }
        }
        assert_eq!(p.captures, 1);
        assert_eq!(p.partial_captures, 0);
        let h = first_header.expect("a chunk was emitted");
        assert_eq!(h.sample_index, 20); // win_start = 50 - 30
        assert!(h.is_discontinuity()); // capture boundary
        assert_eq!(h.gap_samples, 0); // complete window
        // 首 chunk 携带 capture-window 注解：全窗行数 + 触发行（50-20=30）
        assert!(h.annotation_len > 0, "capture-window annotation missing");
        // fs 由帧头推断得 1000.0（无上游宣告），随注解下发
        assert_eq!(first_annotation, Some((100, 30, Some(1000.0))));
        // First captured per-channel sample value == its index (20), full window.
        assert_eq!(got.len(), 100);
        assert_eq!(got[0], 20.0);
        assert_eq!(got[99], 119.0);
        // Marker channel: 0 before the trigger (abs index 50 → window offset 30),
        // 3.3 from the trigger sample onward.
        assert_eq!(marker.len(), 100);
        assert_eq!(marker[29], 0.0);
        assert_eq!(marker[30], 3.3);
        assert_eq!(marker[99], 3.3);
    }

    #[test]
    fn miss_when_event_older_than_buffer() {
        // Small margin so the ring is shallow; trigger far in the past.
        let mut p = new_plugin(50, 0.5, 0.0); // capacity ≈ window (50)
        feed(&mut p, 0, 20, 10); // indices 100..200 retained (oldest≈150)
        let oldest = p.oldest_index;
        assert!(oldest > 0);
        // Trigger for an index well below oldest → dropped miss.
        let t = trigger_frame(1, (10.0 / FS * 1e9) as i64);
        assert!(step(&mut p, None, Some(&t)).is_none());
        assert_eq!(p.missed_captures, 1);
        assert_eq!(p.captures, 0);
    }

    /// 陈年触发（比缓冲还老,被丢弃）不得进 skew 统计——auto_size 曾被
    /// 一条 2.8h 前的补传触发把容量撑到 6.5 亿样本,34 分钟 OOM。
    #[test]
    fn stale_trigger_does_not_poison_auto_size() {
        let mut p = new_plugin(50, 0.5, 0.0);
        p.set_param("auto_size", &ParamValue::Bool(true));
        feed(&mut p, 0, 20, 10); // indices 0..200, ring ≈ window(50)
        let cap_before = p.capacity_samples();
        // 触发指向远古（甚至负下标）→ dropped miss
        let t = trigger_frame(1, -3_600_000_000_000);
        assert!(step(&mut p, None, Some(&t)).is_none());
        assert_eq!(p.missed_captures, 1);
        assert!(p.skews.is_empty(), "stale trigger must not enter skew stats");
        assert_eq!(p.capacity_samples(), cap_before, "capacity must not inflate");
    }

    /// auto_size 的自适应余量受 max_margin_ms 硬顶约束（防统计毒化的兜底）。
    #[test]
    fn auto_margin_clamped_by_ceiling() {
        let mut p = new_plugin(50, 0.5, 1000.0); // manual margin 1s = 1000 samples
        p.set_param("auto_size", &ParamValue::Bool(true));
        p.set_param("max_margin_ms", &ParamValue::F64(2000.0)); // 顶 = 2000 samples
        feed(&mut p, 0, 105, 10); // ring 深 1050（容量内无驱逐,oldest=0）
        // 缓冲内的大 skew（合法迟到）：k_t=10 → skew = 1050-10 = 1040
        let t = trigger_frame(1, (10.0 / FS * 1e9) as i64);
        let _ = step(&mut p, None, Some(&t));
        assert_eq!(p.skews.len(), 1);
        // p99 = 1040 < 2000 顶 → 生效值 = max(manual 1000, 1040) = 1040
        assert_eq!(p.capacity_samples(), 50 + 1040);
        // 收紧顶到 0.5s：自适应部分被压到 500，manual 1000 接管
        p.set_param("max_margin_ms", &ParamValue::F64(1000.0));
        assert_eq!(p.capacity_samples(), 50 + 1000);
    }

    #[test]
    fn partial_when_front_evicted() {
        // pos 1.0 → all pre-trigger. Tight margin so the front is at risk.
        let mut p = new_plugin(100, 1.0, 0.0); // capacity ≈ 100
        feed(&mut p, 0, 12, 10); // newest≈120, oldest≈20
        // Trigger at index 60: window [60-100 .. 60) = [-40..60) → front far below
        // oldest(20) → partial, but k_T(60) >= oldest so not a full miss.
        let t = trigger_frame(1, (60.0 / FS * 1e9) as i64);
        let mut first: Option<FrameHeader> = None;
        // win_end = 60 already <= newest, so it should emit promptly.
        if let Some((h, _)) = step(&mut p, None, Some(&t)) {
            first = Some(h);
        }
        let h = first.expect("partial capture should emit");
        assert_eq!(p.partial_captures, 1);
        assert!(h.is_discontinuity());
        assert!(h.gap_samples > 0); // missing pre-trigger front reported
        assert_eq!(h.sample_index, p_oldest_at_capture());
        fn p_oldest_at_capture() -> u64 {
            20 // oldest after feeding 0..120 with capacity 100
        }
    }

    #[test]
    fn indexed_trigger_bypasses_time_conversion() {
        // window 100, pos 0.3 → 30 pre, 70 post. Marker carries stream index 50
        // (flagged) but a deliberately WRONG t0 (time of index 90): the index
        // path must win, anchoring the window at 50 − 30 = 20.
        let mut p = new_plugin(100, 0.3, 1000.0);
        feed(&mut p, 0, 10, 10); // indices 0..100 buffered

        let mut h = FrameHeader::ZERO;
        h.seq = 1;
        h.sample_index = 50;
        h.t0_ns = (90.0 / FS * 1e9) as i64; // wrong on purpose
        h.set_trig_indexed();
        let t = (h, Vec::new());
        assert!(step(&mut p, None, Some(&t)).is_none()); // pending (post not buffered)

        let row = CH + 1;
        let mut got: Vec<f32> = Vec::new();
        let mut first_header: Option<FrameHeader> = None;
        for i in 10..16 {
            let f = signal_frame(i * 10, 10);
            if let Some((hh, vals)) = step(&mut p, Some(&f), None) {
                first_header.get_or_insert(hh);
                got.extend(vals.iter().step_by(row).copied());
            }
        }
        assert_eq!(p.captures, 1);
        let hh = first_header.expect("a chunk was emitted");
        // Window anchored by the carried INDEX (50−30=20), not the bogus t0
        // (which would have put win_start at 90−30=60).
        assert_eq!(hh.sample_index, 20);
        assert_eq!(got[0], 20.0);
    }

    #[test]
    fn flagged_acquisition_gap_filled_without_dropping_ring() {
        // The source honestly reports a small acquisition gap: DISCONTINUITY
        // flag + sample_index jumped by gap_samples (the SDC gateway drops
        // 36–131-sample runs while relaying pen windows over RF). History
        // must survive and pre-gap events must still capture.
        let mut p = new_plugin(100, 0.3, 1000.0);
        feed(&mut p, 0, 10, 10); // 0..100
        let mut f = signal_frame(140, 10); // 40 samples lost, honestly flagged
        f.0.set_discontinuity(40);
        assert!(step(&mut p, Some(&f), None).is_none());
        assert_eq!(p.torn_holes, 1);
        assert_eq!(p.torn_hole_samples, 40);
        assert_eq!(p.oldest_index, 0, "flagged small gap must not wipe the ring");
        assert_eq!(p.newest_index(), 150);
        let t = trigger_frame(1, (50.0 / FS * 1e9) as i64);
        let (h, _) = step(&mut p, None, Some(&t)).expect("pre-gap event captured");
        assert_eq!(h.sample_index, 20);
        assert_eq!(p.captures, 1);
    }

    #[test]
    fn epoch_marker_on_contiguous_frame_keeps_ring() {
        // DISCONTINUITY flag with gap already folded into a CONTIGUOUS index
        // (a stream-begin boundary marker) needs no reset at all.
        let mut p = new_plugin(100, 0.3, 1000.0);
        feed(&mut p, 0, 10, 10);
        let mut f = signal_frame(100, 10);
        f.0.set_discontinuity(0);
        assert!(step(&mut p, Some(&f), None).is_none());
        assert_eq!(p.oldest_index, 0, "contiguous flagged frame must not reset");
        assert_eq!(p.torn_holes, 0);
        assert_eq!(p.newest_index(), 110);
    }

    #[test]
    fn transport_hole_filled_without_dropping_ring() {
        // window 100, pos 0.3 → 30 pre, 70 post; deep margin.
        let mut p = new_plugin(100, 0.3, 1000.0);
        feed(&mut p, 0, 10, 10); // indices 0..100, values == index

        // Latest-wins tore out indices 100..130: next frame jumps to 130.
        let f = signal_frame(130, 10);
        assert!(step(&mut p, Some(&f), None).is_none());
        assert_eq!(p.torn_holes, 1);
        assert_eq!(p.torn_hole_samples, 30);
        assert_eq!(p.oldest_index, 0, "ring must NOT be wiped by a hole");
        assert_eq!(p.newest_index(), 140);

        // A trigger for a pre-hole event still captures out of retained history
        // (win [20,120): newest 140 ≥ win_end → emits on the trigger tick).
        let t = trigger_frame(1, (50.0 / FS * 1e9) as i64);
        let (h, vals) = step(&mut p, None, Some(&t)).expect("capture emitted");
        assert_eq!(p.captures, 1);
        assert_eq!(h.sample_index, 20);
        let row = CH + 1;
        let ch0: Vec<f32> = vals.iter().step_by(row).copied().collect();
        assert_eq!(ch0[0], 20.0); // pre-hole history intact
        assert_eq!(ch0[79], 99.0); // last real sample before the hole
        assert_eq!(ch0[80], 99.0, "hole region repeats the last value");
        assert_eq!(ch0[99], 99.0);

        // A jump beyond the fill bound (1s = FS samples) is a real break.
        let f2 = signal_frame(10_000, 10);
        assert!(step(&mut p, Some(&f2), None).is_none());
        assert_eq!(p.oldest_index, 10_000, "big jump re-anchors");
        assert_eq!(p.torn_holes, 1, "big jump is not counted as a hole");
    }

    #[test]
    fn holdoff_drops_triggers_during_pending() {
        let mut p = new_plugin(100, 0.3, 1000.0);
        feed(&mut p, 0, 10, 10);
        let t1 = trigger_frame(1, (50.0 / FS * 1e9) as i64);
        assert!(step(&mut p, None, Some(&t1)).is_none()); // pending
        let t2 = trigger_frame(2, (55.0 / FS * 1e9) as i64);
        assert!(step(&mut p, None, Some(&t2)).is_none()); // dropped (holdoff)
        assert_eq!(p.dropped_triggers, 1);
    }
}
