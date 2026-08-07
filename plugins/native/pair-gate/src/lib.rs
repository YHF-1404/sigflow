//! Pair-gate：通用 N 通道时间会合门。
//!
//! 从 capture-touch-merge 提炼出的通用形态：那边是"把 B 注解进 A"的
//! 专用合并节点，这里是**原样透传**的成组放行——前 `channels` 个输入口
//! 各自扣帧，当每个通道都有一帧与锚帧 |t0 差| ≤ window 时整组同 tick
//! 从对应输出口发出（header + payload 逐字节不动，含注解尾巴）；等满
//! timeout 仍凑不齐组的帧告警丢弃，不产生任何输出。典型用法是数据集
//! 落盘前的**完整性门**：触控坐标 × 捕获窗 × 检峰窗三全才放行。
//!
//! 设计要点（与 capture-touch-merge 的经验对照）：
//! - **配对判据 = 帧 t0**，不是到达时序。到达驱动的配对会把"某通道积压
//!   的旧帧"配给刚到的帧。t0 全链路锚在 host 单调钟上（SDC 链端到端
//!   µs 级），语义上就是"同一时刻的事件"。若某个源的 t0 相对 host 有
//!   漂移，调大 window 而不是改判据。
//! - **锚帧 = 全体扣押帧中最早到达者**（FIFO 公平），其余每通道取
//!   |Δt0| 最小且入窗的一帧；全齐才放行，缺位则锚帧继续等或超时。
//! - **超时 = 从到达时刻等成组的时长**，纯粹的放弃机制。本节点按输入
//!   门控 tick，超时结算发生在其后的第一个输入 tick（"至少 t"）。
//! - **先配组后超时**：懒 tick 下"补位帧刚到"和"锚帧已超时"可能同 tick
//!   同真——此时成组语义正确（t0 对得上），放行优于守时。
//! - 每 tick 至多放行一组（壳体一 tick 一帧）；积压组在后续 tick 流出。

use std::collections::VecDeque;

use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameHeader, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

/// manifest 静态申报的口数上限。
const MAX_CHANNELS: usize = 4;

/// 单通道扣押上限：超过说明其他通道长期无数据且超时参数过大，丢最旧的并告警。
const MAX_PENDING: usize = 64;

struct Held {
    arrival_ns: i64,
    header: FrameHeader,
    data: Vec<u8>,
}

pub struct PairGate {
    channels: usize,
    window_ns: i64,
    timeout_ns: i64,
    verbose: bool,
    /// 按到达序入队（front 最旧），只有前 `channels` 条参与。
    pend: [VecDeque<Held>; MAX_CHANNELS],
    groups: u64,
    timeouts: u64,
}

impl PairGate {
    /// 找一组可放行的帧：把每个扣押帧轮流当锚（锚定义组的 t0 参考），
    /// 其余每通道取 |Δt0_锚| 最小且 ≤ window 的一帧；在所有能凑齐的
    /// 候选组里放行 **t0 散布最小** 的那组（配对质量优先——窗内多候选
    /// 时最近者胜出，而不是先到者抢走），平局取到达更早的锚。
    fn find_group(&self) -> Option<Vec<usize>> {
        let mut anchors: Vec<(i64, usize, usize)> = Vec::new();
        for side in 0..self.channels {
            for (i, h) in self.pend[side].iter().enumerate() {
                anchors.push((h.arrival_ns, side, i));
            }
        }
        anchors.sort_unstable_by_key(|&(arr, _, _)| arr);

        let mut best_group: Option<(i64, Vec<usize>)> = None; // (spread, picks)
        'anchor: for &(_, s0, i0) in &anchors {
            let t0 = self.pend[s0][i0].header.t0_ns;
            let mut pick = vec![usize::MAX; self.channels];
            pick[s0] = i0;
            let (mut lo, mut hi) = (t0, t0);
            for side in 0..self.channels {
                if side == s0 {
                    continue;
                }
                let best = self.pend[side]
                    .iter()
                    .enumerate()
                    .map(|(i, h)| (i, h.header.t0_ns))
                    .filter(|&(_, t)| (t - t0).abs() <= self.window_ns)
                    .min_by_key(|&(_, t)| (t - t0).abs());
                match best {
                    Some((i, t)) => {
                        pick[side] = i;
                        lo = lo.min(t);
                        hi = hi.max(t);
                    }
                    None => continue 'anchor,
                }
            }
            let spread = hi - lo;
            if best_group.as_ref().is_none_or(|(s, _)| spread < *s) {
                best_group = Some((spread, pick));
            }
        }
        best_group.map(|(_, pick)| pick)
    }

    /// 丢弃等满 timeout 的帧（front 起，入队即到达序 → 前缀即可）。
    fn expire(&mut self, now: i64) {
        for side in 0..self.channels {
            while let Some(h) = self.pend[side].front() {
                if now - h.arrival_ns < self.timeout_ns {
                    break;
                }
                let h = self.pend[side].pop_front().unwrap();
                self.timeouts += 1;
                eprintln!(
                    "pair_gate: timeout — in_{side} frame t0={} waited {}ms without a \
                     complete group within ±{}ms; dropped (total timeouts {})",
                    h.header.t0_ns,
                    (now - h.arrival_ns) / 1_000_000,
                    self.window_ns / 1_000_000,
                    self.timeouts
                );
            }
        }
    }

    fn clear(&mut self) -> usize {
        let n: usize = self.pend.iter().map(|q| q.len()).sum();
        for q in &mut self.pend {
            q.clear();
        }
        n
    }
}

