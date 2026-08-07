//! Peak-finder: first-peak event extraction from windowed capture streams.
//!
//! `capture` (consumer) carries a windowed multichannel interleaved f32 stream
//! in the capture convention (a window opens with `FLAG_DISCONTINUITY` and its
//! chunks advance `sample_index` contiguously — sdc-usb's `pen` port or a
//! trigger-capture `capture` output). The plugin reassembles one channel of
//! each window and finds the **first peak**: the first local maximum of
//! |x − DC| after the amplitude first exceeds
//! `max(threshold, sigma_mult · σ_noise)`, where DC/σ come from the window's
//! leading `noise_samples` (they must lie inside the upstream pre-trigger
//! region).
//!
//! `trigger` (producer) emits one header-only marker frame per window:
//! `t0_ns` = the peak instant, computed as window-start `t0` + index/fs. With
//! `anchor = hw_edge` the event is instead the upstream marker column's
//! 0→high step (the trailing channel, e.g. sdc-usb pen's hardware-trigger
//! marker) — waveform-independent, so saturated or shapeless windows still
//! emit a deterministic instant. The
//! marker deliberately does **not** set `FLAG_TRIG_INDEXED` — its
//! `sample_index` is on this stream's own axis (informational), not the paired
//! signal stream's, so a downstream trigger-capture aligns via the `t0` time
//! path. fs starts from the `fs_hz` prior and is replaced by the rate inferred
//! from frame headers (`Δsample_index / Δt0`) as soon as two frames are seen —
//! sdc-usb's pen virtual axis makes this work even across single-frame windows.
//!
//! Torn windows (a `sample_index` hole between chunks, latest-wins drops) are
//! discarded with a counter; the next `FLAG_DISCONTINUITY` opens fresh.

