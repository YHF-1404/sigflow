//! Touch tracker plugin: raw byte stream → first-`MOVE` touch events.
//!
//! Consumes the byte stream from a `tcp_source`, reassembles fixed **64-byte**
//! `OnePktData` touch reports, and runs the device's per-`contact_id` state
//! machine. For each touch *round* it emits one event carrying the coordinate of
//! the **first `MOVE`** — the first stable in-contact frame after touchdown
//! (DOWN's centroid is still settling, so MOVE is preferred).
//!
//! State conversion mirrors the firmware's `ConvertRawStateToPointState`. The
//! raw `state` byte is context-dependent: notably `0x04` (DOWN_UP) maps to DOWN
//! when arriving from hover/idle and to UP when arriving from DOWN/MOVE — the
//! same byte means opposite edges. So we track each contact's previous
//! (converted) [`PointState`] and convert per the firmware rules:
//! ```text
//! raw 0x00 HOVER_IN : prev idle/hover_out/up → HOVER_IN; prev hover* → HOVER_MOVE
//! raw 0x04 DOWN_UP  : prev hover/idle/up     → DOWN;     prev down/move → UP
//! raw 0x07 MOVE     : → MOVE
//! raw 0x02 HOVER_OUT: → HOVER_OUT
//! ```
//! A round runs DOWN → MOVE…; we emit the first MOVE of each round (optionally
//! after `settle_moves` skipped MOVE frames).
//!
//! The event payload is a compact JSON object
//! `{"contact_id":N,"x":X,"y":Y,"depth":D,"raw_state":S,"touch_ts":T}`, and the
//! output frame's `t0_ns` is the host arrival time of the report so a downstream
//! merge can pair the event with the vibration capture of the same tap.
//!
//! Wire layout (`#pragma pack(1)`, little-endian):
//! ```text
//! OnePktData (256 B, 5 点驱动): report_id u8 | sub_id u8 | struct_version u8 |
//!                    contact_count u8 | points[5] (28 B each) | timestamp u32 | 零填充至 256
//! OnePointDataRaw (28 B): state u8 | contact_id u8 | centroid_x u16 |
//!                    centroid_y u16 | depth u8 | … (rest unused here)
//! ```
//!
//! Reassembly is resync-tolerant: TCP gives no record boundaries (the server
//! prepends a 4-byte big-endian length to each record) and the shell may
//! coalesce/drop byte frames, so a desynced accumulator slides forward one byte
//! at a time until a structurally-valid 64-byte window is found. The
//! report_id/sub_id/struct_version header forms a 3-byte magic anchor — set it
//! (params) to make recovery robust rather than luck-of-the-bytes.
//!
//! Shell-model note: a consumer's `process()` runs only when input arrives and
//! may emit one frame per output port per call. A single tap yields one
//! first-MOVE event, so one per `process()` suffices for the single-pen case; if
//! two contacts hit their first MOVE in the same input frame, the first is
//! emitted and the second dropped with a warning.
//!
//! # 染色缓冲（取代 wb 的报文缓冲/身份标注职责）
//!
//! 报文除走事件口外还进一条 FIFO 染色缓冲，按 `pen_id` 口（端口 I）的
//! 指派给每点标 type_id 后从 `dst` 口发出（v1 23B/v2 44B 自适应帧,
//! uinput_sink 直连）：
//!
//! - **生命周期**：每个 Down 开一条 life（递增 id）,Up/缺席收尾;全部
//!   life 结束 = 染色期（epoch）结束。
//! - **端口 I**：JSON `{"cid":N,"pen":0|1}`。无 epoch 时到达且 cid 在场
//!   → 按**槽**染色：锚定槽 = pen（期内抬笔重落保色）,**其余槽（在场
//!   与期内新落）一律对侧 1-pen（互斥）**,缓冲立即放行;epoch 存续期间
//!   I 的任何数据直接丢弃（禁翻转）。cid 不在场的指派视为陈旧丢弃。
//! - **放行**：严格保序（下游是 MT 协议 B 状态机,乱序会错位注入）。
//!   队首报文"全部点已决（life 已染色/非 life 悬停点）或 age >
//!   dst_timeout_ms（默认 100）"才出队;超时点 type=0（=软笔白,km 迟到
//!   也不回染——买的是快敲低延迟）。每 tick 至多一帧,tick_driven 驱动
//!   空拍排空（1ms tick ≫ 250Hz 报文率）。

use std::collections::{BTreeMap, HashMap, VecDeque};

use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

/// 染色缓冲上限（报文数）。250Hz 下 ≈1s——正常只在等首个指派的 ≤100ms
/// 里积压 ~25 条，顶到上限说明下游长期不拉，丢最老并计数。
const DST_BUF_CAP: usize = 256;
/// life→pen 染色表只保最近这么多条（缓冲窗口内在场 life 数远小于此）。
const COLOR_CAP: usize = 64;

const PKT_SIZE: usize = 256;
const POINT_SIZE: usize = 28;
const POINT_CNT: usize = 5;
const OFF_REPORT_ID: usize = 0;
const OFF_SUB_ID: usize = 1;
const OFF_STRUCT_VERSION: usize = 2;
const OFF_CONTACT_COUNT: usize = 3;
const OFF_POINTS: usize = 4;
const OFF_TIMESTAMP: usize = OFF_POINTS + POINT_CNT * POINT_SIZE; // 144

// Raw `state` byte values (device protocol).
const RAW_HOVER_IN: u8 = 0x00;
const RAW_HOVER_OUT: u8 = 0x02;
const RAW_DOWN_UP: u8 = 0x04;
const RAW_MOVE: u8 = 0x07;

/// Cap the reassembly accumulator so a pathological desync can't grow it
/// without bound (a couple of reports' worth is plenty of slack).
const ACCUM_CAP: usize = PKT_SIZE * 8;

/// Converted point state (mirrors the firmware enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PointState {
    #[default]
    Idle,
    HoverIn,
    HoverMove,
    Down,
    Move,
    Up,
    HoverOut,
}

/// Convert a raw `state` byte to a [`PointState`] given the contact's previous
/// converted state — a faithful port of the firmware's
/// `ConvertRawStateToPointState` (the `DOWN_UP` byte is edge-ambiguous).
fn convert_state(raw: u8, prev: PointState) -> PointState {
    use PointState::*;
    match raw {
        RAW_HOVER_IN => match prev {
            Idle | HoverOut | Up => HoverIn,
            HoverIn | HoverMove => HoverMove,
            _ => HoverIn,
        },
        RAW_DOWN_UP => match prev {
            // from hover/idle/up → a new touch DOWN; from down/move → UP. These
            // two arms cover all PointState variants (no fallthrough needed).
            HoverIn | HoverMove | HoverOut | Up | Idle => Down,
            Down | Move => Up,
        },
        RAW_MOVE => Move,
        RAW_HOVER_OUT => HoverOut,
        _ => Idle,
    }
}

