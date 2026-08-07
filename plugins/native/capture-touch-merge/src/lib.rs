//! Capture↔touch merge plugin: attach the touch coordinate of a tap to its
//! vibration capture, as a frame annotation.
//!
//! This is the **explicit alignment node** of the touch pipeline. The two
//! inputs are role-distinct (not a bus): `capture` (the vibration window from
//! `trigger-capture`, streamed as several chunks per tap, the first flagged
//! discontinuity) and `touch_event` (sporadic first-MOVE events from
//! `touch_tracker`). The touch↔capture correspondence is a *coarse temporal
//! association* — which tap, not sample-level alignment — so we pair by time
//! proximity.
//!
//! Why a delay buffer (the key correctness fix): in the field the vibration
//! capture chunks reach this node ~20–50 ms *before* the matching touch event
//! (the touch path has more transport latency). Pairing at capture-arrival
//! instant therefore misses ~half the taps — the touch event for that tap is
//! still in flight and not yet in the ring. So captures are held in a FIFO and
//! released only after `pair_delay_ms`, by which time the touch event has
//! arrived and sits in the ring. Releasing oldest-first keeps each tap's chunks
//! contiguous (so `storage` still segments on the first chunk's discontinuity).
//!
//! Shell constraints shaping this: a consumer runs only when input arrives and
//! emits one frame per output port per call, and the shell sizes the output
//! buffer to *this tick's* inputs. So a held capture is released only on a tick
//! that itself carries a capture (the output is then sized for a capture) and
//! that can hold the chunk. Net effect: each tap's capture is written one tap
//! later, matched reliably; the very last tap of a session is held until the
//! next capture arrives.
//!
//! Output: the capture frame, unchanged (header — including the discontinuity
//! flag — and samples), plus a trailing JSON annotation
//! `{"matched":bool,"dt_ms":f,"points":[<event json>]}`. The sink stays dumb.

use std::collections::VecDeque;

use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

/// Self-describing id for the touch-coordinate annotation (JSON-encoded).
pub const TOUCH_ANNOTATION_SCHEMA: u64 = 0x7331_6463_726f_6f63; // "coordc1s" LE-ish tag

/// How many recent touch events to keep for pairing. Taps are human-paced, so a
/// handful is ample.
const RING_CAP: usize = 16;

/// Safety cap on held captures (one tap's worth of chunks is normal; this only
/// trips if draining stalls).
const MAX_PENDING: usize = 512;

struct TouchRec {
    t0_ns: i64,
    json: Vec<u8>,
}

struct PendingCap {
    arrival_ns: i64,
    header: sigflow_plugin_sdk::FrameHeader,
    samples: Vec<u8>,
}

pub struct CaptureTouchMerge {
    window_ns: i64,
    pair_delay_ns: i64,
    /// 按帧 t0 配对（|capture.t0 − touch.t0| ≤ window）而非捕获到达时刻。
    /// 适用于两侧 t0 同锚 host 单调钟的拓扑（如 SDC 链，且上游 pair-gate
    /// 已保证成组送达）：到达时刻含捕获链 0.3-0.5s 的迟到抖动，密集敲击
    /// 时会把邻敲触控关联进来；t0 配对则间隔 > 窗口即永远正确。
    match_by_t0: bool,
    ring: Vec<TouchRec>,
    pending: VecDeque<PendingCap>,
    /// Per-flush detail logging (noisy; for debugging).
    verbose: bool,
    /// Rolling counters for the periodic match-rate summary.
    flushes: u64,
    matches: u64,
}

/// Emit a match-rate summary line every this many flushed captures.
const SUMMARY_EVERY: u64 = 50;

impl CaptureTouchMerge {
    /// Best touch event for a capture received at host-clock `ref_ns`: closest
    /// in time, within the pairing window. Returns `(index, dt_ns)`.
    ///
    /// `ref_ns` is the capture's **arrival** time at this node (real
    /// `CLOCK_MONOTONIC`), NOT the frame's `t0_ns`. The latter is extrapolated
    /// from the device sample axis (anchor + sample_index/fs) and drifts vs the
    /// host clock over a session (~ms/s), so pairing it against the touch
    /// event's real-clock arrival would slowly walk out of the window. Both
    /// `ref_ns` and the touch `t0_ns` are real host arrival times → their
    /// difference is a bounded path-latency delta, not a drifting one.
    fn best_match(&self, ref_ns: i64) -> Option<(usize, i64)> {
        self.ring
            .iter()
            .enumerate()
            .map(|(i, r)| (i, r.t0_ns - ref_ns))
            .filter(|(_, dt)| dt.abs() <= self.window_ns)
            .min_by_key(|(_, dt)| dt.abs())
    }