use sigflow_plugin_sdk::{
    Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

const F32: usize = 4;

/// What instant the emitted event marks.
#[derive(Clone, Copy, PartialEq)]
enum Anchor {
    /// First peak of |x − dc| after threshold onset (detection).
    Peak,
    /// The upstream marker column's 0→high step: the hardware trigger sample.
    HwEdge,
}

/// Which excursion direction the peak search scores.
///
/// `Abs` ties trough against crest on symmetric waveforms (|x − dc| nearly
/// equal on both), so noise decides the argmax and the anchor hops half a
/// cycle between windows. When the first swing's direction is known (a pen
/// knock dip, a falling-first injection), a signed polarity pins it.
#[derive(Clone, Copy, PartialEq)]
enum Polarity {
    /// |x − dc|: either direction (default, historical behavior).
    Abs,
    /// dc − x: negative-going swing only.
    Negative,
    /// x − dc: positive-going swing only.
    Positive,
}

struct Segment {
    /// Absolute t0 of the window's first sample.
    t0_ns: i64,
    /// Stream-axis index of the window's first sample.
    start_index: u64,
    /// Expected `sample_index` of the next chunk (continuity check).
    expected_index: u64,
    /// Selected-channel samples accumulated so far.
    rows: Vec<f32>,
    /// Window-relative row of the marker column's first sample > 1.0 (the
    /// hardware trigger position), if a trailing marker channel exists.
    hw_edge_row: Option<usize>,
}

pub struct PeakFinder {
    // --- Parameters ---
    channel: usize,
    anchor: Anchor,
    peak_polarity: Polarity,
    window_samples: usize,
    threshold: f64,
    sigma_mult: f64,
    noise_samples: usize,
    onset_run: usize,
    peak_search_samples: usize,
    fs_hz: f64,
    verbose: bool,

    // --- Stream state ---
    /// (sample_index, t0_ns) of the previous data frame, for rate inference.
    prev_hdr: Option<(u64, i64)>,
    /// Header-inferred sample rate; 0.0 until two usable frames were seen.
    fs_est: f64,
    seg: Option<Segment>,
    warned_channel: bool,
    warned_no_marker: bool,

    // --- Counters ---
    events: u64,
    windows: u64,
    dropped: u64,
    quiet: u64,
}

impl PeakFinder {
    fn fs(&self) -> f64 {
        if self.fs_est > 0.0 {
            self.fs_est
        } else {
            self.fs_hz
        }
    }

    /// Window analysis: DC/σ from the leading noise region, effective
    /// threshold, and the first peak of |x − dc|.
    ///
    /// Onset = first run of `onset_run` **consecutive** samples at/above the
    /// threshold (a single-sample spike cannot fire it). Peak = the maximum
    /// of |x − dc| within `peak_search_samples` after the onset (first-wins
    /// on ties). A naive "climb to the first local maximum" is deliberately
    /// avoided: high-frequency jitter riding the rising edge creates micro
    /// peaks that end the climb early — argmax over a bounded search window
    /// is immune to that and matches the physical intent ("the first big
    /// swing of the knock"). Peak is None if the window never crosses the
    /// threshold.
    fn analyze(&self, rows: &[f32]) -> (Option<usize>, f64, f64) {
        let k = self.noise_samples.clamp(8, rows.len() / 2);
        let dc = rows[..k].iter().map(|&v| v as f64).sum::<f64>() / k as f64;
        let var = rows[..k]
            .iter()
            .map(|&v| {
                let d = v as f64 - dc;
                d * d
            })
            .sum::<f64>()
            / k as f64;
        let th = self.threshold.max(self.sigma_mult * var.sqrt());
        let amp = |i: usize| {
            let d = rows[i] as f64 - dc;
            match self.peak_polarity {
                Polarity::Abs => d.abs(),
                Polarity::Negative => -d,
                Polarity::Positive => d,
            }
        };

        let run = self.onset_run.max(1);
        let mut onset = None;
        let mut streak = 0usize;
        for i in 0..rows.len() {
            if amp(i) >= th {
                streak += 1;
                if streak >= run {
                    onset = Some(i + 1 - run);
                    break;
                }
            } else {
                streak = 0;
            }
        }
        let peak = onset.map(|o| {
            let end = (o + self.peak_search_samples.max(1)).min(rows.len());
            let mut best = o;
            for i in o..end {
                if amp(i) > amp(best) {
                    best = i;
                }
            }
            best
        });
        (peak, dc, th)
    }

    /// Emit the detection view: interleaved 4-column window
    /// [signal, peak marker (0 → peak amplitude step), dc+th, dc−th],
    /// decimated to fit one frame. Emitted for every window (quiet ones carry
    /// an all-zero marker column) so thresholds can be tuned visually.
    fn emit_view(&self, view: &mut FrameOut, seg: &Segment, peak: Option<usize>, dc: f64, th: f64) {
        const COLS: usize = 4;
        let n = self.window_samples;
        let max_rows = (view.capacity() / F32 / COLS).max(1);
        let step = n.div_ceil(max_rows);
        let mark_level = peak.map(|p| seg.rows[p]).unwrap_or(0.0);
        let hi = (dc + th) as f32;
        let lo = (dc - th) as f32;
        let mut bytes = Vec::with_capacity(n.div_ceil(step) * COLS * F32);
        let mut out_rows = 0u32;
        let mut i = 0;
        while i < n {
            let mark = match peak {
                Some(p) if i >= p => mark_level,
                _ => 0.0,
            };
            for v in [seg.rows[i], mark, hi, lo] {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            out_rows += 1;
            i += step;
        }
        view.write(&bytes);
        view.header.seq = self.windows;
        view.header.sample_index = seg.start_index;
        view.header.t0_ns = seg.t0_ns;
        view.header.n_samples = out_rows;
        view.header.flags = 0;
        view.header.set_discontinuity(0);
    }

    /// Finalize the open segment: pick the event instant per `anchor`, emit
    /// the trigger marker and the detection view.
    fn finalize(&mut self, out: &mut FrameOut, view: Option<&mut FrameOut>) {
        let seg = match self.seg.take() {
            Some(s) => s,
            None => return,
        };
        let (peak, dc, th) = self.analyze(&seg.rows[..self.window_samples]);
        let event = match self.anchor {
            Anchor::Peak => peak,
            Anchor::HwEdge => {
                if seg.hw_edge_row.is_none() && !self.warned_no_marker {
                    self.warned_no_marker = true;
                    eprintln!(
                        "peak-finder: anchor=hw_edge but the window has no trailing \
                         marker-column 0→high step — no event (needs ≥2 channels)"
                    );
                }
                seg.hw_edge_row
            }
        };
        if let Some(view) = view {
            self.emit_view(view, &seg, event, dc, th);
        }
        self.windows += 1;
        match event {
            Some(p) => {
                let t_ev = seg.t0_ns + ((p as f64) * 1e9 / self.fs()).round() as i64;
                out.header.seq = self.events;
                out.header.sample_index = seg.start_index + p as u64;
                out.header.t0_ns = t_ev;
                out.header.n_samples = 0;
                out.set_written(0);
                self.events += 1;
                if self.verbose {
                    let label = match self.anchor {
                        Anchor::Peak => "peak",
                        Anchor::HwEdge => "hw_edge",
                    };
                    eprintln!(
                        "peak-finder: window {} {label}@{} t={}ns amp={:+.3}V th={:.3}V (event #{})",
                        self.windows,
                        p,
                        t_ev,
                        seg.rows[p] as f64 - dc,
                        th,
                        self.events
                    );
                }
            }
            None => {
                self.quiet += 1;
                if self.verbose {
                    eprintln!(
                        "peak-finder: window {} below threshold {:.3}V — no event (quiet={})",
                        self.windows, th, self.quiet
                    );
                }
            }
        }
    }

    fn drop_seg(&mut self, why: &str) {
        if self.seg.take().is_some() {
            self.dropped += 1;
            eprintln!(
                "peak-finder: dropped incomplete window ({why}, dropped={})",
                self.dropped
            );
        }
    }

    fn ingest(&mut self, frame: &Frame, out: &mut FrameOut, view: Option<&mut FrameOut>) {
        let h = frame.header;
        let n = h.n_samples as usize;
        let data = frame.samples();
        if n == 0 || data.len() < n * F32 {
            return;
        }
        let channels = data.len() / (n * F32);
        if self.channel >= channels {
            if !self.warned_channel {
                self.warned_channel = true;
                eprintln!(
                    "peak-finder: channel {} out of range (frame has {channels}) — idle",
                    self.channel
                );
            }
            return;
        }

        // Rate inference from consecutive frame headers (works across windows
        // when the source keeps a coherent stream axis, e.g. sdc-usb pen).
        if let Some((pidx, pt0)) = self.prev_hdr {
            if h.sample_index > pidx && h.t0_ns > pt0 {
                let cand = (h.sample_index - pidx) as f64 * 1e9 / (h.t0_ns - pt0) as f64;
                if (100.0..1e7).contains(&cand) {
                    if self.fs_est == 0.0 && self.verbose {
                        eprintln!("peak-finder: fs inferred {cand:.1}Hz (prior {})", self.fs_hz);
                    }
                    self.fs_est = cand;
                }
            }
        }
        self.prev_hdr = Some((h.sample_index, h.t0_ns));

        if h.is_discontinuity() {
            self.drop_seg("next window arrived first");
            self.seg = Some(Segment {
                t0_ns: h.t0_ns,
                start_index: h.sample_index,
                expected_index: h.sample_index,
                rows: Vec::with_capacity(self.window_samples),
                hw_edge_row: None,
            });
        } else if let Some(seg) = &self.seg {
            if h.sample_index != seg.expected_index {
                self.drop_seg("sample_index hole");
                return;
            }
        } else {
            return; // mid-window join: wait for the next window boundary
        }

        let seg = self.seg.as_mut().expect("segment open");
        for row in 0..n {
            // Hardware-trigger position: first marker-column sample > 1.0
            // (the trailing channel, e.g. sdc-usb pen's 0→3.3 V step).
            if channels >= 2 && seg.hw_edge_row.is_none() {
                let moff = (row * channels + (channels - 1)) * F32;
                let m = f32::from_le_bytes([
                    data[moff],
                    data[moff + 1],
                    data[moff + 2],
                    data[moff + 3],
                ]);
                if m > 1.0 {
                    seg.hw_edge_row = Some(seg.rows.len());
                }
            }
            let off = (row * channels + self.channel) * F32;
            let v = f32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            seg.rows.push(v);
        }
        seg.expected_index += n as u64;

        if seg.rows.len() >= self.window_samples {
            self.finalize(out, view);
        }
    }
}

impl Plugin for PeakFinder {
    fn new(_manifest: &PluginManifest) -> Self {
        PeakFinder {
            channel: 0,
            anchor: Anchor::Peak,
            peak_polarity: Polarity::Abs,
            window_samples: 2048,
            threshold: 0.05,
            sigma_mult: 6.0,
            noise_samples: 256,
            onset_run: 3,
            peak_search_samples: 128,
            fs_hz: 10_000.0,
            verbose: false,
            prev_hdr: None,
            fs_est: 0.0,
            seg: None,
            warned_channel: false,
            warned_no_marker: false,
            events: 0,
            windows: 0,
            dropped: 0,
            quiet: 0,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("channel", ParamValue::U32(v)) => self.channel = *v as usize,
            ("anchor", ParamValue::Enum(v)) | ("anchor", ParamValue::String(v)) => {
                self.anchor = if v == "hw_edge" {
                    Anchor::HwEdge
                } else {
                    Anchor::Peak
                };
            }
            ("peak_polarity", ParamValue::Enum(v))
            | ("peak_polarity", ParamValue::String(v)) => {
                self.peak_polarity = match v.as_str() {
                    "negative" => Polarity::Negative,
                    "positive" => Polarity::Positive,
                    _ => Polarity::Abs,
                };
            }
            ("window_samples", ParamValue::U32(v)) => {
                self.window_samples = (*v as usize).clamp(16, 1_000_000);
            }
            ("threshold", ParamValue::F64(v)) => self.threshold = v.max(0.0),
            ("sigma_mult", ParamValue::F64(v)) => self.sigma_mult = v.max(0.0),
            ("noise_samples", ParamValue::U32(v)) => {
                self.noise_samples = (*v as usize).max(8);
            }
            ("onset_run", ParamValue::U32(v)) => self.onset_run = (*v as usize).max(1),
            ("peak_search_samples", ParamValue::U32(v)) => {
                self.peak_search_samples = (*v as usize).max(1);
            }
            ("fs_hz", ParamValue::F64(v)) => self.fs_hz = v.clamp(100.0, 1e7),
            ("verbose", ParamValue::Bool(v)) => self.verbose = *v,
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        // Fresh stream: forget reassembly + rate inference (counters persist).
        self.seg = None;
        self.prev_hdr = None;
        self.fs_est = 0.0;
        self.warned_channel = false;
        self.warned_no_marker = false;
        ProcessOutcome::Ok
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        if outputs.is_empty() {
            return ProcessOutcome::Ok;
        }
        // Port order matches the manifest producers: 0 = trigger, 1 = view.
        let (trig, rest) = outputs.split_at_mut(1);
        let out = &mut trig[0];
        let mut view = rest.first_mut();
        // Idle default: zero header + zero written → the shell skips the port.
        out.set_written(0);
        if let Some(v) = view.as_deref_mut() {
            v.set_written(0);
        }
        if let Some(frame) = inputs.first() {
            self.ingest(frame, out, view.as_deref_mut());
        }
        ProcessOutcome::Ok
    }

    fn invoke_action(&mut self, id: &str) {
        if id == "reset" {
            self.seg = None;
            self.prev_hdr = None;
            self.fs_est = 0.0;
        }
    }
}

sigflow_plugin_sdk::export_plugin!(PeakFinder);

#[cfg(test)]
mod tests {
    use super::*;
    use sigflow_plugin_sdk::{FrameHeader, FLAG_DISCONTINUITY};

    const FS: f64 = 10_000.0;
    const N: usize = 2048;
    const CH: usize = 2;
    const TRIG: usize = 512; // synthetic knock onset (matches 25% pretrig)

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    fn new_plugin() -> PeakFinder {
        let mut p = PeakFinder::new(&manifest());
        p.set_param("window_samples", &ParamValue::U32(N as u32));
        p.set_param("fs_hz", &ParamValue::F64(FS));
        p
    }

    /// A window whose |x−dc| first exceeds the threshold at TRIG and climbs to
    /// a strict first peak at TRIG+16, then rings down. Returns interleaved
    /// rows (signal ch0 + marker ch1) and the true peak index.
    fn window() -> (Vec<f32>, usize) {
        let peak = TRIG + 16;
        let mut rows = Vec::with_capacity(N * CH);
        for i in 0..N {
            let sig = if i < TRIG {
                1.65 + 0.001 * ((i % 7) as f32 - 3.0) // quiet DC region
            } else if i <= peak {
                1.65 + 0.5 * ((i - TRIG) as f32 + 1.0) / ((peak - TRIG) as f32 + 1.0)
            } else {
                let dt = (i - peak) as f32;
                1.65 + 0.5 * (-dt / 80.0).exp() * (0.3 * dt).cos()
            };
            rows.push(sig);
            rows.push(if i >= TRIG { 3.3 } else { 0.0 });
        }
        (rows, peak)
    }

    fn to_bytes(rows: &[f32]) -> Vec<u8> {
        rows.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn header(seq: u64, idx: u64, t0: i64, n: usize, disc: bool) -> FrameHeader {
        let mut h = FrameHeader {
            seq,
            sample_index: idx,
            t0_ns: t0,
            tx_egress_mono_ns: 0,
            gap_samples: 0,
            n_samples: n as u32,
            flags: 0,
            annotation_len: 0,
        };
        if disc {
            h.flags |= FLAG_DISCONTINUITY;
        }
        h
    }

    /// Drive one frame; return (trigger header if emitted, view frame if emitted).
    fn tick2(
        p: &mut PeakFinder,
        h: FrameHeader,
        data: &[u8],
    ) -> (Option<FrameHeader>, Option<(FrameHeader, Vec<u8>)>) {
        let mut tbuf = vec![0u8; 64];
        let mut vbuf = vec![0u8; 65536];
        let mut outs = vec![FrameOut::new(&mut tbuf), FrameOut::new(&mut vbuf)];
        let frames = [Frame::new(h, data)];
        assert!(matches!(p.process(&frames, &mut outs), ProcessOutcome::Ok));
        let trig = (outs[0].header.t0_ns != 0).then_some(outs[0].header);
        let vh = outs[1].header;
        let vw = outs[1].written();
        let view = (vw > 0).then(|| (vh, vbuf[..vw].to_vec()));
        (trig, view)
    }

    /// Drive one frame; return Some(header) if a trigger marker was emitted.
    fn tick(p: &mut PeakFinder, h: FrameHeader, data: &[u8]) -> Option<FrameHeader> {
        tick2(p, h, data).0
    }

    #[test]
    fn manifest_matches() {
        let m = manifest();
        assert_eq!(m.name, "sigflow.core.peak_finder");
        assert_eq!(m.ports.len(), 3);
    }

    #[test]
    fn view_shows_signal_marker_and_threshold_band() {
        let mut p = new_plugin();
        let (rows, peak) = window();
        let t0 = 7_000_000_000i64;
        let (trig, view) = tick2(&mut p, header(1, 300, t0, N, true), &to_bytes(&rows));
        let trig = trig.expect("trigger emitted");
        let (vh, vd) = view.expect("view emitted");
        assert_eq!(vh.n_samples as usize, N, "no decimation at default window");
        assert_eq!(vd.len(), N * 4 * 4, "4 interleaved f32 columns");
        assert_eq!(vh.sample_index, 300);
        assert_eq!(vh.t0_ns, t0);
        assert!(vh.flags & FLAG_DISCONTINUITY != 0);
        let f = |i: usize| f32::from_le_bytes(vd[i * 4..i * 4 + 4].try_into().unwrap());
        // col0 = signal passthrough
        assert_eq!(f(0), rows[0]);
        assert_eq!(f((peak * 4) + 0), rows[peak * CH]);
        // col1 = marker: 0 before the peak, steps to the peak amplitude at it
        assert_eq!(f(((peak - 1) * 4) + 1), 0.0);
        assert_eq!(f((peak * 4) + 1), rows[peak * CH]);
        assert_eq!(f(((N - 1) * 4) + 1), rows[peak * CH]);
        // marker edge position == trigger sample_index offset
        assert_eq!(trig.sample_index - 300, peak as u64);
        // col2/3 = constant threshold band bracketing the DC region
        let hi = f(2);
        let lo = f(3);
        assert_eq!(hi, f(((N - 1) * 4) + 2));
        assert_eq!(lo, f(((N - 1) * 4) + 3));
        assert!(lo < 1.65 && 1.65 < hi, "band brackets dc: {lo} .. {hi}");

        // Quiet window: view still emitted, marker column all zero.
        let quiet: Vec<f32> = (0..N * CH)
            .map(|i| if i % 2 == 0 { 1.65 } else { 0.0 })
            .collect();
        let (trig, view) = tick2(&mut p, header(2, 90_000, t0 + 5_000_000_000, N, true),
                                 &to_bytes(&quiet));
        assert!(trig.is_none());
        let (_, vd) = view.expect("quiet window still emits a view");
        let f = |i: usize| f32::from_le_bytes(vd[i * 4..i * 4 + 4].try_into().unwrap());
        for i in [0usize, N / 2, N - 1] {
            assert_eq!(f(i * 4 + 1), 0.0, "quiet marker must stay 0 at row {i}");
        }
    }

    #[test]
    fn single_frame_window_emits_first_peak() {
        let mut p = new_plugin();
        let (rows, peak) = window();
        let t0 = 1_000_000_000i64;
        let base = 50_000u64;
        let out = tick(&mut p, header(1, base, t0, N, true), &to_bytes(&rows))
            .expect("marker emitted");
        assert_eq!(out.n_samples, 0);
        assert_eq!(out.seq, 0);
        assert_eq!(out.sample_index, base + peak as u64);
        assert_eq!(out.t0_ns, t0 + ((peak as f64) * 1e9 / FS).round() as i64);
        assert_eq!(out.flags, 0, "marker must not claim TRIG_INDEXED");
    }

    #[test]
    fn chunked_window_matches_single_frame() {
        let (rows, peak) = window();
        let t0 = 2_000_000_000i64;
        let base = 9_000u64;
        let mut p = new_plugin();
        let mut got = None;
        let mut off = 0usize;
        for (i, n) in [700usize, 700, N - 1400].into_iter().enumerate() {
            let chunk = &rows[off * CH..(off + n) * CH];
            let h = header(
                10 + i as u64,
                base + off as u64,
                t0 + ((off as f64) * 1e9 / FS).round() as i64,
                n,
                i == 0,
            );
            if let Some(o) = tick(&mut p, h, &to_bytes(chunk)) {
                got = Some(o);
            }
            off += n;
        }
        let out = got.expect("marker emitted from chunked window");
        assert_eq!(out.sample_index, base + peak as u64);
        assert_eq!(out.t0_ns, t0 + ((peak as f64) * 1e9 / FS).round() as i64);
    }

    #[test]
    fn torn_window_dropped_then_recovers() {
        let (rows, peak) = window();
        let mut p = new_plugin();
        let t0 = 3_000_000_000i64;
        // First 700 rows, then a hole (skip 700), then the tail: must NOT emit.
        assert!(tick(
            &mut p,
            header(1, 0, t0, 700, true),
            &to_bytes(&rows[..700 * CH])
        )
        .is_none());
        assert!(tick(
            &mut p,
            header(2, 1400, t0 + 140_000_000, N - 1400, false),
            &to_bytes(&rows[1400 * CH..])
        )
        .is_none());
        assert_eq!(p.dropped, 1);
        // A fresh complete window recovers.
        let out = tick(&mut p, header(3, 100_000, t0 + 10_000_000_000, N, true), &to_bytes(&rows))
            .expect("recovery window emits");
        assert_eq!(out.sample_index, 100_000 + peak as u64);
    }

    #[test]
    fn hf_jitter_on_rising_edge_does_not_capture_micro_peak() {
        // Rising edge with alternating ±0.02 V jitter: every other sample is a
        // local maximum of |x−dc| — a naive climb stops at the very first one.
        // The true first peak (largest swing of the initial transient) is at
        // TRIG+16 and must win via the bounded-window argmax.
        let peak = TRIG + 16;
        let mut rows = Vec::with_capacity(N * CH);
        for i in 0..N {
            let sig = if i < TRIG {
                1.65
            } else if i <= peak {
                let j = (i - TRIG) as f32;
                let jitter = if (i - TRIG) % 2 == 0 { 0.02 } else { -0.02 };
                1.65 + 0.1 + 0.03 * j + jitter
            } else {
                let dt = (i - peak) as f32;
                1.65 + 0.6 * (-dt / 40.0).exp()
            };
            rows.push(sig);
            rows.push(0.0);
        }
        let mut p = new_plugin();
        let out = tick(&mut p, header(1, 7_000, 8_000_000_000, N, true), &to_bytes(&rows))
            .expect("marker emitted");
        assert_eq!(
            out.sample_index,
            7_000 + peak as u64,
            "peak must be the transient's largest swing, not the first micro-bump"
        );
    }

    #[test]
    fn hw_edge_anchor_emits_hardware_trigger_time() {
        // window() 的 ch1 就是标记列：0 → 3.3 阶跃在 TRIG。hw_edge 模式必须
        // 发 TRIG 时刻的事件，而不是 TRIG+16 的检出峰；且饱和/无峰形也照发。
        let mut p = new_plugin();
        p.set_param("anchor", &ParamValue::Enum("hw_edge".into()));
        let (rows, peak) = window();
        let t0 = 9_000_000_000i64;
        let base = 4_000u64;
        let (trig, view) = tick2(&mut p, header(1, base, t0, N, true), &to_bytes(&rows));
        let out = trig.expect("hw_edge event emitted");
        assert_ne!(peak, TRIG, "fixture keeps peak distinct from the hw edge");
        assert_eq!(out.sample_index, base + TRIG as u64);
        assert_eq!(out.t0_ns, t0 + ((TRIG as f64) * 1e9 / FS).round() as i64);
        // view 的标记列阶跃也在硬件触发位置
        let (_, vd) = view.expect("view emitted");
        let f = |i: usize| f32::from_le_bytes(vd[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(f(((TRIG - 1) * 4) + 1), 0.0);
        assert_ne!(f((TRIG * 4) + 1), 0.0);

        // 饱和窗（无固定峰形，检测意义上静默）也逐窗必发：
        let sat: Vec<f32> = (0..N)
            .flat_map(|i| {
                let sig = if i < TRIG { 1.65 } else { 0.2 }; // 饱和后长平台
                [sig, if i >= TRIG { 3.3 } else { 0.0 }]
            })
            .collect();
        let out = tick(&mut p, header(2, 90_000, t0 + 2_000_000_000, N, true), &to_bytes(&sat))
            .expect("saturated window still emits under hw_edge");
        assert_eq!(out.sample_index, 90_000 + TRIG as u64);

        // String 变体（CLI 路径）也接受
        p.set_param("anchor", &ParamValue::String("peak".into()));
        let (rows, peak) = window();
        let out = tick(&mut p, header(3, 200_000, t0 + 9_000_000_000, N, true), &to_bytes(&rows))
            .expect("back to peak mode");
        assert_eq!(out.sample_index, 200_000 + peak as u64);
    }

    #[test]
    fn negative_polarity_pins_trough_on_symmetric_wave() {
        // 先谷后峰的单周期正弦，波峰幅度比波谷大 2%（噪声/非对称的极端化）：
        // abs 打分下 argmax 被波峰抢走（= 实机 VOFA 偶发"波峰对上升沿"的
        // 半周期跳锚）；negative 极性锁死波谷。周期 64 样本。
        let trough = TRIG + 16;
        let crest = TRIG + 48;
        let mut rows = Vec::with_capacity(N * CH);
        for i in 0..N {
            let sig = if !(TRIG..TRIG + 64).contains(&i) {
                1.65
            } else {
                let ph = (i - TRIG) as f32 / 64.0 * std::f32::consts::TAU;
                let a = if i < TRIG + 32 { 1.0 } else { 1.02 };
                1.65 - a * ph.sin()
            };
            rows.push(sig);
            rows.push(if i >= TRIG { 3.3 } else { 0.0 });
        }
        let mut p = new_plugin();
        let out = tick(&mut p, header(1, 0, 3_000_000_000, N, true), &to_bytes(&rows))
            .expect("abs mode emits");
        assert_eq!(out.sample_index, crest as u64, "abs 打分被更大的波峰抢走");
        p.set_param("peak_polarity", &ParamValue::Enum("negative".into()));
        let out = tick(&mut p, header(2, 90_000, 8_000_000_000, N, true), &to_bytes(&rows))
            .expect("negative mode emits");
        assert_eq!(out.sample_index, 90_000 + trough as u64, "negative 锁死波谷");
    }

    #[test]
    fn hw_edge_without_marker_column_stays_quiet() {
        // 单通道帧没有标记列：hw_edge 不猜、不发事件（告警一次）。
        let mut p = new_plugin();
        p.set_param("anchor", &ParamValue::Enum("hw_edge".into()));
        let rows: Vec<f32> = (0..N)
            .map(|i| if i < TRIG { 1.65 } else { 1.65 + 0.5 } )
            .collect();
        assert!(tick(&mut p, header(1, 0, 5_000_000_000, N, true), &to_bytes(&rows)).is_none());
        assert_eq!(p.quiet, 1);
        assert_eq!(p.windows, 1);
    }

    #[test]
    fn quiet_window_emits_nothing() {
        let mut p = new_plugin();
        let rows: Vec<f32> = (0..N * CH)
            .map(|i| if i % 2 == 0 { 1.65 + 0.0005 * ((i % 11) as f32 - 5.0) } else { 0.0 })
            .collect();
        assert!(tick(&mut p, header(1, 0, 4_000_000_000, N, true), &to_bytes(&rows)).is_none());
        assert_eq!(p.quiet, 1);
        assert_eq!(p.windows, 1);
    }

    #[test]
    fn fs_inferred_across_windows_overrides_prior() {
        // Virtual stream axis at the TRUE rate 12_500 Hz; prior says 10_000.
        // Window 1 computes with the prior (no pair yet); window 2 must use the
        // inferred rate.
        let fs_true = 12_500.0;
        let (rows, peak) = window();
        let mut p = new_plugin();
        let t0_a = 5_000_000_000i64;
        let idx_a = 0u64;
        let out_a = tick(&mut p, header(1, idx_a, t0_a, N, true), &to_bytes(&rows)).unwrap();
        assert_eq!(out_a.t0_ns, t0_a + ((peak as f64) * 1e9 / FS).round() as i64);
        // Second window 1s later on the true-rate axis.
        let gap_s = 1.0f64;
        let idx_b = idx_a + N as u64 + (gap_s * fs_true) as u64;
        let t0_b = t0_a + ((idx_b - idx_a) as f64 * 1e9 / fs_true).round() as i64;
        let out_b = tick(&mut p, header(2, idx_b, t0_b, N, true), &to_bytes(&rows)).unwrap();
        let expect = t0_b + ((peak as f64) * 1e9 / fs_true).round() as i64;
        assert!(
            (out_b.t0_ns - expect).abs() <= 100,
            "inferred-rate peak time off by {}ns",
            out_b.t0_ns - expect
        );
    }

    #[test]
    fn annotated_frame_samples_are_stripped() {
        // An annotation trailer must not be interpreted as samples.
        let (rows, peak) = window();
        let mut bytes = to_bytes(&rows);
        let annot: &[u8] = b"\x01\x00\x00\x00\x00\x00\x00\x00{\"k\":1}";
        bytes.extend_from_slice(annot);
        let mut h = header(1, 0, 6_000_000_000, N, true);
        h.flags |= sigflow_plugin_sdk::FLAG_ANNOTATED;
        h.annotation_len = annot.len() as u32;
        let mut p = new_plugin();
        let out = tick(&mut p, h, &bytes).expect("marker emitted");
        assert_eq!(out.sample_index, peak as u64);
    }
}
