//! Reference passthrough plugin: copies input to output, applying a gain factor.
//!
//! Interprets data as little-endian `i16` samples and multiplies each by the
//! `gain` parameter (default 1.0). Also tracks a frame counter that can be
//! reset via the `reset` action.
//!
//! The plugin's contract (ports, parameters, actions) lives in `manifest.toml`
//! next to the compiled dylib. The shell parses that file and hands the
//! manifest to `Plugin::new`; this code does not declare its own.

use sigflow_plugin_sdk::{Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome};

pub struct Passthrough {
    enabled: bool,
    gain: f64,
    frame_count: u64,
}

impl Plugin for Passthrough {
    fn new(_manifest: &PluginManifest) -> Self {
        // The manifest declares enabled.default=true and gain.default=1.0;
        // mirror those here. (The shell pushes set_param for any saved
        // values after construction, so these are just the cold-start
        // defaults.)
        Passthrough {
            enabled: true,
            gain: 1.0,
            frame_count: 0,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match id {
            "enabled" => {
                if let ParamValue::Bool(v) = value {
                    self.enabled = *v;
                }
            }
            "gain" => {
                if let ParamValue::F64(v) = value {
                    self.gain = v.clamp(0.0, 10.0);
                }
            }
            _ => {}
        }
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        self.frame_count += 1;

        if let (Some(input), Some(output)) = (inputs.first(), outputs.first_mut()) {
            // Passthrough preserves the input frame's metadata (timestamp,
            // sample_index, discontinuity flags) on the way out.
            output.header = input.header;
            let in_data = input.data;
            let cap = output.capacity();

            if !self.enabled {
                output.buffer_mut()[..cap].fill(0);
                output.set_written(cap);
                return ProcessOutcome::Ok;
            }

            let n = in_data.len().min(cap);
            let sample_count = n / 2;
            let buf = output.buffer_mut();
            for i in 0..sample_count {
                let offset = i * 2;
                let sample = i16::from_le_bytes([in_data[offset], in_data[offset + 1]]);
                let gained = (sample as f64 * self.gain).round();
                let clamped = gained.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                let bytes = clamped.to_le_bytes();
                buf[offset] = bytes[0];
                buf[offset + 1] = bytes[1];
            }

            let processed_bytes = sample_count * 2;
            let remaining = n - processed_bytes;
            if remaining > 0 {
                buf[processed_bytes..processed_bytes + remaining]
                    .copy_from_slice(&in_data[processed_bytes..processed_bytes + remaining]);
            }
            output.set_written(n);
        }
        ProcessOutcome::Ok
    }

    fn invoke_action(&mut self, id: &str) {
        if id == "reset" {
            self.frame_count = 0;
        }
    }
}

sigflow_plugin_sdk::export_plugin!(Passthrough);

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal manifest for unit tests. In production the shell
    /// parses `manifest.toml` on install — here we just need a placeholder
    /// to satisfy `Plugin::new`.
    fn dummy_manifest() -> PluginManifest {
        let toml = include_str!("../manifest.toml");
        toml::from_str(toml).expect("manifest.toml should parse")
    }

    /// Drive `process` with one input/output frame; returns bytes written.
    fn run(p: &mut Passthrough, input: &[u8], output: &mut [u8]) -> usize {
        use sigflow_plugin_sdk::FrameHeader;
        let inputs = [Frame::new(FrameHeader::ZERO, input)];
        let mut outs = [FrameOut::new(output)];
        p.process(&inputs, &mut outs);
        outs[0].written()
    }

    #[test]
    fn manifest_toml_parses_with_expected_shape() {
        let m = dummy_manifest();
        assert_eq!(m.name, "sigflow.core.passthrough");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.ports.len(), 2);
        assert_eq!(m.parameters.len(), 2);
        assert_eq!(m.actions.len(), 1);
        assert_eq!(m.actions[0].id, "reset");
    }

    #[test]
    fn process_passthrough_with_gain_1() {
        let mut p = Passthrough::new(&dummy_manifest());
        let sample1 = 100_i16.to_le_bytes();
        let sample2 = (-200_i16).to_le_bytes();
        let input = [sample1[0], sample1[1], sample2[0], sample2[1]];
        let mut output = [0u8; 4];

        {
            run(&mut p, &input, &mut output);
        }

        let out1 = i16::from_le_bytes([output[0], output[1]]);
        let out2 = i16::from_le_bytes([output[2], output[3]]);
        assert_eq!(out1, 100);
        assert_eq!(out2, -200);
    }

    #[test]
    fn process_applies_gain() {
        let mut p = Passthrough::new(&dummy_manifest());
        p.set_param("gain", &ParamValue::F64(2.0));

        let sample = 100_i16.to_le_bytes();
        let input = [sample[0], sample[1]];
        let mut output = [0u8; 2];

        {
            run(&mut p, &input, &mut output);
        }

        let result = i16::from_le_bytes([output[0], output[1]]);
        assert_eq!(result, 200);
    }

    #[test]
    fn process_clamps_on_overflow() {
        let mut p = Passthrough::new(&dummy_manifest());
        p.set_param("gain", &ParamValue::F64(10.0));

        let sample = 10000_i16.to_le_bytes();
        let input = [sample[0], sample[1]];
        let mut output = [0u8; 2];

        {
            run(&mut p, &input, &mut output);
        }

        let result = i16::from_le_bytes([output[0], output[1]]);
        assert_eq!(result, i16::MAX);
    }

    #[test]
    fn disabled_outputs_silence() {
        let mut p = Passthrough::new(&dummy_manifest());
        p.set_param("enabled", &ParamValue::Bool(false));

        let input = [0xFF_u8; 4];
        let mut output = [0xFF_u8; 4];

        {
            run(&mut p, &input, &mut output);
        }

        assert_eq!(output, [0, 0, 0, 0]);
    }

    #[test]
    fn frame_counter_increments_and_resets() {
        let mut p = Passthrough::new(&dummy_manifest());
        assert_eq!(p.frame_count, 0);

        let input = [0u8; 2];
        let mut output = [0u8; 2];
        for _ in 0..5 {
            run(&mut p, &input, &mut output);
        }
        assert_eq!(p.frame_count, 5);

        p.invoke_action("reset");
        assert_eq!(p.frame_count, 0);
    }

    #[test]
    fn gain_clamped_to_range() {
        let mut p = Passthrough::new(&dummy_manifest());
        p.set_param("gain", &ParamValue::F64(99.0));
        assert_eq!(p.gain, 10.0);

        p.set_param("gain", &ParamValue::F64(-5.0));
        assert_eq!(p.gain, 0.0);
    }
}
