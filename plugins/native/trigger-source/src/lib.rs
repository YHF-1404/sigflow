//! Software trigger source: emits event markers for the trigger-capture plugin.
//!
//! A source node (no inputs). Each marker is a header-only frame (no samples)
//! whose `t0_ns` is the host monotonic time of the event — either the moment
//! the `fire` action was invoked (manual, captures the press instant) or each
//! `period_ms` boundary (auto). On idle ticks it produces nothing (a ZERO
//! header with no bytes), which the shell does not publish, so a real marker is
//! never coalesced away by last-wins on the consumer.
//!
//! Because the marker time is host monotonic — the same domain the PLL-corrected
//! ADC stream uses — the trigger-capture plugin can locate the window directly,
//! no clock-domain conversion needed.

use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

pub struct TriggerSource {
    /// Auto-fire period (ns); 0 = manual only.
    period_ns: i64,
    /// Monotonic counter over emitted markers.
    seq: u64,
    /// A manual `fire` is pending, with the captured event time.
    pending: Option<i64>,
    /// Host monotonic time of the last auto-fire (for period scheduling).
    last_auto_ns: i64,
}

impl TriggerSource {
    fn new_inner() -> Self {
        TriggerSource {
            period_ns: 0,
            seq: 0,
            pending: None,
            last_auto_ns: 0,
        }
    }

    /// Emit a marker carrying `event_t0_ns` into `out`.
    fn emit(&mut self, out: &mut FrameOut, event_t0_ns: i64) {
        out.set_written(0);
        out.header.seq = self.seq;
        out.header.sample_index = self.seq; // markers have no sample grid
        out.header.n_samples = 0;
        out.header.t0_ns = event_t0_ns;
        self.seq += 1;
    }
}

impl Plugin for TriggerSource {
    fn new(_manifest: &PluginManifest) -> Self {
        Self::new_inner()
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        if id == "period_ms" {
            if let ParamValue::U32(ms) = value {
                self.period_ns = (*ms as i64) * 1_000_000;
            }
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        self.seq = 0;
        self.pending = None;
        self.last_auto_ns = mono_ns();
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        let out = match outputs.first_mut() {
            Some(o) => o,
            None => return ProcessOutcome::Ok,
        };

        // Manual fire takes priority and carries the captured press instant.
        if let Some(t0) = self.pending.take() {
            self.emit(out, t0);
            return ProcessOutcome::Ok;
        }

        // Auto fire on each period boundary, stamped at this tick.
        if self.period_ns > 0 {
            let now = mono_ns();
            if now - self.last_auto_ns >= self.period_ns {
                self.last_auto_ns = now;
                self.emit(out, now);
                return ProcessOutcome::Ok;
            }
        }

        // Idle: leave the ZERO header with no bytes; the shell skips publishing
        // it, so it cannot overwrite a pending marker on the consumer.
        ProcessOutcome::Ok
    }

    fn invoke_action(&mut self, id: &str) {
        if id == "fire" {
            // Capture the event time now (the press instant), emitted next tick.
            self.pending = Some(mono_ns());
        }
    }
}

sigflow_plugin_sdk::export_plugin!(TriggerSource);

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    fn run(p: &mut TriggerSource) -> (i64, u64, usize) {
        let mut buf = [0u8; 16];
        let (t0, seq, written) = {
            let mut outs = [FrameOut::new(&mut buf)];
            p.process(&[], &mut outs);
            (outs[0].header.t0_ns, outs[0].header.seq, outs[0].written())
        };
        (t0, seq, written)
    }

    #[test]
    fn idle_emits_zero_header() {
        let mut p = TriggerSource::new(&manifest());
        p.start();
        let (t0, _seq, written) = run(&mut p);
        assert_eq!(t0, 0, "idle marker must have t0 == 0 (shell skips it)");
        assert_eq!(written, 0);
    }

    #[test]
    fn fire_emits_one_marker_with_event_time() {
        let mut p = TriggerSource::new(&manifest());
        p.start();
        p.invoke_action("fire");
        let (t0, seq, written) = run(&mut p);
        assert!(t0 != 0, "fired marker must carry a nonzero event time");
        assert_eq!(seq, 0);
        assert_eq!(written, 0, "marker is header-only");
        // Next tick is idle again (single-shot).
        let (t0b, _seq, _w) = run(&mut p);
        assert_eq!(t0b, 0, "fire is single-shot");
    }

    #[test]
    fn distinct_markers_have_increasing_seq() {
        let mut p = TriggerSource::new(&manifest());
        p.start();
        p.invoke_action("fire");
        let (_t0, seq0, _) = run(&mut p);
        p.invoke_action("fire");
        let (_t1, seq1, _) = run(&mut p);
        assert_eq!(seq0, 0);
        assert_eq!(seq1, 1, "each marker must change (seq, t0) so it is not deduped");
    }
}