/// One active touch point we care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Point {
    raw_state: u8,
    contact_id: u8,
    x: u16,
    y: u16,
    depth: u8,
}

/// A parsed report: its active points and the controller timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pkt {
    timestamp: u32,
    points: Vec<Point>,
}

/// Read a point at `points[i]` of a 64-byte record.
fn read_point(rec: &[u8], i: usize) -> Point {
    let b = OFF_POINTS + i * POINT_SIZE;
    Point {
        raw_state: rec[b],
        contact_id: rec[b + 1],
        x: u16::from_le_bytes([rec[b + 2], rec[b + 3]]),
        y: u16::from_le_bytes([rec[b + 4], rec[b + 5]]),
        depth: rec[b + 6],
    }
}

/// Validate a candidate 64-byte window for reassembly resync. The
/// report_id/sub_id/struct_version header is a 3-byte magic anchor (each checked
/// only when its `expect_*` is non-zero), plus a cheap structural bound on
/// contact_count. The raw `state` is NOT range-checked (the device's raw values
/// include 0x07, and the byte is context-dependent anyway).
fn looks_valid(
    rec: &[u8],
    expect_report_id: u8,
    expect_sub_id: u8,
    expect_struct_version: u8,
) -> bool {
    if rec.len() < PKT_SIZE {
        return false;
    }
    if expect_report_id != 0 && rec[OFF_REPORT_ID] != expect_report_id {
        return false;
    }
    if expect_sub_id != 0 && rec[OFF_SUB_ID] != expect_sub_id {
        return false;
    }
    if expect_struct_version != 0 && rec[OFF_STRUCT_VERSION] != expect_struct_version {
        return false;
    }
    (rec[OFF_CONTACT_COUNT] as usize) <= POINT_CNT
}

/// Parse a structurally-valid 64-byte record into a [`Pkt`].
fn parse_pkt(rec: &[u8]) -> Pkt {
    let cc = (rec[OFF_CONTACT_COUNT] as usize).min(POINT_CNT);
    let points = (0..cc).map(|i| read_point(rec, i)).collect();
    let timestamp = u32::from_le_bytes([
        rec[OFF_TIMESTAMP],
        rec[OFF_TIMESTAMP + 1],
        rec[OFF_TIMESTAMP + 2],
        rec[OFF_TIMESTAMP + 3],
    ]);
    Pkt { timestamp, points }
}

/// Per-contact round state.
#[derive(Debug, Default, Clone, Copy)]
struct Contact {
    prev: PointState,
    /// In a DOWN→MOVE round (set on DOWN, cleared when contact ends).
    in_round: bool,
    /// MOVE frames seen so far this round (for `settle_moves`).
    moves: u32,
    emitted: bool,
    /// 最近一次出现的坐标（缺席式抬笔的 up 事件载荷用）。
    last_x: u16,
    last_y: u16,
    /// 当前生命 id（Down 时分配,round 结束归 0；染色表的键）。
    life: u64,
}

impl PointState {
    /// wb/dstserver 的 PointState 枚举序数（DST 帧 state 字段）。
    fn code(self) -> u8 {
        match self {
            PointState::Idle => 0,
            PointState::HoverIn => 1,
            PointState::HoverMove => 2,
            PointState::Down => 3,
            PointState::Move => 4,
            PointState::Up => 5,
            PointState::HoverOut => 6,
        }
    }
}

/// 染色缓冲里的一个点（FSM 转换后的状态 + 所属 life）。
#[derive(Debug, Clone, Copy)]
struct BufPt {
    contact_id: u8,
    state: u8,
    x: u16,
    y: u16,
    /// 0 = 非 round 点（悬停等,恒 type 0）；>0 = 染色表键。
    life: u64,
}

/// 染色缓冲里的一个报文（FIFO 保序放行）。
#[derive(Debug, Clone)]
struct BufReport {
    arrival_ns: i64,
    timestamp: u32,
    pts: Vec<BufPt>,
}

