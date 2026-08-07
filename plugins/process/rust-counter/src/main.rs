//! Counter: a minimal subprocess (Process runtime) source plugin in pure Rust.
//!
//! Each `process()` emits `batch_size` little-endian `u32` samples counting up
//! from where the last batch left off, authoring a self-anchored frame header
//! (seq + sample_index). It exists to validate the Rust process SDK end to end
//! without any hardware — the reference for the USB ADC plugin's runtime.

use sigflow_plugin_sdk::{Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome};

struct Counter {
    next: u32,
    seq: u64,
    sample_index: u64,
    batch: usize,
    /// If > 0, return Fault once this many process() calls have been made
    /// (for testing the shell's fault/Degraded handling). 0 = never.
    fault_after: u64,
    ticks: u64,
}

impl Plugin for Counter {
    fn new(_manifest: &PluginManifest) -> Self {
        Counter {
            next: 0,
            seq: 0,
            sample_index: 0,
            batch: 8,
            fault_after: 0,
            ticks: 0,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match id {
            "batch_size" => {
                if let ParamValue::U32(n) = value {
                    self.batch = (*n as usize).clamp(1, 1024);
                }
            }
            "fault_after" => {
                if let ParamValue::U32(n) = value {
                    self.fault_after = *n as u64;
                }
            }
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        // Fresh stream.
        self.next = 0;
        self.seq = 0;
        self.sample_index = 0;
        self.ticks = 0;
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        self.ticks += 1;
        if self.fault_after > 0 && self.ticks >= self.fault_after {
            return ProcessOutcome::Fault {
                reason: format!("injected fault at tick {}", self.ticks),
            };
        }
        if let Some(out) = outputs.first_mut() {
            let n = self.batch.min(out.capacity() / 4);
            let buf = out.buffer_mut();
            for i in 0..n {
                let v = self.next + i as u32;
                buf[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
            out.set_written(n * 4);

            out.header.seq = self.seq;
            out.header.sample_index = self.sample_index;
            out.header.n_samples = n as u32;

            self.next += n as u32;
            self.seq += 1;
            self.sample_index += n as u64;
        }
        ProcessOutcome::Ok
    }
}

sigflow_plugin_sdk::process_main!(Counter, include_str!("../manifest.toml"));