impl Plugin for PairGate {
    fn new(_manifest: &PluginManifest) -> Self {
        PairGate {
            channels: 2,
            window_ns: 100_000_000,  // 100 ms
            timeout_ns: 500_000_000, // 500 ms
            verbose: false,
            pend: Default::default(),
            groups: 0,
            timeouts: 0,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("channels", ParamValue::U32(v)) => {
                let n = (*v as usize).clamp(2, MAX_CHANNELS);
                if n != self.channels {
                    self.channels = n;
                    // 通道数变了，扣押组的语义随之失效：清空重来。
                    let dropped = self.clear();
                    if dropped > 0 {
                        eprintln!("pair_gate: channels -> {n}; dropped {dropped} held frames");
                    }
                }
            }
            ("window_ms", ParamValue::F64(v)) => {
                self.window_ns = (v.clamp(0.1, 10000.0) * 1e6) as i64
            }
            ("timeout_ms", ParamValue::F64(v)) => {
                self.timeout_ns = (v.clamp(1.0, 60000.0) * 1e6) as i64
            }
            ("verbose", ParamValue::Bool(b)) => self.verbose = *b,
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        self.clear();
        ProcessOutcome::Ok
    }

    fn invoke_action(&mut self, id: &str) {
        if id == "reset" {
            let n = self.clear();
            eprintln!("pair_gate: reset — dropped {n} held frames");
        }
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        let now = mono_ns();

        // 1) 入队本 tick 的到达帧（壳体空占位 = ZERO header + 空 payload；
        //    header-only 事件帧——如纯触发标记——合法，凭 header 非零入队）。
        //    channels 之外的口忽略。
        for side in 0..self.channels {
            if let Some(f) = inputs.get(side) {
                if f.header == FrameHeader::ZERO && f.data.is_empty() {
                    continue;
                }
                self.pend[side].push_back(Held {
                    arrival_ns: now,
                    header: f.header,
                    data: f.data.to_vec(),
                });
                if self.verbose {
                    // 入队后快照（含本帧）：三通道齐时正好读作 [1,1,1] → 放行。
                    let held: Vec<usize> =
                        (0..self.channels).map(|s| self.pend[s].len()).collect();
                    eprintln!(
                        "pair_gate: rx in_{side} t0={} ({} bytes), held {:?}",
                        f.header.t0_ns,
                        f.data.len(),
                        held
                    );
                }
                if self.pend[side].len() > MAX_PENDING {
                    self.pend[side].pop_front();
                    eprintln!(
                        "pair_gate: in_{side} held over {MAX_PENDING}; oldest dropped \
                         (other channels idle? timeout too long?)"
                    );
                }
            }
        }

        // 2) 先配组（懒 tick 下放行优于守时——见模块注释），每 tick 至多一组。
        if let Some(pick) = self.find_group() {
            self.groups += 1;
            let mut t0s: Vec<i64> = Vec::with_capacity(self.channels);
            for (side, &idx) in pick.iter().enumerate() {
                let h = self.pend[side].remove(idx).unwrap();
                t0s.push(h.header.t0_ns);
                if let Some(out) = outputs.get_mut(side) {
                    out.header = h.header;
                    out.write(&h.data);
                }
            }
            if self.verbose {
                let spread_us = (t0s.iter().max().unwrap() - t0s.iter().min().unwrap()) / 1_000;
                eprintln!(
                    "pair_gate: group #{} — t0 spread {}us across {} channels",
                    self.groups, spread_us, self.channels
                );
            }
        }

        // 3) 已可成组、只因"一 tick 一组"排队的帧：续租（刷新到达钟），
        //    免遭下面的超时结算误杀——它们必然在后续 tick 放行。
        let mut queued: Vec<(usize, Held)> = Vec::new();
        while let Some(pick) = self.find_group() {
            for (side, &idx) in pick.iter().enumerate() {
                let mut h = self.pend[side].remove(idx).unwrap();
                h.arrival_ns = now;
                queued.push((side, h));
            }
        }

        // 4) 结算超时（只剩真正的孤儿）。
        self.expire(now);

        // 续租帧放回队尾（arrival=now 保持队内到达序单调）。
        for (side, h) in queued {
            self.pend[side].push_back(h);
        }
        ProcessOutcome::Ok
    }
}

sigflow_plugin_sdk::export_plugin!(PairGate);

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    fn gate(channels: usize) -> PairGate {
        let mut g = PairGate::new(&manifest());
        g.channels = channels;
        g
    }

