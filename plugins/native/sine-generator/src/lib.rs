//! Sine wave generator plugin: produces batches of f32 samples at a
//! configurable frequency, amplitude, and sample rate.
//!
//! This is a source node (no input ports). Each call to `process()` fills the
//! output buffer with the next `batch_size` samples of a continuous sine wave.
//!
//! Contract lives in `manifest.toml` next to the dylib.

use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

pub struct SineGenerator {
    frequency_hz: f64,
    amplitude: f64,
    sample_rate_hz: f64,
    batch_size: u32,
    phase: f64,
    /// Monotonic frame counter since stream start (set on `start()`).
    seq: u64,
    /// Absolute sample index of the next sample to emit, since stream start.
    sample_index: u64,
    /// Absolute `CLOCK_MONOTONIC` ns of sample 0, captured at `start()`. Frame
    /// `t0_ns` derives from this so timestamps share a common zero across
    /// sources and `now − t0_ns` is real latency.
    stream_t0_ns: i64,
}

impl Plugin for SineGenerator {
    fn new(_manifest: &PluginManifest) -> Self {
        SineGenerator {
            frequency_hz: 440.0,
            amplitude: 0.5,
            sample_rate_hz: 48000.0,
            batch_size: 1024,
            phase: 0.0,
            seq: 0,
            sample_index: 0,
            stream_t0_ns: 0,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match id {
            "frequency_hz" => {
                if let ParamValue::F64(v) = value {
                    self.frequency_hz = v.clamp(1.0, 20000.0);
                }
            }
            "amplitude" => {
                if let ParamValue::F64(v) = value {
                    self.amplitude = v.clamp(0.0, 1.0);
                }
            }
            "sample_rate_hz" => {
                if let ParamValue::F64(v) = value {
                    self.sample_rate_hz = v.clamp(1000.0, 192000.0);
                }
            }
            "batch_size" => {
                if let ParamValue::U32(v) = value {
                    self.batch_size = (*v).clamp(64, 4096);
                }
            }
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        // Fresh stream: reset counters and anchor sample 0 to the monotonic
        // clock. A software source has no acquisition latency, so anchoring at
        // start() is exact.
        self.seq = 0;
        self.sample_index = 0;
        self.phase = 0.0;
        self.stream_t0_ns = mono_ns();
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        if let Some(out) = outputs.first_mut() {
            // Real-time pacing: emit only up to the sample count that *should*
            // exist by now (elapsed monotonic time × sample_rate), so
            // sample_index advances at the declared rate in real time and t0
            // stays consistent with the host clock. Emitting a fixed batch every
            // tick would run far faster than sample_rate_hz, making t0 a fiction
            // that no real-clock event (e.g. a trigger) can align to.
            // batch_size caps how much catch-up happens per tick.
            let cap_samples = out.capacity() / 4;
            let target_total = if self.sample_rate_hz > 0.0 {
                (((mono_ns() - self.stream_t0_ns) as f64) / 1e9 * self.sample_rate_hz).max(0.0)
                    as u64
            } else {
                self.sample_index
            };
            let due = target_total.saturating_sub(self.sample_index) as usize;
            let samples_to_generate = due.min(self.batch_size as usize).min(cap_samples);
            if samples_to_generate == 0 {
                // Nothing due yet this tick — emit nothing (shell skips the
                // empty frame); do not advance seq/sample_index.
                return ProcessOutcome::Ok;
            }
            let buf = out.buffer_mut();
            for i in 0..samples_to_generate {
                let t = self.phase + (i as f64 / self.sample_rate_hz);
                let sample = (self.amplitude
                    * (2.0 * std::f64::consts::PI * self.frequency_hz * t).sin())
                    as f32;
                let bytes = sample.to_le_bytes();
                let offset = i * 4;
                buf[offset..offset + 4].copy_from_slice(&bytes);
            }
            out.set_written(samples_to_generate * 4);

            // Author a self-anchored header: t0 = absolute monotonic anchor of
            // sample 0 plus the offset of this frame's first sample.
            out.header.seq = self.seq;
            out.header.sample_index = self.sample_index;
            out.header.n_samples = samples_to_generate as u32;
            out.header.t0_ns = if self.sample_rate_hz > 0.0 {
                self.stream_t0_ns + ((self.sample_index as f64 / self.sample_rate_hz) * 1e9) as i64
            } else {
                self.stream_t0_ns
            };

            self.seq += 1;
            self.sample_index += samples_to_generate as u64;
            self.phase += samples_to_generate as f64 / self.sample_rate_hz;
            if self.frequency_hz > 0.0 {
                self.phase %= 1.0 / self.frequency_hz;
            }
        }
        ProcessOutcome::Ok
    }

    fn invoke_action(&mut self, id: &str) {
        if id == "reset_phase" {
            self.phase = 0.0;
        }
    }
}

sigflow_plugin_sdk::export_plugin!(SineGenerator);

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_manifest() -> PluginManifest {
        let toml = include_str!("../manifest.toml");
        toml::from_str(toml).expect("manifest.toml should parse")
    }

    /// Drive `process` on a source (no inputs) into `output`; returns written.
    fn run(gen: &mut SineGenerator, output: &mut [u8]) -> usize {
        let mut outs = [FrameOut::new(output)];
        gen.process(&[], &mut outs);
        outs[0].written()
    }

    #[test]
    fn manifest_toml_parses_with_expected_shape() {
        let m = dummy_manifest();
        assert_eq!(m.name, "sigflow.core.sine_generator");
        assert_eq!(m.ports.len(), 1);
        assert_eq!(m.ports[0].id, "signal_out");
        assert_eq!(m.parameters.len(), 4);
        assert_eq!(m.actions.len(), 1);
        assert_eq!(m.actions[0].id, "reset_phase");
    }

    #[test]
    fn process_generates_nonzero_output() {
        let mut gen = SineGenerator::new(&dummy_manifest());
        let mut output = vec![0u8; 1024 * 4];

        run(&mut gen, &mut output);

        let has_nonzero = output.iter().any(|&b| b != 0);
        assert!(has_nonzero, "output should contain non-zero samples");
    }

    #[test]
    fn process_generates_valid_sine_wave() {
        let mut gen = SineGenerator::new(&dummy_manifest());
        gen.set_param("frequency_hz", &ParamValue::F64(1000.0));
        gen.set_param("amplitude", &ParamValue::F64(1.0));
        gen.set_param("sample_rate_hz", &ParamValue::F64(48000.0));

        let num_samples = 1024_usize;
        let mut output = vec![0u8; num_samples * 4];

        run(&mut gen, &mut output);

        let samples: Vec<f32> = (0..num_samples)
            .map(|i| {
                let offset = i * 4;
                f32::from_le_bytes([
                    output[offset],
                    output[offset + 1],
                    output[offset + 2],
                    output[offset + 3],
                ])
            })
            .collect();

        assert!(
            samples[0].abs() < 0.001,
            "first sample should be ~0 (sin(0)), got {}",
            samples[0]
        );

        let quarter_idx = (48000.0 / 1000.0 / 4.0) as usize;
        assert!(
            (samples[quarter_idx] - 1.0).abs() < 0.05,
            "quarter-cycle sample should be ~1.0, got {}",
            samples[quarter_idx]
        );

        for (i, &s) in samples.iter().enumerate() {
            assert!(
                s.is_finite() && s >= -1.01 && s <= 1.01,
                "sample[{i}] = {s} is out of range"
            );
        }
    }

    #[test]
    fn reset_phase_action_resets_phase() {
        let mut gen = SineGenerator::new(&dummy_manifest());
        gen.set_param("frequency_hz", &ParamValue::F64(1000.0));
        gen.set_param("amplitude", &ParamValue::F64(1.0));
        gen.set_param("sample_rate_hz", &ParamValue::F64(48000.0));

        let num_samples = 256_usize;
        let mut output1 = vec![0u8; num_samples * 4];
        let mut output2 = vec![0u8; num_samples * 4];

        run(&mut gen, &mut output1);

        gen.invoke_action("reset_phase");

        run(&mut gen, &mut output2);

        assert_eq!(output1, output2, "output after reset should match first batch");
    }

    #[test]
    fn parameters_are_clamped_to_range() {
        let mut gen = SineGenerator::new(&dummy_manifest());

        gen.set_param("frequency_hz", &ParamValue::F64(99999.0));
        assert_eq!(gen.frequency_hz, 20000.0);

        gen.set_param("frequency_hz", &ParamValue::F64(-10.0));
        assert_eq!(gen.frequency_hz, 1.0);

        gen.set_param("amplitude", &ParamValue::F64(5.0));
        assert_eq!(gen.amplitude, 1.0);

        gen.set_param("amplitude", &ParamValue::F64(-1.0));
        assert_eq!(gen.amplitude, 0.0);

        gen.set_param("batch_size", &ParamValue::U32(10));
        assert_eq!(gen.batch_size, 64);

        gen.set_param("batch_size", &ParamValue::U32(99999));
        assert_eq!(gen.batch_size, 4096);
    }
}