/// 极简 JSON 数值字段提取（端口 I 的 {"cid":N,"pen":N}）。
fn json_num_field(json: &str, key: &str) -> Option<i64> {
    let pat = format!("\"{key}\":");
    let start = json.find(&pat)? + pat.len();
    let rest = &json[start..];
    let end = rest
        .find(|c: char| c != '-' && !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// An emitted touch event: round 首 MOVE（ev=first）、节流的持续位置
/// 更新（ev=move，emit_moves 开启时）或抬笔边沿（ev=up——km 靠它即刻
/// 除名触点+铸魂,消灭"停更但未过 TTL"的僵尸绑定窗）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvKind {
    First,
    Move,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event {
    kind: EvKind,
    contact_id: u8,
    /// 事件所在报文的上游帧序号（ts 每报文一帧,header.seq）——下游 DST
    /// 回写用它定位原始报文（帧序号注解,JSON 字段随载荷穿 tc/km）。
    src_seq: u64,
    x: u16,
    y: u16,
    depth: u8,
    raw_state: u8,
    touch_ts: u32,
}

impl Event {
    fn to_json(self) -> String {
        format!(
            r#"{{"contact_id":{},"src_seq":{},"x":{},"y":{},"depth":{},"raw_state":{},"touch_ts":{},"ev":"{}"}}"#,
            self.contact_id,
            self.src_seq,
            self.x,
            self.y,
            self.depth,
            self.raw_state,
            self.touch_ts,
            match self.kind {
                EvKind::First => "first",
                EvKind::Move => "move",
                EvKind::Up => "up",
            }
        )
    }
}

pub struct TouchTracker {
    accum: Vec<u8>,
    contacts: HashMap<u8, Contact>,
    seq: u64,
    /// Resync anchors (0 = don't check). The first three header bytes
    /// report_id/sub_id/struct_version form a 3-byte magic.
    expect_report_id: u8,
    expect_sub_id: u8,
    expect_struct_version: u8,
    /// Skip this many MOVE frames before emitting the round's coordinate
    /// (0 = the first MOVE).
    settle_moves: u32,
    /// 持续位置流：round 内每 move_every_n 个 MOVE 报文补发一条 ev=move
    /// 位置更新（km 状态匹配的数据源——写小字不抬笔时 round 永不重开，
    /// 只有位置流能反映触点实时坐标）。
    emit_moves: bool,
    move_every_n: u32,
    /// Diagnostic: log the first byte arrival once (distinguishes "no data from
    /// tcp_source" from "data but no MOVE detected").
    logged_first_bytes: bool,
    /// When true, log every state transition + emit (noisy; for debugging).
    verbose: bool,

    // ---- 染色缓冲（端口 I / dst 口）----
    /// 报文 FIFO：队首"全点已决或超时"才放行（严格保序,下游 MT 状态机）。
    dst_buf: VecDeque<BufReport>,
    /// life → pen（0/1）。epoch 结束不清（缓冲里可能还有引用）,按容量 GC。
    colors: BTreeMap<u64, u8>,
    /// 染色期：Some((锚定 cid, 锚定 pen))。按**槽**染色：期内新 Down
    /// 落在锚定槽→锚定 pen（锚定笔抬笔重落保色）,其余槽→对侧;期内 I
    /// 数据丢弃。全部 life 结束时归 None。
    epoch: Option<(u8, u8)>,
    next_life: u64,
    dst_timeout_ms: f64,
    type_pen: [u8; 2],
    dst_seq: u64,
    dst_dropped: u64,
    /// 押队取证去重：已吼过的队首 arrival（每个被押报文只警一次）。
    stall_logged_arrival: i64,
}

impl TouchTracker {
    /// Feed raw bytes, reassemble records, advance the FSM, and return any
    /// first-MOVE events produced (in order).
    fn ingest(&mut self, bytes: &[u8], src_seq: u64, now_ns: i64) -> Vec<Event> {
        self.accum.extend_from_slice(bytes);
        let mut events = Vec::new();

        let mut start = 0usize;
        while start + PKT_SIZE <= self.accum.len() {
            let rec = &self.accum[start..start + PKT_SIZE];
            if !looks_valid(
                rec,
                self.expect_report_id,
                self.expect_sub_id,
                self.expect_struct_version,
            ) {
                start += 1; // desync: slide and retry
                continue;
            }
            let pkt = parse_pkt(rec);
            self.step_fsm(&pkt, src_seq, now_ns, &mut events);
            start += PKT_SIZE;
        }

        self.accum.drain(..start);
        if self.accum.len() > ACCUM_CAP {
            let overflow = self.accum.len() - ACCUM_CAP;
            self.accum.drain(..overflow);
        }
        events
    }

    fn step_fsm(&mut self, pkt: &Pkt, src_seq: u64, now_ns: i64, events: &mut Vec<Event>) {
        let settle = self.settle_moves;
        let verbose = self.verbose;
        // 借用规避：epoch/next_life 在点循环里要用,循环后写回
        let epoch = self.epoch;
        let mut next_life = self.next_life;
        let mut new_colors: Vec<(u64, u8)> = Vec::new();
        let mut buf_pts: Vec<BufPt> = Vec::with_capacity(pkt.points.len());
        for p in &pkt.points {
            let c = self.contacts.entry(p.contact_id).or_default();
            let state = convert_state(p.raw_state, c.prev);
            let life_before = c.life;
            // Diagnostic (verbose): log state transitions so the node.log shows
            // the DOWN→MOVE progression of each tap.
            // t= 单调纳秒（与 sdc 窗口 t_trig 同域）——缺报文尸检要把
            // "缺失时刻"对到槽位占用状态上,没有时间戳这一环闭不了
            if verbose && state != c.prev {
                eprintln!(
                    "touch_tracker: c{} raw={:#04x} {:?}->{:?} x={} y={} depth={} t={}",
                    p.contact_id, p.raw_state, c.prev, state, p.x, p.y, p.depth, mono_ns()
                );
            }
            match state {
                PointState::Down => {
                    c.in_round = true;
                    c.moves = 0;
                    c.emitted = false;
                    // 新生命按槽染色：锚定槽保色,其余槽染对侧（互斥推定）
                    c.life = next_life;
                    next_life += 1;
                    if verbose {
                        eprintln!(
                            "touch_tracker: life OPEN c{} life{} @({},{}) t={}",
                            p.contact_id, c.life, p.x, p.y, now_ns
                        );
                    }
                    if let Some((anchor_cid, anchor_pen)) = epoch {
                        let pen = if p.contact_id == anchor_cid { anchor_pen } else { 1 - anchor_pen };
                        new_colors.push((c.life, pen));
                    }
                }
                PointState::Move if c.in_round => {
                    if !c.emitted && c.moves >= settle {
                        c.emitted = true;
                        if verbose {
                            eprintln!(
                                "touch_tracker: EMIT first-MOVE c{} x={} y={} depth={} ts={}",
                                p.contact_id, p.x, p.y, p.depth, pkt.timestamp
                            );
                        }
                        events.push(Event {
                            kind: EvKind::First,
                            contact_id: p.contact_id,
                            src_seq,
                            x: p.x,
                            y: p.y,
                            depth: p.depth,
                            raw_state: p.raw_state,
                            touch_ts: pkt.timestamp,
                        });
                    } else if c.emitted
                        && self.emit_moves
                        && c.moves % self.move_every_n.max(1) == 0
                    {
                        // 节流的持续位置更新（250Hz 报文 ÷ n ≈ 位置流节奏）
                        events.push(Event {
                            kind: EvKind::Move,
                            contact_id: p.contact_id,
                            src_seq,
                            x: p.x,
                            y: p.y,
                            depth: p.depth,
                            raw_state: p.raw_state,
                            touch_ts: pkt.timestamp,
                        });
                    }
                    c.moves += 1;
                }
                PointState::Move => { /* MOVE with no preceding DOWN: ignore */ }
                // Up / Idle / HoverOut / HoverIn / HoverMove: out of the
                // down→move contact phase; the next DOWN starts a fresh round.
                _ => {
                    if c.in_round && c.emitted {
                        // 抬笔边沿事件：km 即刻除名+铸魂（只对已 emit 过
                        // first 的 round 发——km 没见过的触点无需除名）
                        if verbose {
                            eprintln!("touch_tracker: EMIT up c{}", p.contact_id);
                        }
                        events.push(Event {
                            kind: EvKind::Up,
                            contact_id: p.contact_id,
                            src_seq,
                            x: p.x,
                            y: p.y,
                            depth: p.depth,
                            raw_state: p.raw_state,
                            touch_ts: pkt.timestamp,
                        });
                    } else if c.in_round {
                        // 零 MOVE 快敲：round 直接 DOWN→UP,没有 MOVE 帧可
                        // 发 first——补发 late-first(DOWN 坐标),否则 km 全盲
                        //(触点从未存在,trig 无绑定可继/热图无候选,轮必饿死;
                        // 真机双笔同敲实锤:29% 的敲击零 MOVE)。不发 up,
                        // km 靠 TTL 收尾——同批 first+up 会让触点零存活。
                        if verbose {
                            eprintln!(
                                "touch_tracker: EMIT late-first c{} x={} y={}",
                                p.contact_id, c.last_x, c.last_y
                            );
                        }
                        events.push(Event {
                            kind: EvKind::First,
                            contact_id: p.contact_id,
                            src_seq,
                            x: c.last_x,
                            y: c.last_y,
                            depth: p.depth,
                            raw_state: p.raw_state,
                            touch_ts: pkt.timestamp,
                        });
                    }
                    if verbose && life_before > 0 {
                        eprintln!(
                            "touch_tracker: life CLOSE c{} life{} ({:?}) t={}",
                            p.contact_id, life_before, state, now_ns
                        );
                    }
                    c.in_round = false;
                    c.moves = 0;
                    c.emitted = false;
                    c.life = 0;
                }
            }
            c.prev = state;
            c.last_x = p.x;
            c.last_y = p.y;
            // 染色缓冲取"该点所属生命"：Down 用新分配的,收尾用归零前的
            let pt_life = if state == PointState::Down { c.life } else { life_before.max(c.life) };
            buf_pts.push(BufPt {
                contact_id: p.contact_id,
                state: state.code(),
                x: p.x,
                y: p.y,
                life: pt_life,
            });
        }
        // 缺席式抬笔：设备抬笔常不发 UP 报文、直接停报该触点（wb 早有同
        // 语义兜底）。报文流还在（本报文有别的点）而 in_round 触点缺席
        // → 视作抬笔,补发 ev=up——否则 km 的僵尸绑定要挨到 TTL,快写间隙
        // 内 trig/热图会把分类喂给上一轮（真机双笔同时写 253 饿死轮实锤,
        // up 缺失 ~290 划恰与之吻合）。
        for (&cid, c) in self.contacts.iter_mut() {
            if !c.in_round || pkt.points.iter().any(|p| p.contact_id == cid) {
                continue;
            }
            if c.emitted {
                if verbose {
                    eprintln!("touch_tracker: EMIT up c{cid} (absent from report)");
                }
                events.push(Event {
                    kind: EvKind::Up,
                    contact_id: cid,
                    src_seq,
                    x: c.last_x,
                    y: c.last_y,
                    depth: 0,
                    raw_state: 0,
                    touch_ts: pkt.timestamp,
                });
            } else {
                // 缺席收尾的零 MOVE 快敲同样补发 late-first（见边沿路径）
                if verbose {
                    eprintln!(
                        "touch_tracker: EMIT late-first c{cid} x={} y={} (absent)",
                        c.last_x, c.last_y
                    );
                }
                events.push(Event {
                    kind: EvKind::First,
                    contact_id: cid,
                    src_seq,
                    x: c.last_x,
                    y: c.last_y,
                    depth: 0,
                    raw_state: 0,
                    touch_ts: pkt.timestamp,
                });
            }
            if verbose && c.life > 0 {
                eprintln!(
                    "touch_tracker: life CLOSE c{cid} life{} (absent-from-report) t={now_ns}",
                    c.life
                );
            }
            c.in_round = false;
            c.moves = 0;
            c.emitted = false;
            c.prev = PointState::Idle;
            c.life = 0;
        }

        // 写回借用规避的局部量
        self.next_life = next_life;
        for (life, pen) in new_colors {
            self.colors.insert(life, pen);
        }
        while self.colors.len() > COLOR_CAP {
            self.colors.pop_first();
        }

        // 报文入染色缓冲（含悬停点；放行时非 life 点恒 type 0）
        if self.dst_buf.len() >= DST_BUF_CAP {
            self.dst_buf.pop_front();
            self.dst_dropped += 1;
            if self.dst_dropped % 100 == 1 {
                eprintln!("touch_tracker: dst buffer overflow ({} dropped)", self.dst_dropped);
            }
        }
        self.dst_buf.push_back(BufReport {
            arrival_ns: now_ns,
            timestamp: pkt.timestamp,
            pts: buf_pts,
        });

        // 染色期终点：全部生命结束（无 in_round 触点）
        if self.epoch.is_some() && !self.contacts.values().any(|c| c.in_round) {
            if self.verbose {
                eprintln!("touch_tracker: epoch end (all lives closed)");
            }
            self.epoch = None;
        }
    }

    /// 端口 I：{"cid":N,"pen":0|1}。epoch 存续期一律丢弃；无 epoch 时锚定
    /// cid 当前生命 = pen,其余在场生命 = 对侧,并开启 epoch。
    fn handle_pen_assign(&mut self, json: &str) {
        if self.epoch.is_some() {
            if self.verbose {
                eprintln!("touch_tracker: pen assign dropped (epoch active): {json}");
            }
            return;
        }
        let (Some(cid), Some(pen)) = (json_num_field(json, "cid"), json_num_field(json, "pen"))
        else {
            eprintln!("touch_tracker: malformed pen assign: {json}");
            return;
        };
        let (cid, pen) = (cid as u8, (pen as u8) & 1);
        let Some(anchor_life) = self.contacts.get(&cid).filter(|c| c.in_round && c.life > 0).map(|c| c.life)
        else {
            if self.verbose {
                eprintln!("touch_tracker: pen assign for absent c{cid} dropped (stale)");
            }
            return;
        };
        let peer = 1 - pen;
        self.colors.insert(anchor_life, pen);
        for c in self.contacts.values() {
            if c.in_round && c.life > 0 && c.life != anchor_life {
                self.colors.insert(c.life, peer);
            }
        }
        self.epoch = Some((cid, pen));
        if self.verbose {
            eprintln!(
                "touch_tracker: epoch start — anchor c{cid} (life {anchor_life}) = pen{pen}, other slots = pen{peer}"
            );
        }
    }

    /// dst 口放行：队首"全点已决或超时"才出队（严格保序）。一次 process
    /// 里把当下全部可放行报文拼成一个字节帧（下游按 version 自定界）。
    fn flush_dst(&mut self, now_ns: i64) -> Option<(Vec<u8>, u32)> {
        let timeout_ns = (self.dst_timeout_ms * 1e6) as i64;
        let mut out = Vec::new();
        let mut n = 0u32;
        while let Some(head) = self.dst_buf.front() {
            let resolved = head
                .pts
                .iter()
                .all(|p| p.life == 0 || self.colors.contains_key(&p.life));
            let age_ms = (now_ns - head.arrival_ns) as f64 / 1e6;
            if !resolved && now_ns - head.arrival_ns < timeout_ns {
                // 押队取证：队首押过 30ms 就不正常（bind_classify 中位
                // 5-12ms）——每报文只吼一次,谁在等、等多久、epoch 何态
                if self.verbose && age_ms > 30.0 && self.stall_logged_arrival != head.arrival_ns {
                    self.stall_logged_arrival = head.arrival_ns;
                    let waiting: Vec<String> = head
                        .pts
                        .iter()
                        .filter(|p| p.life > 0 && !self.colors.contains_key(&p.life))
                        .map(|p| format!("c{}/life{}", p.contact_id, p.life))
                        .collect();
                    eprintln!(
                        "touch_tracker: dst head STALLED {age_ms:.0}ms waiting=[{}] epoch={:?} buf={} t={now_ns}",
                        waiting.join(","),
                        self.epoch,
                        self.dst_buf.len()
                    );
                }
                break;
            }
            if !resolved && self.verbose {
                // 超时放行（type 0 白）：短笔/白头尸检的直接证词
                let uncolored: Vec<String> = head
                    .pts
                    .iter()
                    .filter(|p| p.life > 0 && !self.colors.contains_key(&p.life))
                    .map(|p| format!("c{}/life{}", p.contact_id, p.life))
                    .collect();
                eprintln!(
                    "touch_tracker: dst TIMEOUT-flush age={age_ms:.0}ms uncolored=[{}] epoch={:?} buf={} t={now_ns}",
                    uncolored.join(","),
                    self.epoch,
                    self.dst_buf.len()
                );
            }
            let rep = self.dst_buf.pop_front().unwrap();
            out.extend_from_slice(&self.build_dst(&rep));
            n += 1;
        }
        if n > 0 {
            Some((out, n))
        } else {
            None
        }
    }

    /// 与 wb 同款双版本 dst 帧：≤2 点 v1 23B / >2 点 v2 44B。
    fn build_dst(&self, r: &BufReport) -> Vec<u8> {
        let (ver, cap, size) = if r.pts.len() <= 2 {
            (1u32, 2usize, 23usize)
        } else {
            (2u32, 5usize, 44usize)
        };
        let mut d = vec![0u8; size];
        d[0..4].copy_from_slice(&ver.to_le_bytes());
        d[4..8].copy_from_slice(&r.timestamp.to_le_bytes());
        d[8] = r.pts.len().min(cap) as u8;
        for (j, p) in r.pts.iter().take(cap).enumerate() {
            let o = 9 + j * 7;
            let type_id = if p.life == 0 {
                0
            } else {
                self.colors
                    .get(&p.life)
                    .map(|&pen| self.type_pen[pen as usize])
                    .unwrap_or(0)
            };
            d[o] = p.contact_id;
            d[o + 1] = p.state;
            d[o + 2] = type_id;
            d[o + 3..o + 5].copy_from_slice(&p.x.to_le_bytes());
            d[o + 5..o + 7].copy_from_slice(&p.y.to_le_bytes());
        }
        d
    }
}

impl Plugin for TouchTracker {
    fn new(_manifest: &PluginManifest) -> Self {
        TouchTracker {
            accum: Vec::new(),
            contacts: HashMap::new(),
            seq: 0,
            expect_report_id: 0,
            expect_sub_id: 0,
            expect_struct_version: 0,
            settle_moves: 0,
            emit_moves: false,
            move_every_n: 5,
            logged_first_bytes: false,
            verbose: false,
            dst_buf: VecDeque::new(),
            colors: BTreeMap::new(),
            epoch: None,
            next_life: 1,
            dst_timeout_ms: 100.0,
            type_pen: [1, 2],
            dst_seq: 0,
            dst_dropped: 0,
            stall_logged_arrival: 0,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        let u32v = |v: &ParamValue| match v {
            ParamValue::U32(x) => Some(*x),
            _ => None,
        };
        match id {
            "report_id" => {
                if let Some(v) = u32v(value) {
                    self.expect_report_id = (v & 0xff) as u8;
                }
            }
            "sub_id" => {
                if let Some(v) = u32v(value) {
                    self.expect_sub_id = (v & 0xff) as u8;
                }
            }
            "struct_version" => {
                if let Some(v) = u32v(value) {
                    self.expect_struct_version = (v & 0xff) as u8;
                }
            }
            "settle_moves" => {
                if let Some(v) = u32v(value) {
                    self.settle_moves = v;
                }
            }
            "emit_moves" => {
                if let ParamValue::Bool(b) = value {
                    self.emit_moves = *b;
                }
            }
            "move_every_n" => {
                if let Some(v) = u32v(value) {
                    self.move_every_n = v.clamp(1, 50);
                }
            }
            "verbose" => {
                if let ParamValue::Bool(b) = value {
                    self.verbose = *b;
                }
            }
            "dst_timeout_ms" => {
                if let Some(v) = u32v(value) {
                    self.dst_timeout_ms = v as f64;
                }
            }
            "type_id_pen0" => {
                if let Some(v) = u32v(value) {
                    self.type_pen[0] = v as u8;
                }
            }
            "type_id_pen1" => {
                if let Some(v) = u32v(value) {
                    self.type_pen[1] = v as u8;
                }
            }
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        self.accum.clear();
        self.contacts.clear();
        self.seq = 0;
        self.logged_first_bytes = false;
        self.dst_buf.clear();
        self.colors.clear();
        self.epoch = None;
        self.next_life = 1;
        self.dst_seq = 0;
        self.dst_dropped = 0;
        self.stall_logged_arrival = 0;
        ProcessOutcome::Ok
    }

    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        let now = mono_ns();

        // 端口 I（pen_id）先于报文：指派引用的是已知生命,不能吃到本 tick
        // 新开的 life。epoch 存续期数据在 handle 内直接丢弃。
        if let Some(f) = inputs.get(1) {
            if f.header.n_samples > 0 {
                if let Ok(text) = std::str::from_utf8(f.samples()) {
                    for line in text.lines().filter(|l| !l.trim().is_empty()) {
                        self.handle_pen_assign(line);
                    }
                }
            }
        }

        let mut arrival_ns = now;
        let mut events = Vec::new();
        if let Some(input) = inputs.first() {
            let bytes = input.samples();
            if input.header.n_samples > 0 && !bytes.is_empty() {
                if !self.logged_first_bytes {
                    self.logged_first_bytes = true;
                    eprintln!(
                        "touch_tracker: receiving data ({} bytes first frame)",
                        bytes.len()
                    );
                }
                arrival_ns = input.header.t0_ns;
                events = self.ingest(bytes, input.header.seq, now);
            }
        }

        // 同帧多事件（双触点同帧首 MOVE / 位置流批次）合成 JSON Lines 一帧
        // ——下游按 .lines() 解析（km/wb/合并器均已如此）。
        if !events.is_empty() {
            if let Some(out) = outputs.first_mut() {
                let json: Vec<String> = events.iter().map(|e| e.to_json()).collect();
                out.write(json.join("\n").as_bytes());
                out.header.seq = self.seq;
                out.header.t0_ns = arrival_ns; // pairing time = report arrival
                // n_samples = 事件行数。下游（wb 等）按 n_samples>0 门控——
                // 事件模型时代这里靠 tc 合帧补 1,tc 退役后必须源头自证。
                out.header.n_samples = events.len() as u32;
                self.seq += 1;
            }
        }

        // dst 口：放行全部"已决或超时"的队首报文（tick_driven 空拍也会
        // 走到这里,超时放行不依赖新报文到达）。
        if let Some((bytes, n)) = self.flush_dst(now) {
            if let Some(out) = outputs.get_mut(1) {
                out.write(&bytes);
                out.header.seq = self.dst_seq;
                out.header.t0_ns = now;
                out.header.n_samples = n;
                self.dst_seq += 1;
            }
        }
        ProcessOutcome::Ok
    }
}

sigflow_plugin_sdk::export_plugin!(TouchTracker);

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    /// Build a 64-byte report with one active point at the given raw state.
    fn rec_one(raw_state: u8, contact_id: u8, x: u16, y: u16, depth: u8, ts: u32) -> Vec<u8> {
        let mut r = vec![0u8; PKT_SIZE];
        r[OFF_REPORT_ID] = 0xFB;
        r[OFF_SUB_ID] = 0xB8;
        r[OFF_STRUCT_VERSION] = 0x04;
        r[OFF_CONTACT_COUNT] = 1;
        let b = OFF_POINTS;
        r[b] = raw_state;
        r[b + 1] = contact_id;
        r[b + 2..b + 4].copy_from_slice(&x.to_le_bytes());
        r[b + 4..b + 6].copy_from_slice(&y.to_le_bytes());
        r[b + 6] = depth;
        r[OFF_TIMESTAMP..OFF_TIMESTAMP + 4].copy_from_slice(&ts.to_le_bytes());
        r
    }

    fn tracker() -> TouchTracker {
        TouchTracker::new(&manifest())
    }

    #[test]
    fn convert_down_up_byte_is_edge_ambiguous() {
        // 0x04 from hover/idle → DOWN; from down/move → UP.
        assert_eq!(convert_state(RAW_DOWN_UP, PointState::HoverMove), PointState::Down);
        assert_eq!(convert_state(RAW_DOWN_UP, PointState::Idle), PointState::Down);
        assert_eq!(convert_state(RAW_DOWN_UP, PointState::Down), PointState::Up);
        assert_eq!(convert_state(RAW_DOWN_UP, PointState::Move), PointState::Up);
        assert_eq!(convert_state(RAW_MOVE, PointState::Down), PointState::Move);
    }

    #[test]
    fn manifest_has_consumer_and_producer_ports() {
        let m = manifest();
        assert_eq!(m.ports.len(), 4);
        assert_eq!(m.ports[0].id, "bytes_in");
        assert_eq!(m.ports[1].id, "pen_id");
        assert_eq!(m.ports[2].id, "touch_event");
        assert_eq!(m.ports[3].id, "dst");
    }

    /// The real-tap raw run: 0(hover)×2, 4(DOWN), 7(MOVE), 7, 2(HOVER_OUT), 0.
    /// First MOVE = the first 0x07 after the 0x04 DOWN.
    #[test]
    fn emits_first_move_of_real_tap_sequence() {
        let mut t = tracker();
        let mut s = Vec::new();
        s.extend(rec_one(0x00, 0, 20760, 16226, 43, 1)); // HOVER_IN
        s.extend(rec_one(0x00, 0, 20759, 16241, 54, 2)); // HOVER_MOVE
        s.extend(rec_one(0x04, 0, 20739, 16240, 69, 3)); // DOWN
        s.extend(rec_one(0x07, 0, 20743, 16243, 100, 4)); // MOVE → event
        s.extend(rec_one(0x07, 0, 20711, 16140, 100, 5)); // MOVE (no new event)
        s.extend(rec_one(0x02, 0, 20000, 16000, 100, 6)); // HOVER_OUT
        let ev = t.ingest(&s, 0, 0);
        assert_eq!(ev.len(), 2, "first + up 边沿");
        assert_eq!((ev[0].x, ev[0].y), (20743, 16243)); // the first MOVE, not the DOWN
        assert_eq!(ev[0].raw_state, 0x07);
        assert_eq!(ev[0].touch_ts, 4);
        assert_eq!(ev[1].kind, EvKind::Up, "round 结束发抬笔边沿");
        assert!(ev[1].to_json().contains(r#""ev":"up""#));
    }

    #[test]
    fn down_without_move_emits_late_first() {
        // 0x04 (DOWN) then 0x04 again (→UP) — 零 MOVE 快敲。曾按"first-MOVE
        // 模型"静默,真机双笔同敲实锤 29% 敲击零 MOVE、km 全盲轮必饿死——
        // 现在 round 关闭时补发 late-first(DOWN 坐标),不发 up(km TTL 收尾)。
        let mut t = tracker();
        let mut s = Vec::new();
        s.extend(rec_one(0x00, 0, 1, 1, 40, 1)); // HOVER_IN
        s.extend(rec_one(0x04, 0, 2, 2, 60, 2)); // DOWN
        s.extend(rec_one(0x04, 0, 2, 2, 0, 3)); // UP
        let ev = t.ingest(&s, 0, 0);
        assert_eq!(ev.len(), 1, "late-first only: {ev:?}");
        assert_eq!(ev[0].kind, EvKind::First);
        assert_eq!((ev[0].x, ev[0].y), (2, 2), "DOWN 坐标");
        assert!(ev[0].to_json().contains(r#""ev":"first""#));
    }

    #[test]
    fn stray_move_without_down_is_ignored() {
        let mut t = tracker();
        let ev = t.ingest(&rec_one(0x07, 0, 5, 5, 100, 1), 0, 0); // MOVE, no DOWN
        assert!(ev.is_empty());
    }

    #[test]
    fn second_round_emits_again() {
        let mut t = tracker();
        let mut s = Vec::new();
        s.extend(rec_one(0x04, 0, 10, 20, 80, 1)); // DOWN (from idle)
        s.extend(rec_one(0x07, 0, 11, 21, 100, 2)); // MOVE → event
        s.extend(rec_one(0x04, 0, 11, 21, 0, 3)); // UP (prev MOVE)
        s.extend(rec_one(0x00, 0, 0, 0, 30, 4)); // HOVER_IN
        s.extend(rec_one(0x04, 0, 30, 40, 80, 5)); // DOWN again
        s.extend(rec_one(0x07, 0, 31, 41, 100, 6)); // MOVE → event
        let ev = t.ingest(&s, 0, 0);
        assert_eq!(ev.len(), 3, "first + up + first");
        assert_eq!((ev[0].x, ev[0].y), (11, 21));
        assert_eq!(ev[1].kind, EvKind::Up);
        assert_eq!((ev[2].x, ev[2].y), (31, 41));
    }

    #[test]
    fn settle_moves_skips_initial_moves() {
        let mut t = tracker();
        t.settle_moves = 1; // emit the 2nd MOVE
        let mut s = Vec::new();
        s.extend(rec_one(0x04, 0, 1, 1, 80, 1)); // DOWN
        s.extend(rec_one(0x07, 0, 11, 11, 100, 2)); // 1st MOVE, skipped
        s.extend(rec_one(0x07, 0, 22, 22, 100, 3)); // 2nd MOVE → event
        let ev = t.ingest(&s, 0, 0);
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].x, ev[0].y), (22, 22));
    }

    /// 双触点同报文（真协议形态:活跃触点全在 points[2] 里——缺席检测
    /// 依赖这一点,单触点分报会被误判抬笔）。
    fn rec_two(pts: [(u8, u8, u16, u16); 2], ts: u32) -> Vec<u8> {
        let mut r = vec![0u8; PKT_SIZE];
        r[OFF_REPORT_ID] = 0xFB;
        r[OFF_SUB_ID] = 0xB8;
        r[OFF_STRUCT_VERSION] = 0x04;
        r[OFF_CONTACT_COUNT] = 2;
        for (i, (raw_state, cid, x, y)) in pts.iter().enumerate() {
            let b = OFF_POINTS + i * POINT_SIZE;
            r[b] = *raw_state;
            r[b + 1] = *cid;
            r[b + 2..b + 4].copy_from_slice(&x.to_le_bytes());
            r[b + 4..b + 6].copy_from_slice(&y.to_le_bytes());
            r[b + 6] = 80;
        }
        r[OFF_TIMESTAMP..OFF_TIMESTAMP + 4].copy_from_slice(&ts.to_le_bytes());
        r
    }

    #[test]
    fn two_contacts_each_emit_independently() {
        let mut t = tracker();
        let mut s = Vec::new();
        s.extend(rec_two([(0x04, 0, 1, 1), (0x04, 1, 2, 2)], 1)); // 双 DOWN
        s.extend(rec_two([(0x07, 0, 11, 11), (0x07, 1, 22, 22)], 2)); // 双 MOVE
        let ev = t.ingest(&s, 0, 0);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev.iter().find(|e| e.contact_id == 0).unwrap().x, 11);
        assert_eq!(ev.iter().find(|e| e.contact_id == 1).unwrap().x, 22);
    }

    /// 缺席式抬笔：对侧触点还在报而 in_round 触点从报文里消失 → 补发
    /// ev=up（真机双笔实锤:抬笔常无 UP 报文直接停报,僵尸绑定要挨 TTL）。
    #[test]
    fn absent_contact_emits_up() {
        let mut t = tracker();
        let mut s = Vec::new();
        s.extend(rec_two([(0x04, 0, 1, 1), (0x04, 1, 2, 2)], 1));
        s.extend(rec_two([(0x07, 0, 11, 11), (0x07, 1, 22, 22)], 2));
        let ev = t.ingest(&s, 0, 0);
        assert_eq!(ev.len(), 2);
        // c1 抬笔:设备停报它,报文只剩 c0
        let ev = t.ingest(&rec_one(0x07, 0, 12, 12, 100, 3), 1, 0);
        assert_eq!(ev.len(), 1, "缺席补发 up: {ev:?}");
        assert_eq!(ev[0].kind, EvKind::Up);
        assert_eq!(ev[0].contact_id, 1);
        assert_eq!((ev[0].x, ev[0].y), (22, 22), "up 载荷=最后已知坐标");
        // c1 再落 → 新 round 正常 first
        let mut s = Vec::new();
        s.extend(rec_two([(0x07, 0, 13, 13), (0x04, 1, 30, 30)], 4));
        s.extend(rec_two([(0x07, 0, 14, 14), (0x07, 1, 31, 31)], 5));
        let ev = t.ingest(&s, 2, 0);
        assert!(ev.iter().any(|e| e.kind == EvKind::First && e.contact_id == 1));
    }

    #[test]
    fn resyncs_after_length_prefix_and_junk_with_magic() {
        let mut t = tracker();
        t.expect_report_id = 0xFB;
        t.expect_sub_id = 0xB8;
        t.expect_struct_version = 0x04;
        // 4-byte big-endian length prefix the TCP server prepends, plus junk.
        let mut s = vec![0x00u8, 0x00, 0x00, 0x40, 0xFF, 0xAB];
        s.extend(rec_one(0x04, 0, 0, 0, 80, 1)); // DOWN
        s.extend(rec_one(0x07, 0, 99, 88, 100, 2)); // MOVE → event
        let ev = t.ingest(&s, 0, 0);
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].x, ev[0].y), (99, 88));
    }

    #[test]
    fn split_record_across_two_ingests_reassembles() {
        let mut t = tracker();
        let down = rec_one(0x04, 0, 0, 0, 80, 1);
        let mv = rec_one(0x07, 0, 7, 8, 100, 2);
        assert!(t.ingest(&down, 0, 0).is_empty()); // DOWN, no MOVE yet
        assert!(t.ingest(&mv[..30], 0, 0).is_empty()); // partial MOVE held back
        let ev = t.ingest(&mv[30..], 0, 0); // completes it
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].x, ev[0].y), (7, 8));
    }

    #[test]
    fn process_emits_event_frame_with_arrival_time() {
        let mut t = tracker();
        let mut stream = Vec::new();
        stream.extend(rec_one(0x04, 0, 1, 1, 80, 6)); // DOWN
        stream.extend(rec_one(0x07, 0, 640, 480, 100, 7)); // MOVE
        let mut h = sigflow_plugin_sdk::FrameHeader::ZERO;
        h.t0_ns = 1_234_567;
        h.n_samples = stream.len() as u32;
        let inputs = [Frame::new(h, &stream)];
        let mut buf = vec![0u8; 256];
        let mut outs = [FrameOut::new(&mut buf)];
        let _ = t.process(&inputs, &mut outs);
        let w = outs[0].written();
        assert!(w > 0);
        assert_eq!(outs[0].header.t0_ns, 1_234_567);
        // 下游（wb 等）按 n_samples>0 门控,必须源头自证（回归防护:
        // 曾因透传 0 让 wb 静默丢弃全部匹配）
        assert!(outs[0].header.n_samples > 0);
        let json = std::str::from_utf8(&outs[0].buffer_mut()[..w]).unwrap();
        assert!(json.contains(r#""x":640"#));
        assert!(json.contains(r#""raw_state":7"#));
        assert!(json.contains(r#""touch_ts":7"#));
    }

    /// emit_moves：round 内每 move_every_n 个 MOVE 补发一条 ev=move 位置
    /// 更新（首 MOVE 仍为 ev=first）；同批多事件合成 JSON Lines 一帧。
    #[test]
    fn move_stream_emits_throttled_updates() {
        let mut t = tracker();
        t.set_param("emit_moves", &ParamValue::Bool(true));
        t.set_param("move_every_n", &ParamValue::U32(2));
        let mut bytes = rec_one(0x04, 1, 100, 100, 5, 1); // DOWN
        for i in 0..6u16 {
            // MOVE ×6,坐标递增
            bytes.extend_from_slice(&rec_one(0x07, 1, 100 + i * 10, 100, 5, 2 + i as u32));
        }
        let ev = t.ingest(&bytes, 42, 0);
        // moves 计数 0..5：#0 首发(ev=first)、#2/#4 节流位置流(ev=move)
        assert_eq!(ev.len(), 3, "first + 2 throttled moves: {ev:?}");
        assert!(
            ev[0].kind == EvKind::First && ev[1].kind == EvKind::Move && ev[2].kind == EvKind::Move
        );
        assert_eq!((ev[1].x, ev[2].x), (120, 140), "moves carry the live coordinate");
        let j = ev[1].to_json();
        assert!(j.contains(r#""ev":"move""#), "{j}");
        assert!(ev[0].to_json().contains(r#""ev":"first""#));
    }

    // ---- 染色缓冲 ----

    /// 无指派时报文押在缓冲里；I 口指派到达 → 锚定槽染 pen、在场对侧槽
    /// 染 1-pen，缓冲立即放行（type_id 默认映射 pen0→1/pen1→2）。
    #[test]
    fn pen_assign_colors_buffer_and_flushes_with_mutex() {
        let mut t = tracker();
        let mut s = Vec::new();
        s.extend(rec_two([(0x04, 0, 1, 1), (0x04, 1, 2, 2)], 1)); // 双 DOWN
        s.extend(rec_two([(0x07, 0, 11, 11), (0x07, 1, 22, 22)], 2)); // 双 MOVE
        t.ingest(&s, 0, 1_000);
        assert!(t.flush_dst(2_000).is_none(), "未指派未超时:押住");
        t.handle_pen_assign(r#"{"cid":1,"pen":0}"#);
        let (bytes, n) = t.flush_dst(3_000).expect("指派后立即放行");
        assert_eq!(n, 2, "两个报文都放行");
        assert_eq!(bytes.len(), 46, "2 点报文 → v1 23B ×2");
        // 报文1: [9]=cid0 [11]=type, [16]=cid1 [18]=type
        assert_eq!(bytes[9], 0, "point0 = c0");
        assert_eq!(bytes[11], 2, "c0 = 对侧 pen1 → type 2");
        assert_eq!(bytes[16], 1, "point1 = c1");
        assert_eq!(bytes[18], 1, "锚定 c1 = pen0 → type 1");
    }

    /// 染色期存续中 I 口数据一律丢弃（禁翻转）；全部生命结束后 epoch
    /// 关闭,新指派重新受理。
    #[test]
    fn epoch_discards_assigns_until_all_lives_end() {
        let mut t = tracker();
        t.ingest(&rec_one(0x04, 0, 1, 1, 80, 1), 0, 0); // c0 DOWN
        t.handle_pen_assign(r#"{"cid":0,"pen":0}"#);
        assert_eq!(t.epoch, Some((0, 0)));
        // 期内翻转尝试 → 丢弃
        t.handle_pen_assign(r#"{"cid":0,"pen":1}"#);
        let (bytes, _) = t.flush_dst(1).unwrap();
        assert_eq!(bytes[11], 1, "保持 pen0 → type 1,翻转被丢");
        // c0 UP → 全部生命结束 → epoch 关闭
        t.ingest(&rec_one(0x04, 0, 1, 1, 0, 2), 0, 0);
        assert_eq!(t.epoch, None, "生命清零后染色期结束");
        // 新生命 + 新指派受理
        t.ingest(&rec_one(0x04, 0, 5, 5, 80, 3), 1, 0);
        t.handle_pen_assign(r#"{"cid":0,"pen":1}"#);
        let (bytes, _) = t.flush_dst(1).unwrap();
        let last = &bytes[bytes.len() - 23..];
        assert_eq!(last[11], 2, "新一轮指派 pen1 → type 2");
    }

    /// epoch 内新落下的生命自动染对侧（互斥推定,I 口不再说话）。
    #[test]
    fn life_born_during_epoch_gets_peer_pen() {
        let mut t = tracker();
        t.ingest(&rec_one(0x04, 0, 1, 1, 80, 1), 0, 0); // c0 DOWN
        t.handle_pen_assign(r#"{"cid":0,"pen":1}"#); // c0=pen1, epoch peer=pen0
        t.flush_dst(1);
        // c1 在 epoch 内落下（c0 还按着——同报文两点）
        let mut s = Vec::new();
        s.extend(rec_two([(0x07, 0, 2, 2), (0x04, 1, 9, 9)], 2));
        t.ingest(&s, 1, 0);
        let (bytes, _) = t.flush_dst(1).expect("epoch 内即到即决");
        assert_eq!(bytes[16], 1, "point1 = c1");
        assert_eq!(bytes[18], 1, "epoch 内新生命 → 对侧 pen0 → type 1");
    }

    /// 无指派 → 队首押满 dst_timeout_ms 后以 type 0 放行（严格保序）。
    #[test]
    fn timeout_flushes_uncolored_as_type_zero() {
        let mut t = tracker();
        t.ingest(&rec_one(0x04, 0, 1, 1, 80, 1), 0, 1_000_000);
        assert!(t.flush_dst(1_000_000 + 99_000_000).is_none(), "99ms:未超时");
        let (bytes, n) = t.flush_dst(1_000_000 + 101_000_000).expect("101ms:超时放行");
        assert_eq!(n, 1);
        assert_eq!(bytes[11], 0, "未染色 → type 0");
        assert_eq!(bytes[10], 3, "state = Down(3)");
    }

    /// 指派引用不在场的 cid → 陈旧丢弃,不建 epoch。
    #[test]
    fn assign_for_absent_contact_is_dropped() {
        let mut t = tracker();
        t.handle_pen_assign(r#"{"cid":3,"pen":0}"#);
        assert_eq!(t.epoch, None);
        assert!(t.colors.is_empty());
    }

    /// 悬停点（非 life）不押缓冲节奏：恒 type 0 即决放行。
    #[test]
    fn hover_reports_flush_immediately_as_type_zero() {
        let mut t = tracker();
        t.ingest(&rec_one(0x00, 0, 7, 7, 40, 1), 0, 0); // HOVER_IN
        let (bytes, n) = t.flush_dst(1).expect("悬停即决");
        assert_eq!(n, 1);
        assert_eq!(bytes[10], 1, "state = HoverIn(1)");
        assert_eq!(bytes[11], 0);
    }

    #[test]
    fn set_param_configures_anchors_and_settle() {
        let mut t = tracker();
        t.set_param("report_id", &ParamValue::U32(0xFB));
        t.set_param("sub_id", &ParamValue::U32(0xB8));
        t.set_param("struct_version", &ParamValue::U32(0x04));
        t.set_param("settle_moves", &ParamValue::U32(3));
        assert_eq!(t.expect_report_id, 0xFB);
        assert_eq!(t.expect_sub_id, 0xB8);
        assert_eq!(t.expect_struct_version, 0x04);
        assert_eq!(t.settle_moves, 3);
    }
}
