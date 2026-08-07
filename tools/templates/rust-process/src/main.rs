//! __PLUGIN_NAME__ — sigflow 子进程（Process runtime）源插件骨架。
//!
//! 每次 process() 发一批递增的 u32 样本，自锚定帧头（seq/sample_index）。
//! 把它改成你的数据源/处理逻辑；契约在 manifest.toml。

use sigflow_plugin_sdk::{Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome};

struct __PLUGIN_STRUCT__ {
    next: u32,
    seq: u64,
    sample_index: u64,
    batch: usize,
}

impl Plugin for __PLUGIN_STRUCT__ {
    fn new(_manifest: &PluginManifest) -> Self {
        Self { next: 0, seq: 0, sample_index: 0, batch: 8 }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        if id == "batch_size" {
            if let ParamValue::U32(n) = value {
                self.batch = (*n as usize).clamp(1, 1024);
            }
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        self.next = 0;
        self.seq = 0;
        self.sample_index = 0;
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        if let Some(out) = outputs.first_mut() {
            let n = self.batch.min(out.capacity() / 4);
            let buf = out.buffer_mut();
            for i in 0..n {
                buf[i * 4..i * 4 + 4]
                    .copy_from_slice(&(self.next + i as u32).to_le_bytes());
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

sigflow_plugin_sdk::process_main!(__PLUGIN_STRUCT__, include_str!("../manifest.toml"));