    fn ev(t0_ns: i64, payload: &[u8]) -> (FrameHeader, Vec<u8>) {
        let mut h = FrameHeader::ZERO;
        h.t0_ns = t0_ns;
        h.n_samples = 1;
        (h, payload.to_vec())
    }

    /// 驱动一个 tick；inputs 按口序给（None = 该口本 tick 无数据）。
    /// 返回每个输出口的 (header, payload)，None = 无输出。
    fn run(
        g: &mut PairGate,
        frames: [Option<&(FrameHeader, Vec<u8>)>; MAX_CHANNELS],
    ) -> Vec<Option<(FrameHeader, Vec<u8>)>> {
        let empty = (FrameHeader::ZERO, Vec::<u8>::new());
        let inputs: Vec<Frame> = frames
            .iter()
            .map(|f| {
                let (h, d) = f.unwrap_or(&empty);
                Frame::new(*h, d)
            })
            .collect();
        let mut bufs: Vec<Vec<u8>> = (0..MAX_CHANNELS).map(|_| vec![0u8; 4096]).collect();
        let mut outs: Vec<FrameOut> = bufs.iter_mut().map(|b| FrameOut::new(b)).collect();
        let _ = g.process(&inputs, &mut outs);
        // 有效输出 = 写了字节或 header 非零（header-only 事件帧合法；
        // 壳体同款判据：written==0 && header==ZERO 才视为闲置跳过）。
        outs.iter_mut()
            .map(|o| {
                let w = o.written();
                (w > 0 || o.header != FrameHeader::ZERO)
                    .then(|| (o.header, o.buffer_mut()[..w].to_vec()))
            })
            .collect()
    }

