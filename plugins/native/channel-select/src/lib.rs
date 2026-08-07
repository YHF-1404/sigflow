//! sigflow.dsp.channel_select — 通用通道选择器。
//!
//! 从 n 列交织 f32 流选出 `channels` 参数指定的列（输出列顺序 = 列表
//! 顺序）。无状态逐帧变换：帧头与随帧注解原样透传，chunk 边界不动。
//! 空列表 = 全通（恒等透传，注解照带）。

use sigflow_plugin_sdk::{
    parse_annotation, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

const F32: usize = 4;

pub struct ChannelSelect {
    /// 选中的列下标（输出顺序）；空 = 全通。
    channels: Vec<usize>,
    verbose: bool,
    warned_range: bool,
    warned_cap: bool,
}

impl ChannelSelect {
    fn ingest(&mut self, frame: &Frame, out: &mut FrameOut) {
        let h = frame.header;
        let data = frame.samples();
        if data.is_empty() || h.n_samples == 0 {
            return;
        }
        let rows = h.n_samples as usize;
        let in_cols = (data.len() / F32) / rows;
        if in_cols == 0 {
            return;
        }

        let out_bytes = if self.channels.is_empty() {
            data.len()
        } else {
            rows * self.channels.len() * F32
        };
        // 注解透传也要空间
        let annot = frame.annotation();
        if out_bytes + annot.map_or(0, <[u8]>::len) > out.capacity() {
            if !self.warned_cap {
                self.warned_cap = true;
                eprintln!(
                    "channel_select: frame {} B exceeds output buffer {} B — frames skipped",
                    out_bytes,
                    out.capacity()
                );
            }
            return;
        }

        if self.channels.is_empty() {
            out.write(data); // 全通
        } else {
            if self.channels.iter().any(|&c| c >= in_cols) {
                if !self.warned_range {
                    self.warned_range = true;
                    eprintln!(
                        "channel_select: selection {:?} out of range (frame has {in_cols} columns) — frames skipped until fixed",
                        self.channels
                    );
                }
                return;
            }
            let dst = out.buffer_mut();
            let k = self.channels.len();
            for r in 0..rows {
                let src_base = r * in_cols * F32;
                let dst_base = r * k * F32;
                for (j, &c) in self.channels.iter().enumerate() {
                    let s = src_base + c * F32;
                    let d = dst_base + j * F32;
                    dst[d..d + F32].copy_from_slice(&data[s..s + F32]);
                }
            }
            out.set_written(out_bytes);
        }

        // 帧头透传（n_samples 不变：行数语义；列数由 payload 自描述）
        out.header = h;
        out.header.flags &= !sigflow_plugin_sdk::FLAG_ANNOTATED;
        out.header.annotation_len = 0;
        if let Some((schema, body)) = annot.and_then(parse_annotation) {
            out.set_annotation(schema, body);
        }
    }
}

impl Plugin for ChannelSelect {
    fn new(_manifest: &PluginManifest) -> Self {
        ChannelSelect {
            channels: Vec::new(),
            verbose: false,
            warned_range: false,
            warned_cap: false,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("channels", ParamValue::String(v)) | ("channels", ParamValue::Enum(v)) => {
                let parsed: Vec<usize> = v
                    .split(',')
                    .filter_map(|t| {
                        let t = t.trim();
                        if t.is_empty() {
                            None
                        } else {
                            t.parse::<usize>()
                                .map_err(|_| eprintln!("channel_select: ignoring invalid column '{t}'"))
                                .ok()
                        }
                    })
                    .collect();
                if self.verbose {
                    eprintln!("channel_select: channels = {parsed:?}");
                }
                self.channels = parsed;
                self.warned_range = false;
            }
            ("verbose", ParamValue::Bool(v)) => self.verbose = *v,
            _ => {}
        }
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        let Some(out) = outputs.first_mut() else {
            return ProcessOutcome::Ok;
        };
        out.set_written(0);
        if let Some(frame) = inputs.first() {
            self.ingest(frame, out);
        }
        ProcessOutcome::Ok
    }
}

#[cfg(feature = "abi")]
sigflow_plugin_sdk::export_plugin!(ChannelSelect);

#[cfg(test)]
mod tests {
    use super::*;
    use sigflow_plugin_sdk::{FrameHeader, FLAG_DISCONTINUITY};

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    fn run(p: &mut ChannelSelect, h: FrameHeader, data: &[u8]) -> (FrameHeader, Vec<u8>, usize) {
        let mut buf = vec![0u8; 4096];
        let mut outs = vec![FrameOut::new(&mut buf)];
        let frames = [Frame::new(h, data)];
        assert!(matches!(p.process(&frames, &mut outs), ProcessOutcome::Ok));
        let w = outs[0].written();
        (outs[0].header, buf[..w].to_vec(), w)
    }

    /// 3 行 × 4 列（值 = 行*10+列）+ 注解，选 "3,1"。
    #[test]
    fn selects_columns_in_list_order_and_passes_annotation() {
        let mut p = ChannelSelect::new(&manifest());
        p.set_param("channels", &ParamValue::String("3,1".into()));

        let mut payload = Vec::new();
        for r in 0..3 {
            for c in 0..4 {
                payload.extend_from_slice(&((r * 10 + c) as f32).to_le_bytes());
            }
        }
        let annot_body = b"{\"window_rows\":3,\"trig_row\":1}";
        let mut annotated = payload.clone();
        annotated.extend_from_slice(&0x1234_5678_9abc_def0u64.to_le_bytes());
        annotated.extend_from_slice(annot_body);
        let mut h = FrameHeader::ZERO;
        h.seq = 7;
        h.sample_index = 42;
        h.t0_ns = 999;
        h.n_samples = 3;
        h.flags = FLAG_DISCONTINUITY | sigflow_plugin_sdk::FLAG_ANNOTATED;
        h.annotation_len = (8 + annot_body.len()) as u32;
        h.gap_samples = 5;

        let (oh, out, w) = run(&mut p, h, &annotated);
        // 帧头透传
        assert_eq!(oh.sample_index, 42);
        assert_eq!(oh.t0_ns, 999);
        assert_eq!(oh.seq, 7);
        assert_eq!(oh.gap_samples, 5);
        assert!(oh.flags & FLAG_DISCONTINUITY != 0);
        assert!(oh.annotation_len > 0, "annotation passthrough");
        // 样本：3 行 × 2 列，列序 = [3,1]
        let sample_bytes = w - oh.annotation_len as usize;
        assert_eq!(sample_bytes, 3 * 2 * 4);
        let f = |i: usize| f32::from_le_bytes(out[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!([f(0), f(1)], [3.0, 1.0]);
        assert_eq!([f(2), f(3)], [13.0, 11.0]);
        assert_eq!([f(4), f(5)], [23.0, 21.0]);
        // 注解逐字节透传
        let out_frame = Frame::new(oh, &out);
        let (schema, body) = parse_annotation(out_frame.annotation().unwrap()).unwrap();
        assert_eq!(schema, 0x1234_5678_9abc_def0);
        assert_eq!(body, annot_body);
    }

    #[test]
    fn empty_list_is_identity_and_out_of_range_skips() {
        let mut p = ChannelSelect::new(&manifest());
        let payload: Vec<u8> = (0..8u32).flat_map(|v| (v as f32).to_le_bytes()).collect();
        let mut h = FrameHeader::ZERO;
        h.n_samples = 2; // 2 行 × 4 列
        let (_, out, w) = run(&mut p, h, &payload);
        assert_eq!(w, payload.len());
        assert_eq!(out, payload);

        p.set_param("channels", &ParamValue::String("9".into()));
        let (_, _, w) = run(&mut p, h, &payload);
        assert_eq!(w, 0, "out-of-range selection must skip");
    }
}
