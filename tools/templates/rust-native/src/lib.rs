//! __PLUGIN_NAME__ — sigflow native 插件骨架：透传 + 参数示例。
//!
//! 契约（端口/参数/动作）声明在旁边的 manifest.toml，壳体解析后传给
//! `Plugin::new`——代码里只写处理逻辑。构建装载三步见 README。

use sigflow_plugin_sdk::{Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome};

pub struct __PLUGIN_STRUCT__ {
    gain: f64,
}

impl Plugin for __PLUGIN_STRUCT__ {
    fn new(_manifest: &PluginManifest) -> Self {
        // 冷启动默认值与 manifest 的 default 保持一致；壳体会在构造后
        // 把已保存的参数逐个 set_param 推下来。
        Self { gain: 1.0 }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        if id == "gain" {
            if let ParamValue::F64(v) = value {
                self.gain = v.clamp(0.0, 10.0);
            }
        }
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        if let (Some(input), Some(out)) = (inputs.first(), outputs.first_mut()) {
            // 透传帧头，保住时间语义（t0/sample_index/不连续标志）。
            out.header = input.header;
            let n = input.data.len().min(out.capacity());
            out.buffer_mut()[..n].copy_from_slice(&input.data[..n]);
            out.set_written(n);
        }
        ProcessOutcome::Ok
    }
}

sigflow_plugin_sdk::export_plugin!(__PLUGIN_STRUCT__);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_toml_parses() {
        let m: PluginManifest =
            toml::from_str(include_str!("../manifest.toml")).expect("manifest parses");
        assert_eq!(m.name, "__PLUGIN_ID__");
        assert_eq!(m.ports.len(), 2);
    }

    #[test]
    fn passthrough_copies_input() {
        let m: PluginManifest = toml::from_str(include_str!("../manifest.toml")).unwrap();
        let mut p = __PLUGIN_STRUCT__::new(&m);
        use sigflow_plugin_sdk::FrameHeader;
        let input = [1u8, 2, 3, 4];
        let mut buf = [0u8; 4];
        let frames = [Frame::new(FrameHeader::ZERO, &input)];
        let mut outs = [FrameOut::new(&mut buf)];
        assert!(matches!(p.process(&frames, &mut outs), ProcessOutcome::Ok));
        assert_eq!(outs[0].written(), 4);
        assert_eq!(buf, input);
    }
}
