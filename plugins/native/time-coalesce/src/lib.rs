//! sigflow.core.time_coalesce — 通用时间合帧（领头帧会话窗）。
//!
//! 双笔同落消歧的前置件（[[dual-pen-match]] 设计）：把 t0 相近的事件帧
//! 合并成一帧，下游匹配节点一次拿到同窗的全部事件。
//!
//! 为什么不是"首帧启动定时器 + 到期批切分"：批边界会劈裂恰好跨界的一对
//! 事件（如 wait=50 时 t=48/t=52 的双笔），正是本节点要防的错配。改为
//! **领头帧会话窗**：帧到达时能装进某开放组（t0 − leader_t0 < window_ms）
//! 即入组，否则自己开新组；组在领头帧**到达**后 group_wait_ms（墙钟）
//! 发射——归属用 t0 轴、发射时限用墙钟，两参各司其职（wait ≥ window）。
//!
//! 载荷合成：组内按 t0 排序，各帧 samples 以 '\n' 拼接（JSON 事件帧 →
//! JSON Lines）；header t0/seq 取领头帧，n_samples = 组内帧数。

use std::collections::VecDeque;

use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

struct Group {
    leader_t0: i64,
    arrival_ns: i64,
    /// (t0, payload)
    members: Vec<(i64, Vec<u8>)>,
}

pub struct TimeCoalesce {
    window_ms: f64,
    group_wait_ms: f64,
    verbose: bool,
    groups: VecDeque<Group>,
    seq: u64,
    emitted: u64,
    merged: u64,
}

impl Plugin for TimeCoalesce {
    fn new(_m: &PluginManifest) -> Self {
        TimeCoalesce {
            window_ms: 30.0,
            group_wait_ms: 50.0,
            verbose: false,
            groups: VecDeque::new(),
            seq: 0,
            emitted: 0,
            merged: 0,
        }
    }