    #[test]
    fn manifest_declares_four_in_four_out_plus_channels_param() {
        let m = manifest();
        let ids: Vec<_> = m.ports.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            ["in_0", "in_1", "in_2", "in_3", "out_0", "out_1", "out_2", "out_3"]
        );
        assert!(m.parameters.iter().any(|p| p.id == "channels"));
    }

    #[test]
    fn pair_within_window_emits_both_verbatim() {
        let mut g = gate(2);
        let a = ev(1_000_000_000, b"{\"x\":1}");
        let r = run(&mut g, [Some(&a), None, None, None]);
        assert!(r.iter().all(|o| o.is_none())); // A 独自扣押
        let b = ev(1_030_000_000, b"{\"touch\":true}"); // 30ms 后，入窗
        let r = run(&mut g, [None, Some(&b), None, None]);
        let (ha, da) = r[0].clone().expect("out_0 emits");
        let (hb, db) = r[1].clone().expect("out_1 emits");
        assert_eq!(ha.t0_ns, 1_000_000_000);
        assert_eq!(da, a.1);
        assert_eq!(hb.t0_ns, 1_030_000_000);
        assert_eq!(db, b.1);
        assert!(g.pend.iter().all(|q| q.is_empty()));
    }

    #[test]
    fn three_way_group_needs_all_three() {
        let mut g = gate(3);
        let touch = ev(1_000_000_000, b"touch");
        let cap = ev(1_005_000_000, b"cap");
        let view = ev(1_002_000_000, b"view");
        let r = run(&mut g, [Some(&touch), Some(&cap), None, None]);
        assert!(r.iter().all(|o| o.is_none()), "2/3 must not release");
        let r = run(&mut g, [None, None, Some(&view), None]);
        assert_eq!(r[0].clone().unwrap().1, b"touch".to_vec());
        assert_eq!(r[1].clone().unwrap().1, b"cap".to_vec());
        assert_eq!(r[2].clone().unwrap().1, b"view".to_vec());
        assert!(r[3].is_none());
    }

    #[test]
    fn channel_beyond_active_count_is_ignored() {
        let mut g = gate(2);
        let x = ev(1_000_000_000, b"x");
        run(&mut g, [None, None, Some(&x), None]); // in_2 在 channels=2 时无效
        assert!(g.pend[2].is_empty());
    }

    #[test]
    fn outside_window_stays_held() {
        let mut g = gate(2);
        let a = ev(1_000_000_000, b"A");
        let b = ev(1_300_000_000, b"B"); // 300ms > 默认窗 100ms
        let r = run(&mut g, [Some(&a), Some(&b), None, None]);
        assert!(r.iter().all(|o| o.is_none()));
        assert_eq!(g.pend[0].len(), 1);
        assert_eq!(g.pend[1].len(), 1);
    }

    #[test]
    fn nearest_of_several_partners_wins() {
        let mut g = gate(2);
        let a = ev(2_000_000_000, b"A");
        let b1 = ev(2_090_000_000, b"far");
        let b2 = ev(2_010_000_000, b"near");
        run(&mut g, [None, Some(&b1), None, None]);
        run(&mut g, [None, Some(&b2), None, None]);
        let r = run(&mut g, [Some(&a), None, None, None]);
        assert_eq!(r[1].clone().expect("pairs").1, b"near".to_vec());
        assert_eq!(g.pend[1].len(), 1); // far 的 B 还扣着
    }

    #[test]
    fn timeout_drops_lone_frame_without_output() {
        let mut g = gate(2);
        g.timeout_ns = 0; // 立即超时
        let a = ev(1_000_000_000, b"A");
        let r = run(&mut g, [Some(&a), None, None, None]); // 入队后同 tick 结算掉
        assert!(r.iter().all(|o| o.is_none()));
        assert!(g.pend[0].is_empty());
        assert_eq!(g.timeouts, 1);
        // 之后配对帧再来也没用了
        let b = ev(1_000_000_100, b"B");
        let r = run(&mut g, [None, Some(&b), None, None]);
        assert!(r.iter().all(|o| o.is_none()));
    }

    #[test]
    fn match_wins_over_simultaneous_expiry() {
        // 懒 tick 语义：补位帧到达与超时同 tick 同真 → 放行优于守时。
        let mut g = gate(2);
        g.timeout_ns = 0;
        let a = ev(1_000_000_000, b"A");
        let b = ev(1_020_000_000, b"B");
        let r = run(&mut g, [Some(&a), Some(&b), None, None]);
        assert!(r[0].is_some() && r[1].is_some());
        assert_eq!(g.timeouts, 0);
    }

    #[test]
    fn one_group_per_tick_backlog_flushes_in_order() {
        let mut g = gate(2);
        let a1 = ev(1_000_000_000, b"a1");
        let a2 = ev(3_000_000_000, b"a2");
        let b1 = ev(1_010_000_000, b"b1");
        let b2 = ev(3_010_000_000, b"b2");
        run(&mut g, [Some(&a1), None, None, None]);
        let r = run(&mut g, [Some(&a2), Some(&b1), None, None]); // 放行 (a1,b1)
        assert_eq!(r[0].clone().unwrap().0.t0_ns, 1_000_000_000);
        let r = run(&mut g, [None, Some(&b2), None, None]); // 放行 (a2,b2)
        assert_eq!(r[0].clone().unwrap().0.t0_ns, 3_000_000_000);
    }

    #[test]
    fn header_only_event_frame_is_accepted() {
        let mut g = gate(2);
        let mut h = FrameHeader::ZERO;
        h.t0_ns = 7_000_000_000;
        h.n_samples = 1;
        let trig = (h, Vec::new()); // 纯触发标记：有 header 无 payload
        run(&mut g, [Some(&trig), None, None, None]);
        assert_eq!(g.pend[0].len(), 1);
        let b = ev(7_001_000_000, b"B");
        let r = run(&mut g, [None, Some(&b), None, None]);
        assert_eq!(r[0].clone().expect("emits").0.t0_ns, 7_000_000_000);
        assert!(r[1].is_some());
    }

    #[test]
    fn pending_overflow_drops_oldest() {
        let mut g = gate(2);
        for i in 0..(MAX_PENDING + 3) {
            let a = ev(i as i64 * 1_000_000_000, b"A");
            run(&mut g, [Some(&a), None, None, None]);
        }
        assert_eq!(g.pend[0].len(), MAX_PENDING);
        assert_eq!(g.pend[0].front().unwrap().header.t0_ns, 3_000_000_000);
    }

    #[test]
    fn queued_complete_groups_survive_expiry() {
        // 三组的"最后一块"同 tick 各从一口到达 → 三组同时齐，一 tick 只能
        // 放行一组；排队的两组跨越超时期限后必须仍被放行（续租），而不是
        // 在下一 tick 的超时结算里被误杀。
        let mut g = gate(3);
        g.timeout_ns = 200_000_000; // 200ms
        let (ta, ca, va) = (ev(1_000_000_000, b"tA"), ev(1_002_000_000, b"cA"), ev(1_005_000_000, b"vA"));
        let (tb, cb, vb) = (ev(3_000_000_000, b"tB"), ev(3_010_000_000, b"cB"), ev(3_008_000_000, b"vB"));
        let (tc, cc, vc) = (ev(5_000_000_000, b"tC"), ev(5_015_000_000, b"cC"), ev(5_020_000_000, b"vC"));
        run(&mut g, [Some(&ta), Some(&ca), None, None]); // A 缺 vA
        run(&mut g, [Some(&tb), None, Some(&vb), None]); // B 缺 cB
        run(&mut g, [None, Some(&cc), Some(&vc), None]); // C 缺 tC
        // 补齐三组的最后一块：同 tick 三组全齐，放行散布最小的 A
        let r = run(&mut g, [Some(&tc), Some(&cb), Some(&va), None]);
        assert_eq!(r[0].clone().unwrap().0.t0_ns, 1_000_000_000, "A releases first");
        // 跨越超时期限——没有续租的话，排队的 B/C 会被误杀
        std::thread::sleep(std::time::Duration::from_millis(250));
        let r = run(&mut g, [None, None, None, None]);
        assert_eq!(r[0].clone().unwrap().0.t0_ns, 3_000_000_000, "B releases");
        std::thread::sleep(std::time::Duration::from_millis(250));
        let r = run(&mut g, [None, None, None, None]);
        assert_eq!(r[0].clone().unwrap().0.t0_ns, 5_000_000_000, "C releases");
        assert_eq!(g.timeouts, 0, "no false expiry of complete groups");
        assert!(g.pend.iter().all(|q| q.is_empty()));
    }

    #[test]
    fn channels_change_clears_held() {
        let mut g = gate(3);
        let a = ev(1_000_000_000, b"A");
        run(&mut g, [Some(&a), None, None, None]);
        assert_eq!(g.pend[0].len(), 1);
        g.set_param("channels", &ParamValue::U32(2));
        assert!(g.pend[0].is_empty());
        // 等值写入不清
        let b = ev(2_000_000_000, b"B");
        run(&mut g, [Some(&b), None, None, None]);
        g.set_param("channels", &ParamValue::U32(2));
        assert_eq!(g.pend[0].len(), 1);
    }
}