    fn annotation_for(&self, ref_ns: i64) -> String {
        match self.best_match(ref_ns) {
            Some((i, dt)) => {
                let dt_ms = dt as f64 / 1e6;
                let pt = std::str::from_utf8(&self.ring[i].json).unwrap_or("{}");
                format!(r#"{{"matched":true,"dt_ms":{dt_ms},"points":[{pt}]}}"#)
            }
            None => r#"{"matched":false}"#.to_string(),
        }
    }
}

impl Plugin for CaptureTouchMerge {
    fn new(_manifest: &PluginManifest) -> Self {
        CaptureTouchMerge {
            window_ns: 100_000_000,    // 100 ms pairing window
            pair_delay_ns: 80_000_000, // 80 ms hold before release
            match_by_t0: false,
            ring: Vec::new(),
            pending: VecDeque::new(),
            verbose: false,
            flushes: 0,
            matches: 0,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("window_ms", ParamValue::F64(v)) => self.window_ns = (v.clamp(1.0, 1000.0) * 1e6) as i64,
            ("pair_delay_ms", ParamValue::F64(v)) => {
                self.pair_delay_ns = (v.clamp(0.0, 2000.0) * 1e6) as i64
            }
            ("match_by_t0", ParamValue::Bool(b)) => self.match_by_t0 = *b,
            ("verbose", ParamValue::Bool(b)) => self.verbose = *b,
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        self.ring.clear();
        self.pending.clear();
        ProcessOutcome::Ok
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        // Port order follows the manifest: 0 = capture, 1 = touch_event.
        // Record any touch event first so a slightly-later capture can match it.
        if let Some(te) = inputs.get(1) {
            let data = te.samples();
            if !data.is_empty() {
                if self.verbose {
                    eprintln!(
                        "capture_touch_merge: rx touch_event t0_ns={} ({} bytes), ring={}",
                        te.header.t0_ns,
                        data.len(),
                        self.ring.len() + 1
                    );
                }
                self.ring.push(TouchRec {
                    t0_ns: te.header.t0_ns,
                    json: data.to_vec(),
                });
                if self.ring.len() > RING_CAP {
                    let overflow = self.ring.len() - RING_CAP;
                    self.ring.drain(..overflow);
                }
            }
        }

        // Buffer this tick's capture chunk (if any).
        let has_capture = match inputs.first() {
            Some(cap) if !cap.samples().is_empty() => {
                self.pending.push_back(PendingCap {
                    arrival_ns: mono_ns(),
                    header: cap.header,
                    samples: cap.samples().to_vec(),
                });
                true
            }
            _ => false,
        };

        // Release the oldest held chunk, oldest-first, only on a capture-bearing
        // tick (so the shell sized this output for a capture) and only once it
        // has aged past pair_delay (touch had time to arrive). Skip if it would
        // not fit this output buffer.
        if !has_capture {
            return ProcessOutcome::Ok;
        }
        let Some(out) = outputs.first_mut() else {
            return ProcessOutcome::Ok;
        };
        let now = mono_ns();
        let cap_bytes = out.capacity();
        let ready = self.pending.front().is_some_and(|p| {
            (now - p.arrival_ns >= self.pair_delay_ns || self.pending.len() > MAX_PENDING)
                && p.samples.len() <= cap_bytes
        });
        if !ready {
            return ProcessOutcome::Ok;
        }
        let p = self.pending.pop_front().unwrap();
        // Default: pair on the capture's real host arrival time (not its
        // extrapolated, possibly drifting frame t0_ns) — see best_match.
        // match_by_t0: both sides share the host-monotonic anchor, pair on
        // frame time (immune to capture-path lateness jitter).
        let ref_ns = if self.match_by_t0 { p.header.t0_ns } else { p.arrival_ns };
        let annotation = self.annotation_for(ref_ns);

        // Counters + periodic match-rate summary (always on, low-rate).
        self.flushes += 1;
        let matched = annotation.contains(r#""matched":true"#);
        if matched {
            self.matches += 1;
        }
        if self.verbose {
            eprintln!(
                "capture_touch_merge: flush capture t0_ns={} held_ms={} ring={} pending={} -> {}",
                p.header.t0_ns,
                (now - p.arrival_ns) / 1_000_000,
                self.ring.len(),
                self.pending.len(),
                annotation
            );
        }
        if self.flushes % SUMMARY_EVERY == 0 {
            eprintln!(
                "capture_touch_merge: {}/{} captures matched a touch ({}%)",
                self.matches,
                self.flushes,
                self.matches * 100 / self.flushes
            );
        }
        // Preserve the capture header verbatim (incl. discontinuity flag) so the
        // sink segments unchanged, then graft the annotation on.
        out.header = p.header;
        out.write(&p.samples);
        if !out.set_annotation(TOUCH_ANNOTATION_SCHEMA, annotation.as_bytes()) {
            eprintln!("capture_touch_merge: annotation didn't fit; emitting capture unannotated");
        }
        ProcessOutcome::Ok
    }
}

sigflow_plugin_sdk::export_plugin!(CaptureTouchMerge);

#[cfg(test)]
mod tests {
    use super::*;
    use sigflow_plugin_sdk::{parse_annotation, FrameHeader};

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    fn merge() -> CaptureTouchMerge {
        let mut m = CaptureTouchMerge::new(&manifest());
        m.pair_delay_ns = 0; // tests release immediately (no wall-clock wait)
        m
    }

    fn touch_frame(t0_ns: i64, json: &str) -> (FrameHeader, Vec<u8>) {
        let mut h = FrameHeader::ZERO;
        h.t0_ns = t0_ns;
        (h, json.as_bytes().to_vec())
    }

    fn capture_frame(t0_ns: i64, n_ch: usize, n_samp: usize) -> (FrameHeader, Vec<u8>) {
        let mut h = FrameHeader::ZERO;
        h.t0_ns = t0_ns;
        h.n_samples = n_samp as u32;
        (h, vec![0u8; n_ch * n_samp * 4])
    }

    fn run(
        m: &mut CaptureTouchMerge,
        capture: Option<&(FrameHeader, Vec<u8>)>,
        touch: Option<&(FrameHeader, Vec<u8>)>,
    ) -> Option<(FrameHeader, Vec<u8>)> {
        let empty = (FrameHeader::ZERO, Vec::<u8>::new());
        let cap = capture.unwrap_or(&empty);
        let tch = touch.unwrap_or(&empty);
        let inputs = [Frame::new(cap.0, &cap.1), Frame::new(tch.0, &tch.1)];
        let mut buf = vec![0u8; 8192];
        let mut outs = [FrameOut::new(&mut buf)];
        let _ = m.process(&inputs, &mut outs);
        let w = outs[0].written();
        if w == 0 {
            return None;
        }
        Some((outs[0].header, outs[0].buffer_mut()[..w].to_vec()))
    }

    #[test]
    fn manifest_has_two_consumers_and_a_producer() {
        let m = manifest();
        assert_eq!(m.ports.len(), 3);
        assert_eq!(m.ports[0].id, "capture");
        assert_eq!(m.ports[1].id, "touch_event");
        assert_eq!(m.ports[2].id, "out");
    }

    fn push_touch(m: &mut CaptureTouchMerge, t0_ns: i64, json: &str) {
        m.ring.push(TouchRec {
            t0_ns,
            json: json.as_bytes().to_vec(),
        });
    }

    // Pairing is tested directly on `annotation_for` with an explicit host-clock
    // reference (the real-arrival time used in production), since `process()`
    // pairs on `mono_ns()` which a unit test can't pin.
    #[test]
    fn pairs_nearest_touch_within_window() {
        let mut m = merge();
        push_touch(
            &mut m,
            1_000_000,
            r#"{"contact_id":0,"x":640,"y":480,"depth":100,"raw_state":7,"touch_ts":7}"#,
        );
        let s = m.annotation_for(1_030_000); // capture arrived 30 us after the touch
        assert!(s.contains(r#""matched":true"#), "got {s}");
        assert!(s.contains(r#""x":640"#));
    }

    #[test]
    fn process_emits_capture_with_annotation_and_preserves_samples() {
        let mut m = merge();
        let cap = capture_frame(1_030_000, 2, 4);
        let (h, payload) = run(&mut m, Some(&cap), None).expect("emits");
        assert!(h.is_annotated());
        let (samples, ann) = h.split_payload(&payload);
        assert_eq!(samples, &cap.1[..]); // capture samples preserved verbatim
        let (schema, _) = parse_annotation(ann.unwrap()).unwrap();
        assert_eq!(schema, TOUCH_ANNOTATION_SCHEMA);
    }

    #[test]
    fn capture_is_held_until_delay() {
        let mut m = merge();
        m.pair_delay_ns = 10_000_000_000; // 10 s — never reached within the test
        let cap = capture_frame(1000, 1, 4);
        assert!(run(&mut m, Some(&cap), None).is_none()); // held, not emitted
        assert_eq!(m.pending.len(), 1);
    }

    #[test]
    fn preserves_discontinuity_flag_for_segmenting() {
        let mut m = merge();
        let mut cap = capture_frame(5_000, 1, 8);
        cap.0.set_discontinuity(0);
        let (h, _payload) = run(&mut m, Some(&cap), None).expect("emits");
        assert!(h.is_discontinuity());
        assert!(h.is_annotated());
    }

    #[test]
    fn unmatched_when_no_touch_in_window() {
        let mut m = merge();
        push_touch(&mut m, 0, r#"{"x":1}"#);
        let s = m.annotation_for(500_000_000); // 500 ms away — outside window
        assert!(s.contains(r#""matched":false"#));
    }

    #[test]
    fn chunks_release_oldest_first_preserving_order() {
        // Two captures buffered (delay 0 releases one per capture-tick, oldest
        // first) — verify FIFO order so a sink segments contiguously.
        let mut m = merge();
        let c1 = capture_frame(1000, 1, 4);
        let c2 = capture_frame(2000, 1, 4);
        let (h1, _) = run(&mut m, Some(&c1), None).expect("c1");
        let (h2, _) = run(&mut m, Some(&c2), None).expect("c2");
        assert_eq!(h1.t0_ns, 1000);
        assert_eq!(h2.t0_ns, 2000);
    }

    #[test]
    fn no_capture_emits_nothing() {
        let mut m = merge();
        let touch = touch_frame(1, r#"{"x":1}"#);
        assert!(run(&mut m, None, Some(&touch)).is_none());
    }

    #[test]
    fn match_by_t0_pairs_on_frame_time_not_arrival() {
        // 环里有两个触控：一个 t0 靠近捕获帧的 t0（正确的那次敲击），
        // 一个 t0 靠近"现在"（邻敲的触控，到达时刻配对会错选它）。
        let mut m = merge();
        m.match_by_t0 = true;
        let cap_t0 = 5_000_000_000i64;
        push_touch(&mut m, cap_t0 + 50_000_000, r#"{"x":111}"#); // 正确：+50ms
        push_touch(&mut m, mono_ns(), r#"{"x":999}"#); // 邻敲：贴着当前时刻
        let cap = capture_frame(cap_t0, 1, 4);
        let (h, payload) = run(&mut m, Some(&cap), None).expect("emits");
        let (_, ann) = h.split_payload(&payload);
        let (_, data) = parse_annotation(ann.unwrap()).unwrap();
        let s = std::str::from_utf8(data).unwrap();
        assert!(s.contains(r#""x":111"#), "t0 matching must pick the co-timed touch, got {s}");
        assert!(!s.contains(r#""x":999"#));
    }

    #[test]
    fn closest_of_several_touches_wins() {
        let mut m = merge();
        push_touch(&mut m, 1_000_000, r#"{"x":1}"#);
        push_touch(&mut m, 2_000_000, r#"{"x":2}"#);
        push_touch(&mut m, 3_000_000, r#"{"x":3}"#);
        let s = m.annotation_for(2_010_000);
        assert!(s.contains(r#""x":2"#), "got {s}");
    }
}