    fn set_param(&mut self, id: &str, v: &ParamValue) {
        match (id, v) {
            ("window_ms", ParamValue::F64(x)) => self.window_ms = *x,
            ("group_wait_ms", ParamValue::F64(x)) => self.group_wait_ms = *x,
            ("verbose", ParamValue::Bool(x)) => self.verbose = *x,
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        self.groups.clear();
        self.seq = 0;
        ProcessOutcome::Ok
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        let now = mono_ns();
        // 入组（每 tick 每口一帧——FIFO 消费语义）
        for f in inputs {
            if f.header.n_samples == 0 && f.data.is_empty() {
                continue;
            }
            let t0 = f.header.t0_ns;
            let payload = f.samples().to_vec();
            let win_ns = (self.window_ms * 1e6) as i64;
            match self
                .groups
                .iter_mut()
                .find(|g| (t0 - g.leader_t0).abs() < win_ns)
            {
                Some(g) => {
                    g.members.push((t0, payload));
                    self.merged += 1;
                    if self.verbose {
                        eprintln!(
                            "time_coalesce: joined group (leader_t0={}, Δ={}ms, size={})",
                            g.leader_t0,
                            (t0 - g.leader_t0) / 1_000_000,
                            g.members.len()
                        );
                    }
                }
                None => self.groups.push_back(Group {
                    leader_t0: t0,
                    arrival_ns: now,
                    members: vec![(t0, payload)],
                }),
            }
        }
        // 发射：领头帧到达超过 group_wait 的组（FIFO；每 tick 至多一组）
        let wait_ns = (self.group_wait_ms * 1e6) as i64;
        if let Some(g) = self.groups.front() {
            if now - g.arrival_ns >= wait_ns {
                let mut g = self.groups.pop_front().unwrap();
                g.members.sort_by_key(|(t0, _)| *t0);
                if let Some(out) = outputs.first_mut() {
                    let mut buf: Vec<u8> = Vec::new();
                    for (i, (_, p)) in g.members.iter().enumerate() {
                        if i > 0 {
                            buf.push(b'\n');
                        }
                        buf.extend_from_slice(p);
                    }
                    out.write(&buf);
                    out.header.seq = self.seq;
                    out.header.t0_ns = g.leader_t0;
                    out.header.n_samples = g.members.len() as u32;
                    self.seq += 1;
                    self.emitted += 1;
                    if self.verbose && g.members.len() > 1 {
                        eprintln!(
                            "time_coalesce: emit group of {} (leader_t0={})",
                            g.members.len(),
                            g.leader_t0
                        );
                    }
                }
            }
        }
        ProcessOutcome::Ok
    }
}

#[cfg(feature = "abi")]
sigflow_plugin_sdk::export_plugin!(TimeCoalesce);

#[cfg(test)]
mod tests {
    use super::*;
    use sigflow_plugin_sdk::FrameHeader;

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).unwrap()
    }

    fn frame_at(t0: i64, body: &str) -> (FrameHeader, Vec<u8>) {
        let mut h = FrameHeader::ZERO;
        h.t0_ns = t0;
        h.n_samples = 1;
        (h, body.as_bytes().to_vec())
    }

    fn tick(p: &mut TimeCoalesce, ins: &[(FrameHeader, Vec<u8>)]) -> Option<(FrameHeader, Vec<u8>)> {
        let frames: Vec<Frame> = ins.iter().map(|(h, d)| Frame::new(*h, d)).collect();
        let mut buf = vec![0u8; 4096];
        let mut outs = [FrameOut::new(&mut buf)];
        p.process(&frames, &mut outs);
        let w = outs[0].written();
        if outs[0].header.n_samples > 0 {
            let h = outs[0].header;
            Some((h, outs[0].buffer_mut()[..w].to_vec()))
        } else {
            None
        }
    }

    #[test]
    fn manifest_shape() {
        let m = manifest();
        assert_eq!(m.name, "sigflow.core.time_coalesce");
        assert!(m.tick_driven);
    }

    /// 单帧：等待期不出、超时后单帧原样出。
    #[test]
    fn single_frame_passes_after_wait() {
        let mut p = TimeCoalesce::new(&manifest());
        p.set_param("group_wait_ms", &ParamValue::F64(30.0));
        p.start();
        let t0 = mono_ns();
        assert!(tick(&mut p, &[frame_at(t0, r#"{"a":1}"#)]).is_none());
        std::thread::sleep(std::time::Duration::from_millis(40));
        let (h, d) = tick(&mut p, &[]).expect("emit after wait");
        assert_eq!(h.n_samples, 1);
        assert_eq!(h.t0_ns, t0);
        assert_eq!(d, br#"{"a":1}"#);
    }

    /// 双帧同窗合并（含乱序到达按 t0 排序）；跨窗第三帧独立成组。
    #[test]
    fn near_frames_merge_far_frame_opens_new_group() {
        let mut p = TimeCoalesce::new(&manifest());
        p.set_param("window_ms", &ParamValue::F64(30.0));
        p.set_param("group_wait_ms", &ParamValue::F64(30.0));
        p.start();
        let base = mono_ns();
        // 乱序：后到的帧 t0 更小
        assert!(tick(&mut p, &[frame_at(base + 20_000_000, r#"{"b":2}"#)]).is_none());
        assert!(tick(&mut p, &[frame_at(base, r#"{"a":1}"#)]).is_none());
        // 远帧（t0 差 100ms）→ 新组
        assert!(tick(&mut p, &[frame_at(base + 100_000_000, r#"{"c":3}"#)]).is_none());
        std::thread::sleep(std::time::Duration::from_millis(40));
        let (h, d) = tick(&mut p, &[]).expect("group 1");
        assert_eq!(h.n_samples, 2);
        assert_eq!(d, b"{\"a\":1}\n{\"b\":2}"); // t0 排序后 a 在前
        assert_eq!(h.t0_ns, base + 20_000_000); // 领头 = 先到的 b 帧
        let (h2, d2) = tick(&mut p, &[]).expect("group 2");
        assert_eq!(h2.n_samples, 1);
        assert_eq!(d2, br#"{"c":3}"#);
    }
}
