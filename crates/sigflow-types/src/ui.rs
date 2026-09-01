//! UI widget instance schema.
//!
//! A widget is an instance of a `PluginCategory::UiWidget` plugin attached to a
//! node. Multiple instances per node, distinguished by `alias`. Bindings to
//! the node's compute plugin (param/port/state/action) are added separately
//! via `widget bind`; a widget with `bind = None` is an orphan, which is a
//! legal state.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "ts")]
use ts_rs::TS;

// ---------------------------------------------------------------------------
// Widget instance (persisted in node.toml under `[[widgets]]`).
// ---------------------------------------------------------------------------

/// A single widget instance attached to a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct UiWidget {
    /// Unique within the node. Doubles as a human label.
    pub alias: String,
    /// Widget plugin name (e.g. `sigflow.ui.slider`). Single-version globally
    /// so we do not lock to a content hash here.
    pub plugin: String,
    /// Version recorded at install time, informational.
    pub version: String,
    pub layout: Layout,
    /// `None` means the widget is an orphan (legal state — added but not yet
    /// bound to anything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub bind: Option<WidgetBinding>,
    /// Free-form widget-instance config (e.g. `{"orientation": "horizontal"}`).
    /// Validated against the widget plugin's `config_schema` at `widget_add`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub config: HashMap<String, serde_json::Value>,
}

/// How a widget instance connects to a node's compute surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct WidgetBinding {
    pub kind: BindKind,
    /// Descendant of the host node that owns the target, as a `/`-separated
    /// child-id path relative to the host (`"dsp"`, `"dsp/filter"`). `None`
    /// binds the host node itself. Relative — like `ParentPortDecl.bind_node`
    /// — so bindings survive copy/export/import of the host container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub node: Option<String>,
    /// Param id, port id, action id, document id, or empty string for
    /// `State` / `Logs`.
    pub target: String,
    /// Only meaningful when `kind == Port` — tap subscription config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub tap: Option<TapConfig>,
    /// Only meaningful when `kind == Port` — 示波器的 setup（[`ScopeConfig`]）。
    /// 一个口绑定要么是 tap（订阅）要么是 scope（采集引擎），按控件类型二选一。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub scope: Option<ScopeConfig>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum BindKind {
    Param,
    Port,
    State,
    Action,
    /// The node's log stream (`node.log`), read over `get_logs`. Like `State`
    /// it addresses the node as a whole, so `target` is empty.
    Logs,
    /// A configuration document the node's plugin declares
    /// (`PluginManifest::documents`); `target` is the document id. Unlike a
    /// param binding, what the widget receives is a schema plus a tree, not
    /// one scalar.
    Doc,
}

impl BindKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BindKind::Param => "param",
            BindKind::Port => "port",
            BindKind::State => "state",
            BindKind::Action => "action",
            BindKind::Logs => "logs",
            BindKind::Doc => "doc",
        }
    }

    /// Whether this kind addresses the node itself rather than a named item on
    /// its compute surface (and so takes an empty `target`).
    pub fn is_whole_node(self) -> bool {
        matches!(self, BindKind::State | BindKind::Logs)
    }
}

/// Logical position and size on the node UI panel.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct Layout {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 40.0,
        }
    }
}

/// 一条 tap = 一个观察者对一个口的订阅：按模式把样本摘给它。
///
/// 一个口上可以开任意多份 tap，每份自己选列（那是带宽）。**触发、深存储、
/// 测量不在这里**——那些是示波器的事（[`ScopeConfig`]：住在生产口所在壳体
/// 里的独立采集引擎，见 sigflow-core `docs/scope.md`）。配置持久化在绑定它的
/// 控件的 `bind.tap` 里（node.toml），也就是 CLI `widget bind` 的
/// `--window/--refresh/--mode/--channels` 写的那一份；绑定里没有 tap 段时前端
/// 按控件类型兜底。
///
/// 三种模式（[`TapMode`]；`mode` 缺省由 `window_samples` 推：0 = frame，
/// > 0 = window）：
/// - window：最近 N 拍的环，按 `refresh_hz` 出窗（无触发；要触发用示波器）。
/// - frame：生产方每帧原样转发——生产方自己开窗的口（热图、估计）。
/// - stream：每一拍都发，按 `refresh_hz` 打包并带样本轴元数据（首样本序号 /
///   通道数 / 断），客户端自己拼一段可回翻、带断标的历史。
///
/// **几何** = `mode` + `window_samples` + `channels`：三者决定环的布局，改了
/// 必须重建（攒的窗丢掉，见 [`TapConfig::same_geometry`]）；其余项
/// （`refresh_hz` / `stats`）就地生效。run/stop 是观察者的运行态，不是声明：
/// stop 时 stream 累加器里的样本带 STOPPED 位冲出，run 之后第一帧带
/// DISCONTINUITY（停着的那段时间不存在）。
///
/// 整数 dtype 的口（见 [`crate::semantic::Dtype`]）tap 按列的 `scale`/`offset`
/// 换成物理量 f32 再发，线协议不变；带 `FLAG_PEAK_PAIRS` 的帧原样以对传下去
/// （`TAP_FLAG_PEAK_PAIRS`），不许只取一半。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct TapConfig {
    /// window 模式的窗长（拍数，每通道）。`0` 且 `mode` 未声明 = frame 模式。
    pub window_samples: u32,
    /// 每秒最多出几帧——按墙钟，不按样本数（tap 不知道采样率）。
    pub refresh_hz: f32,
    /// Built-in statistics to compute: `rms`, `peak`, `min`, `max`, `mean`.
    ///
    /// 它是把发出去的样本**所有列交织在一起**算的，多列口上没有意义。留着给
    /// CLI / 测试。
    #[serde(default)]
    pub stats: Vec<String>,
    /// Explicit mode override. `None` keeps the `window_samples` heuristic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub mode: Option<TapMode>,
    /// 只抽这些列进环、只发这些列。空 = 整口全部列（老配置的含义不变）。
    ///
    /// 按列 id 存，不按下标——口加一列，下标就悄悄指错（同别名那课）。壳体建
    /// tap 时对着口的列契约解析成通道号，解析不到就拒绝并列出可选列；帧上的
    /// `channels` 数 = 这里选中的列数，顺序 = 这里的顺序（有 `repeat_by` 的口
    /// 不带组序的引用按组序展开）。它住在 tap 里而不只住在图例里，是带宽：
    /// window 模式每次刷新都拷整窗，25 列只看 5 列时线上的字节差 5 倍。
    #[serde(default)]
    pub channels: Vec<ColumnRef>,
}

impl TapConfig {
    /// 生效的模式：显式声明优先，否则 `window_samples == 0` 是 frame、> 0 是
    /// window。壳体、CLI、前端都从这一处问，不各推各的。
    pub fn effective_mode(&self) -> TapMode {
        self.mode.unwrap_or(if self.window_samples == 0 {
            TapMode::Frame
        } else {
            TapMode::Window
        })
    }

    /// 几何相同 = 环布局不用重建，改动可以就地生效（攒的窗留着）。
    pub fn same_geometry(&self, other: &TapConfig) -> bool {
        self.effective_mode() == other.effective_mode()
            && self.window_samples == other.window_samples
            && self.channels == other.channels
    }

    /// 声明层面的自洽性——不需要口描述子就能查的那部分。列 id 能不能在口上
    /// 找到，要到壳体拿着描述子才知道。
    pub fn validate(&self) -> Result<(), String> {
        if !(self.refresh_hz.is_finite() && self.refresh_hz > 0.0) {
            return Err(format!("refresh_hz must be > 0 (got {})", self.refresh_hz));
        }
        if self.effective_mode() == TapMode::Window && self.window_samples == 0 {
            return Err("window mode needs window_samples > 0".to_string());
        }
        for c in &self.channels {
            c.validate().map_err(|e| format!("channels: {e}"))?;
        }
        Ok(())
    }
}

/// Tap operating mode (see [`TapConfig::mode`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum TapMode {
    Window,
    Frame,
    Stream,
}

impl TapMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TapMode::Window => "window",
            TapMode::Frame => "frame",
            TapMode::Stream => "stream",
        }
    }
}

impl std::str::FromStr for TapMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "window" => Ok(TapMode::Window),
            "frame" => Ok(TapMode::Frame),
            "stream" => Ok(TapMode::Stream),
            other => Err(format!(
                "unknown tap mode {other:?} (want window|frame|stream)"
            )),
        }
    }
}

/// 指一列。
///
/// - 按列契约的 id：`{ column = "iq" }`；有 `repeat_by` 的口可带组序（0 起，
///   同 `column_groups.label` 的 `{i}`）：`{ column = "iq", group = 2 }`。
///   选列时不带组序 = 每组都要——组数只有帧知道（口通道数 ÷ 组内列数），壳体
///   到 feed 时按帧宽展开。做触发源时，口只要**声明了** `column_groups` 就必须
///   带组序（源只能是一个通道；按声明判，不看运行时几组）。
/// - 按通道下标：`{ channel = 3 }`——给没有列契约的口；有契约的口也认，但那
///   就回到了"口加一列就错位"的老路，声明里别这么写。
///
/// **相等是"写法相等"，不是"指同一列"**：单组口上 `{ column = "v" }`（每组）与
/// `{ column = "v", group = 0 }`（第 0 组）指着同一路，`PartialEq` 却判不等——拿它
/// 当 map 键、做集合比较就会两边对不上（CLI 写一种、控件写另一种，谁也看不见谁）。
/// 做身份之前**先落到具体通道**（解析成交织下标，或 (列 id, 组) 一对）再比。
///
/// 文本形式（CLI）：`iq`、`2/iq`、`@3`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum ColumnRef {
    Column {
        column: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        group: Option<u32>,
    },
    Channel {
        channel: u32,
    },
}

impl ColumnRef {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            ColumnRef::Column { column, .. } if column.trim().is_empty() => {
                Err("empty column id".to_string())
            }
            _ => Ok(()),
        }
    }

    /// `self` 是不是在选列集 `sel` 里。空集 = 整口，什么都在。不带组序的选列
    /// 引用覆盖该列的所有组；**反过来不成立**——`sel` 指定了组，而被查的引用不带
    /// 组（= 每组）不是它的子集，判不在。多组口上这是对的（"每组"确实越出了那一
    /// 组），单组口上是保守的拒绝：把两边的写法对齐即可。
    pub fn selected_in(&self, sel: &[ColumnRef]) -> bool {
        if sel.is_empty() {
            return true;
        }
        sel.iter().any(|s| match (s, self) {
            (
                ColumnRef::Column {
                    column: a,
                    group: ga,
                },
                ColumnRef::Column {
                    column: b,
                    group: gb,
                },
            ) => a == b && (ga.is_none() || ga == gb),
            (ColumnRef::Channel { channel: a }, ColumnRef::Channel { channel: b }) => a == b,
            _ => false,
        })
    }
}

impl std::str::FromStr for ColumnRef {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty column ref".to_string());
        }
        if let Some(n) = s.strip_prefix('@') {
            return n
                .parse::<u32>()
                .map(|channel| ColumnRef::Channel { channel })
                .map_err(|_| format!("bad channel index in {s:?} (want @<n>)"));
        }
        if let Some((g, id)) = s.split_once('/') {
            let group = g
                .parse::<u32>()
                .map_err(|_| format!("bad group in {s:?} (want <group>/<column>)"))?;
            if id.is_empty() {
                return Err(format!("missing column id in {s:?}"));
            }
            return Ok(ColumnRef::Column {
                column: id.to_string(),
                group: Some(group),
            });
        }
        Ok(ColumnRef::Column {
            column: s.to_string(),
            group: None,
        })
    }
}

impl std::fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ColumnRef::Column {
                column,
                group: Some(g),
            } => write!(f, "{g}/{column}"),
            ColumnRef::Column {
                column,
                group: None,
            } => f.write_str(column),
            ColumnRef::Channel { channel } => write!(f, "@{channel}"),
        }
    }
}

// ---------------------------------------------------------------------------
// 示波器 setup（persisted in the binding as `bind.scope`）
// ---------------------------------------------------------------------------

/// 示波器的时钟。
///
/// - `native`（缺省，现状）：环跟着**唯一**的源走——存源的原生 dtype、存储率 =
///   `fs_native / D`（D 整数、超预算走入口峰值检测、每 scan 存一对 min/max）。
///   保留它是因为 100 MS/s 下峰值检测能保住 1 拍宽的毛刺，**任何重采样都会把毛刺
///   滤掉**：抓毛刺就得走这条。只能有一个源。
/// - `fixed`：示波器有自己的采样率，任何率、任何 dtype 的源都重采样到这根时钟上，
///   于是多口、多节点（同主机）的通道能进同一个环、同一次触发、同一屏。环每 scan
///   每列存**一个值**（不存对），列 `kind` 是 enum / bitfield / counter 的一律 ZOH
///   不插值（给状态量插出中间值是画假）。
///
/// `fs_hz` 省略 = auto：取各源实测率的最大值，再按壳体上限封顶。**上限归壳体**
/// （环境变量 `SIGFLOW_SCOPE_MAX_FS`，缺省 8e6），这里只查 > 0——同 `depth_bytes`
/// 的分工。`mode = native` 时不许给 `fs_hz`（给了说明写的人以为它有用）。
///
/// 超预算时降的是**示波器时钟**，不是源率（见 [`ScopeConfig::budget_bytes_per_s`]）。
///
/// 一期 `fixed` 环恒 f32。想用 i16 换深度得先定量化标度的出处（列 `bound`？每通道
/// full-scale？），那是个决定不是个字段，二期再加 `clock.dtype`。
///
/// **能照实读幅度的带宽只到 0.8 × fs/2**（≈ `fs/2.5`）：重采样的抗混叠滤波器在带
/// 顶要滚降（实测 0.8 × 奈奎斯特 93.8%、0.9 × 奈奎斯特 73.1%，见 docs/scope-contract.md
/// §8.4）。要量某个频率的峰峰值，时钟至少取它的 2.5 倍。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ScopeClock {
    #[serde(default)]
    pub mode: ClockMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub fs_hz: Option<f64>,
}

/// 时钟模式，见 [`ScopeClock`]。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum ClockMode {
    #[default]
    Native,
    Fixed,
}

impl ScopeClock {
    pub fn is_fixed(&self) -> bool {
        self.mode == ClockMode::Fixed
    }

    pub fn validate(&self) -> Result<(), String> {
        match (self.mode, self.fs_hz) {
            (ClockMode::Native, Some(_)) => Err(
                "clock: mode = native 跟着源走，不接受 fs_hz（要自己的采样率写 mode = \"fixed\"）"
                    .to_string(),
            ),
            (_, Some(f)) if !(f.is_finite() && f > 0.0) => {
                Err(format!("clock.fs_hz must be > 0 (got {f})"))
            }
            _ => Ok(()),
        }
    }
}

/// 一台示波器的一个源：某个节点的某个口的某几列。
///
/// `node` 省略 = 挂控件的那个节点（环的 owner）。**同主机**才行——跨主机要等
/// `sigflow_types::time` 的 wall/mono 锚点对，壳体会直接拒。`channels` 空 = 整口。
/// 多源只在 `clock.mode = fixed` 下成立（没有共同网格就没有共同的一拍）。
///
/// **名字，不是下标**：通道引用（[`ChanRef::source`]）按名字指源。缺省名 = `port`
/// （给了 `node` 就是 `node/port`），重名就得给其中一个写 `name`。之所以不用
/// `sources` 的下标：下标是位置，中间插一个源、删一个源，所有引用会**悄悄**指到
/// 另一路去；名字错了 validate 当场报，这跟"口加一列就错位"是同一笔账
/// （见 [`ColumnRef`]）。名字里不能有 `:`——文本形式 `源:列` 靠它分段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ScopeSource {
    /// 引用这个源用的名字。省略 = `port` 或 `node/port`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name: Option<String>,
    /// 节点 id，省略 = 挂控件的那个节点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub node: Option<String>,
    /// 口 id（在 `node` 的命名空间里）。
    pub port: String,
    /// 进环的列。空 = 整口。
    #[serde(default)]
    pub channels: Vec<ColumnRef>,
}

impl ScopeSource {
    /// 引用这个源用的名字：`name`，没写就是 `port` / `node/port`。
    pub fn effective_name(&self) -> String {
        match (&self.name, &self.node) {
            (Some(n), _) => n.clone(),
            (None, Some(nd)) => format!("{nd}/{}", self.port),
            (None, None) => self.port.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.port.trim().is_empty() {
            return Err("port is required".to_string());
        }
        if let Some(n) = &self.node {
            if n.trim().is_empty() {
                return Err("node must not be empty (省略 = 本节点)".to_string());
            }
        }
        if let Some(n) = &self.name {
            if n.trim().is_empty() {
                return Err("name must not be empty".to_string());
            }
        }
        let name = self.effective_name();
        if name.contains(':') {
            return Err(format!(
                "源名 {name:?} 里有 ':'（文本形式 `源:列` 靠它分段）——写一个不带 ':' 的 name"
            ));
        }
        for c in &self.channels {
            c.validate().map_err(|e| format!("channels: {e}"))?;
        }
        Ok(())
    }
}

/// 带源的通道引用：多源示波器上，光有列 id 指不出唯一一路。
///
/// `source` 省略 = 唯一的那个源（单源简写、或只有一个 `[[sources]]`）；有两个以上
/// 源时省略是**错**，不是"取第 0 个"——猜错了就是量错了一路。
///
/// 序列化是把 [`ColumnRef`] 摊平进来，所以老的写法一字不改照旧能读：
/// `source = { column = "z" }`、`{ source = "rotor", column = "z", group = 0 }`。
/// 文本形式（CLI）：`rotor:0/theta`、`iu`（不指源）。
///
/// **相等仍然是"写法相等"**，而且现在有两层：`{column="v"}` 与
/// `{source="rotor", column="v"}` 在单源上指同一路却判不等。当键、做集合比较之前
/// 先用 [`ScopeConfig::resolve`] 落到 (源槽位, 列) 再比。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ChanRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub source: Option<String>,
    #[serde(flatten)]
    pub column: ColumnRef,
}

impl ChanRef {
    pub fn new(source: Option<String>, column: ColumnRef) -> Self {
        ChanRef { source, column }
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(s) = &self.source {
            if s.trim().is_empty() {
                return Err("empty source name".to_string());
            }
        }
        self.column.validate()
    }
}

impl From<ColumnRef> for ChanRef {
    fn from(column: ColumnRef) -> Self {
        ChanRef {
            source: None,
            column,
        }
    }
}

impl std::str::FromStr for ChanRef {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        match s.split_once(':') {
            Some((src, col)) => {
                let src = src.trim();
                if src.is_empty() {
                    return Err(format!(
                        "empty source name in {s:?} (want <source>:<column>)"
                    ));
                }
                Ok(ChanRef {
                    source: Some(src.to_string()),
                    column: col.parse()?,
                })
            }
            None => Ok(ChanRef {
                source: None,
                column: s.parse()?,
            }),
        }
    }
}

impl std::fmt::Display for ChanRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            Some(s) => write!(f, "{s}:{}", self.column),
            None => write!(f, "{}", self.column),
        }
    }
}

/// 示波器触发。住在采集引擎里——它决定"哪一窗给你"，不是画法。
///
/// 引擎在原生 dtype 上逐拍跑（电平换成码），语义：
/// - `clock = native` 下源列在写环时读整行，所以**不要求在 `channels` 里**
///   （外触发）；`clock = fixed` 下环里只有选中的列，没进环的列不在共同网格上，
///   所以触发源**必须在该源的选列里**，validate 会拦。
/// - 电平与原始物理量比；显示侧的 AC 耦合只是把画法平移，不改比较。
/// - NaN 拍透明：不触发、不更新比较状态。
/// - 比较态：rising 时 `v ≤ level − h/2` 进 Low、`v ≥ level + h/2` 进 High，
///   带内保持；h = 0 时就是 `v < level` / `v ≥ level`。跨越 = 比较态按沿翻转。
/// - armed = 连续段已攒够 pre 拍 ∧ 没有等待中的捕获 ∧ 距上次触发拍 ≥
///   `holdoff_samples` ∧ running。触发拍本身算 post 的第一拍；亚采样相位
///   `frac = (level − v[t−1]) / (v[t] − v[t−1])` 随记录存，叠余辉时按它平移。
/// - 断（帧头声明的 / 跳号 / 布局变）清连续段、取消等待中的捕获。
/// - 触发点在屏幕的位置是时基的事（[`Timebase::position`]），这里没有。
/// - 峰值检测存储时 rising 在 max 平面上跑、falling 在 min 平面上跑。
///
/// `kind`：一期只有 edge；pulse（正/负脉宽 >、<、区间）和 window（进/出带）
/// 二期加可选字段，声明层现在就认这三个名字。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ScopeTrigger {
    #[serde(default)]
    pub kind: TrigKind,
    pub source: ChanRef,
    /// 物理量单位（整数口按列的 scale/offset 换算后的量）。
    pub level: f64,
    #[serde(default)]
    pub slope: TrigSlope,
    /// 迟滞带宽（物理量，≥ 0）。缺省 0——单位不知道就选不出一个非零缺省；
    /// UI 给"抗噪 = 源列可见 pk-pk 的 5%"一键写入。
    #[serde(default)]
    pub hysteresis: f64,
    /// 释抑：触发后至少再过这么多拍才能再触发。`clock = native` 下按**源口的拍数**
    /// （抽取前），引擎按抽取比换成存储拍；`clock = fixed` 下按**示波器时钟的拍**
    /// （没有 D，不换算）。与 `span_scans` 同一度量；UI 有率时并排显示时间。
    #[serde(default)]
    pub holdoff_samples: u32,
    #[serde(default)]
    pub mode: TrigMode,
}

impl ScopeTrigger {
    pub fn validate(&self) -> Result<(), String> {
        self.source.validate().map_err(|e| format!("source: {e}"))?;
        if !self.level.is_finite() {
            return Err(format!("level must be finite (got {})", self.level));
        }
        if !(self.hysteresis.is_finite() && self.hysteresis >= 0.0) {
            return Err(format!("hysteresis must be >= 0 (got {})", self.hysteresis));
        }
        Ok(())
    }
}

/// 触发种类。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum TrigKind {
    #[default]
    Edge,
    Pulse,
    Window,
}

/// 触发沿。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum TrigSlope {
    #[default]
    Rising,
    Falling,
    Either,
}

impl TrigSlope {
    pub fn as_str(self) -> &'static str {
        match self {
            TrigSlope::Rising => "rising",
            TrigSlope::Falling => "falling",
            TrigSlope::Either => "either",
        }
    }
}

impl std::str::FromStr for TrigSlope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "rising" => Ok(TrigSlope::Rising),
            "falling" => Ok(TrigSlope::Falling),
            "either" => Ok(TrigSlope::Either),
            other => Err(format!(
                "unknown trigger slope {other:?} (want rising|falling|either)"
            )),
        }
    }
}

/// 触发模式：auto 没触发也按刷新率给自由跑的窗（找电平时屏幕不冻住）；
/// normal 只在触发时出窗，其间显示保持上一窗；single 触发一次就停。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum TrigMode {
    #[default]
    Auto,
    Normal,
    Single,
}

impl TrigMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TrigMode::Auto => "auto",
            TrigMode::Normal => "normal",
            TrigMode::Single => "single",
        }
    }
}

impl std::str::FromStr for TrigMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "auto" => Ok(TrigMode::Auto),
            "normal" => Ok(TrigMode::Normal),
            "single" => Ok(TrigMode::Single),
            other => Err(format!(
                "unknown trigger mode {other:?} (want auto|normal|single)"
            )),
        }
    }
}

/// 时基：一屏多宽、触发点在屏幕哪儿。
///
/// 跨度二选一：`span_s`（秒，有率的口）或 `span_scans`（拍数，没有率的口也能用）。
/// 拍的度量看时钟：`clock = native` 下是**源口的拍**（抽取前）——入口峰值检测
/// 抽取 D:1 之后引擎自己除以 D 换成存储拍，操作者不用知道 D；`clock = fixed` 下
/// 是**示波器时钟的拍**（重采样后每 scan 一个值，没有 D）。`holdoff_samples`、
/// [`CursorPair`] 同一度量。
/// `position` = 触发点（t = 0）在屏幕的位置，[0, 1]，缺省 0.5——示波器上这是
/// 水平位置旋钮，不是触发的属性；pre = round(position × span_scans)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct Timebase {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub span_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "number"))]
    pub span_scans: Option<u64>,
    #[serde(default = "default_half")]
    pub position: f32,
}

fn default_half() -> f32 {
    0.5
}

impl Timebase {
    pub fn validate(&self) -> Result<(), String> {
        match (self.span_s, self.span_scans) {
            (Some(s), None) if s.is_finite() && s > 0.0 => {}
            (None, Some(n)) if n > 0 => {}
            (Some(_), Some(_)) => {
                return Err("timebase: give span_s or span_scans, not both".to_string())
            }
            (None, None) => return Err("timebase: span_s or span_scans required".to_string()),
            _ => return Err("timebase: span must be > 0".to_string()),
        }
        if !(self.position.is_finite() && (0.0..=1.0).contains(&self.position)) {
            return Err(format!(
                "timebase: position must be within [0, 1] (got {})",
                self.position
            ));
        }
        Ok(())
    }
}

/// 采集模式：normal 每条记录一窗；average 同一触发相位的 N 窗平均（要触发）；
/// persist 每条记录都进余辉密度图、刷新时只把最新一条当矢量线。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum AcqMode {
    #[default]
    Normal,
    Average,
    Persist,
}

/// 采集设置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct AcqSetting {
    #[serde(default)]
    pub mode: AcqMode,
    /// average 模式平均几窗，≥ 2。
    #[serde(default = "default_average_n")]
    pub average_n: u32,
    /// 余辉衰减：每次刷新 count × decay，[0, 1]；**1 = 不衰减**（无限余辉）。
    #[serde(default = "default_persist_decay")]
    pub persist_decay: f32,
}

fn default_average_n() -> u32 {
    16
}

fn default_persist_decay() -> f32 {
    0.9
}

impl Default for AcqSetting {
    fn default() -> Self {
        AcqSetting {
            mode: AcqMode::Normal,
            average_n: default_average_n(),
            persist_decay: default_persist_decay(),
        }
    }
}

impl AcqSetting {
    pub fn validate(&self) -> Result<(), String> {
        if self.average_n < 2 {
            return Err(format!(
                "acq.average_n must be >= 2 (got {})",
                self.average_n
            ));
        }
        if !(self.persist_decay.is_finite() && (0.0..=1.0).contains(&self.persist_decay)) {
            return Err(format!(
                "acq.persist_decay must be within [0, 1] (got {})",
                self.persist_decay
            ));
        }
        Ok(())
    }
}

/// 耦合：dc 原样；ac = 减去当前记录窗（触发窗 / 屏幕跨度）内的均值，密度累积
/// 与测量都按耦合后的值。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum Coupling {
    #[default]
    Dc,
    Ac,
}

/// 显示插值（显示域的事，前端做）：none（点/台阶）、linear、sinc（加窗 sinc，
/// 前提是带限于 fs/2.5——PWM 门极、计数器上会画出不存在的过冲）。列 `kind`
/// 是 enum / bitfield / counter 的通道一律保持台阶，不看这一项。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum Interp {
    None,
    #[default]
    Linear,
    Sinc,
}

/// 逐通道的垂直设置，键是列引用。没写的通道取缺省：v_div 由引擎首帧自适应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct VerticalSetting {
    pub channel: ChanRef,
    /// 每格多少（物理量），> 0。
    pub v_div: f64,
    /// 屏幕中线对应的值（物理量）。
    #[serde(default)]
    pub offset: f64,
    #[serde(default = "default_true")]
    pub on: bool,
    #[serde(default)]
    pub coupling: Coupling,
    #[serde(default)]
    pub interp: Interp,
    /// 带宽限制（Hz），0 = 关。
    #[serde(default)]
    pub bw_limit_hz: f64,
}

fn default_true() -> bool {
    true
}

impl VerticalSetting {
    pub fn validate(&self) -> Result<(), String> {
        self.channel
            .validate()
            .map_err(|e| format!("channel: {e}"))?;
        if !(self.v_div.is_finite() && self.v_div > 0.0) {
            return Err(format!("v_div must be > 0 (got {})", self.v_div));
        }
        if !self.offset.is_finite() {
            return Err("offset must be finite".to_string());
        }
        if !(self.bw_limit_hz.is_finite() && self.bw_limit_hz >= 0.0) {
            return Err(format!(
                "bw_limit_hz must be >= 0 (got {})",
                self.bw_limit_hz
            ));
        }
        Ok(())
    }
}

/// 测量门：screen = 屏幕可见区；cursors = 时间光标之间。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum Gate {
    #[default]
    Screen,
    Cursors,
}

/// 一对时间光标：相对时间零点（触发窗以触发拍为 0、自由跑 / roll 以右缘为 0）
/// 的**存储拍**数，可为负（`clock = fixed` 下存储拍就是示波器时钟的拍）。它是 setup 的一部分（真机的光标随 setup 存），UI
/// 拖光标即时 `scope_set`、手势结束回写绑定。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct CursorPair {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub a: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub b: i64,
}

/// 测量设置：量哪些通道、门在哪儿。测量在引擎里按记录算，随 meas 帧发；
/// 不适用 = NaN 不是 0。`gate = cursors` 时 `cursors` 必须给（门 = 两光标之
/// 间），引擎不会拿屏幕代替——那是把不正常换成正常。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct MeasureSetting {
    #[serde(default)]
    pub channels: Vec<ChanRef>,
    #[serde(default)]
    pub gate: Gate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub cursors: Option<CursorPair>,
}

impl MeasureSetting {
    pub fn validate(&self) -> Result<(), String> {
        if self.gate == Gate::Cursors {
            match self.cursors {
                None => return Err("measure: gate = cursors needs measure.cursors".to_string()),
                Some(c) if c.a == c.b => {
                    return Err("measure.cursors: a and b must differ".to_string())
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// 一台示波器的 setup——持久化在绑定的 `bind.scope` 里（node.toml），CLI
/// `widget bind <alias> port:<id> --scope-file setup.toml` 写的就是它；UI 上拨
/// 的旋钮即时 `scope_set`、手势结束回写绑定（同一个旋钮，不另存一份）。
///
/// 示波器 = 生产口所在壳体里的一台采集引擎：帧单次拷贝进 mmap 文件环（原生
/// dtype、按列分平面、16× min/max 金字塔），独立线程做触发 / 余辉 / 视图，浏览
/// 器只拉视图与密度图。设计在 sigflow-core `docs/scope.md`。
///
/// 几何（改了要重建环）= `clock` + `channels` / `sources` + `depth_bytes` +
/// `budget_bytes_per_s`；其余就地生效。
///
/// ```toml
/// [widgets.bind.scope]
/// channels = [{ column = "iu" }, { column = "iv" }, { column = "iw" }, { column = "z" }]
/// depth_bytes = 1073741824
/// budget_bytes_per_s = 400000000
/// refresh_hz = 30
/// timebase = { span_s = 0.002, position = 0.5 }
/// roll_threshold_s = 0.5
/// [widgets.bind.scope.trigger]
/// kind = "edge"
/// source = { column = "z" }
/// level = 0.5
/// slope = "rising"
/// mode = "auto"
/// [widgets.bind.scope.acq]
/// mode = "persist"
/// persist_decay = 0.9
/// [[widgets.bind.scope.vertical]]
/// channel = { column = "iu" }
/// v_div = 50
/// coupling = "dc"
/// [widgets.bind.scope.measure]
/// channels = [{ column = "iu" }]
/// gate = "screen"
/// ```
///
/// 多源（示波器自己的时钟，各源重采样到它上面；通道引用按**源名**指路）：
///
/// ```toml
/// [widgets.bind.scope]
/// clock = { mode = "fixed", fs_hz = 1000000 }   # 省 fs_hz = auto（取各源实测率最大值，壳体封顶）
/// depth_bytes = 1073741824
/// timebase = { span_s = 0.01, position = 0.5 }
/// [[widgets.bind.scope.sources]]
/// port = "loop"                                  # 名字缺省 = "loop"
/// channels = [{ column = "iu" }, { column = "iv" }]
/// [[widgets.bind.scope.sources]]
/// node = "foc"
/// port = "rotor"                                 # 名字缺省 = "foc/rotor"
/// channels = [{ column = "theta", group = 0 }]
/// [widgets.bind.scope.trigger]
/// source = { source = "foc/rotor", column = "theta", group = 0 }
/// level = 0.5
/// [[widgets.bind.scope.vertical]]
/// channel = { source = "loop", column = "iu" }
/// v_div = 50
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ScopeConfig {
    /// 时钟：跟着源走（native，缺省）还是示波器自己的采样率（fixed）。多源、
    /// 混合率、混合 dtype 都只在 fixed 下成立。改了要重建环。
    #[serde(default)]
    pub clock: ScopeClock,
    /// 进环的列，**单源简写**：源 = 控件绑定的那个口。空 = 整口。字节预算按选中
    /// 列的 dtype 字节算。与 `sources` 二选一。
    #[serde(default)]
    pub channels: Vec<ColumnRef>,
    /// 多源：一台示波器收几个口（可跨节点，同主机）的通道。非空时以它为准，
    /// `channels` 必须空。两个以上源要 `clock.mode = fixed`。
    #[serde(default)]
    pub sources: Vec<ScopeSource>,
    /// 存储深度（字节）。上限归壳体（环境变量），这里只要 > 0。
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub depth_bytes: u64,
    /// 写环字节预算（B/s）。`fs_allowed = budget / Σ 选中列字节`。`clock = native`
    /// 下超了先向生产方请求降率（口声明了 `negotiable.rate_param`），不支持就入口
    /// 峰值检测抽取——峰值存储每 scan 存一对，抽取比按存储字节算；`clock = fixed`
    /// 下降的是**示波器时钟**，源率不动。
    #[serde(default = "default_budget")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub budget_bytes_per_s: u64,
    /// 视图 / 密度图 / 测量的推送率。
    #[serde(default = "default_refresh_hz")]
    pub refresh_hz: f32,
    pub timebase: Timebase,
    /// 一屏长过它自动进 roll（跟着写指针走，不触发）。
    #[serde(default = "default_roll_threshold")]
    pub roll_threshold_s: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub trigger: Option<ScopeTrigger>,
    #[serde(default)]
    pub acq: AcqSetting,
    #[serde(default)]
    pub vertical: Vec<VerticalSetting>,
    #[serde(default)]
    pub measure: MeasureSetting,
}

fn default_budget() -> u64 {
    400_000_000
}

fn default_refresh_hz() -> f32 {
    30.0
}

fn default_roll_threshold() -> f64 {
    0.5
}

impl ScopeConfig {
    /// 源的槽位数。单源简写（`sources` 空）也算一个源。
    pub fn source_count(&self) -> usize {
        self.sources.len().max(1)
    }

    /// 各源的引用名，槽位序。单源简写没有名字（空表）。
    pub fn source_names(&self) -> Vec<String> {
        self.sources.iter().map(|s| s.effective_name()).collect()
    }

    /// 第 `slot` 个源的选列（空 = 整口）。单源简写 → `channels`。
    pub fn channels_of(&self, slot: usize) -> &[ColumnRef] {
        match self.sources.get(slot) {
            Some(s) => &s.channels,
            None if slot == 0 && self.sources.is_empty() => &self.channels,
            None => &[],
        }
    }

    /// 把通道引用落到 (源槽位, 列引用)。**当键、做集合比较之前先过这一步**——
    /// [`ChanRef`] / [`ColumnRef`] 的相等是写法相等，两边写法不同就互相看不见。
    ///
    /// 没指名源：只有一个源时是它，两个以上是错——不猜第 0 个，猜错了是量错一路。
    pub fn resolve<'a>(&self, r: &'a ChanRef) -> Result<(usize, &'a ColumnRef), String> {
        match &r.source {
            None if self.source_count() == 1 => Ok((0, &r.column)),
            None => Err(format!(
                "这台示波器有 {} 个源，通道引用要写 source（{}）",
                self.source_count(),
                self.source_names().join(" / ")
            )),
            Some(name) if self.sources.is_empty() => Err(format!(
                "指名了源 {name:?}，但这台示波器是单源简写（channels = 绑定的那个口）——\
                 要多源就把 channels 换成 [[sources]]"
            )),
            Some(name) => self
                .sources
                .iter()
                .position(|s| &s.effective_name() == name)
                .map(|i| (i, &r.column))
                .ok_or_else(|| {
                    format!(
                        "找不到源 {name:?}（有：{}）",
                        self.source_names().join(" / ")
                    )
                }),
        }
    }

    /// 几何相同 = 环不用重建。
    pub fn same_geometry(&self, other: &ScopeConfig) -> bool {
        self.clock == other.clock
            && self.channels == other.channels
            && self.sources == other.sources
            && self.depth_bytes == other.depth_bytes
            && self.budget_bytes_per_s == other.budget_bytes_per_s
    }

    /// 声明层面的自洽性。列 id 存不存在、节点/口在不在图上、深度与
    /// `clock.fs_hz` 超不超壳体上限，要到壳体才知道。
    pub fn validate(&self) -> Result<(), String> {
        if self.depth_bytes == 0 {
            return Err("depth_bytes must be > 0".to_string());
        }
        if self.budget_bytes_per_s == 0 {
            return Err("budget_bytes_per_s must be > 0".to_string());
        }
        if !(self.refresh_hz.is_finite() && self.refresh_hz > 0.0) {
            return Err(format!("refresh_hz must be > 0 (got {})", self.refresh_hz));
        }
        if !(self.roll_threshold_s.is_finite() && self.roll_threshold_s >= 0.0) {
            return Err(format!(
                "roll_threshold_s must be >= 0 (got {})",
                self.roll_threshold_s
            ));
        }
        self.clock.validate()?;
        for c in &self.channels {
            c.validate().map_err(|e| format!("channels: {e}"))?;
        }
        if !self.sources.is_empty() && !self.channels.is_empty() {
            return Err(
                "channels（单源简写）与 sources 二选一：写了 sources 就把 channels 清空"
                    .to_string(),
            );
        }
        for (i, src) in self.sources.iter().enumerate() {
            src.validate().map_err(|e| format!("sources[{i}]: {e}"))?;
        }
        let names = self.source_names();
        for (i, n) in names.iter().enumerate() {
            if let Some(j) = names[..i].iter().position(|m| m == n) {
                return Err(format!(
                    "sources[{i}] 与 sources[{j}] 解析出同一个名字 {n:?}——给其中一个写 name"
                ));
            }
        }
        if self.sources.len() > 1 && !self.clock.is_fixed() {
            return Err(format!(
                "{} 个源要 clock.mode = \"fixed\"：native 是跟着唯一的源走（原生 dtype + 整数抽取），\
                 多源没有共同网格",
                self.sources.len()
            ));
        }
        self.timebase.validate()?;
        if let Some(t) = &self.trigger {
            t.validate().map_err(|e| format!("trigger: {e}"))?;
            let (slot, col) = self
                .resolve(&t.source)
                .map_err(|e| format!("trigger.source: {e}"))?;
            // native：源列在写环时读整行，外触发不必在选列里。
            // fixed：环里只有选中的列，没进环就不在共同网格上，触发跑不了。
            if self.clock.is_fixed() && !col.selected_in(self.channels_of(slot)) {
                return Err(format!(
                    "trigger.source: {} 不在选列里——clock = fixed 下触发源必须进环",
                    t.source
                ));
            }
        }
        self.acq.validate()?;
        if self.acq.mode == AcqMode::Average && self.trigger.is_none() {
            return Err("acq.mode = average needs a trigger".to_string());
        }
        for (i, v) in self.vertical.iter().enumerate() {
            v.validate().map_err(|e| format!("vertical[{i}]: {e}"))?;
            let (slot, col) = self
                .resolve(&v.channel)
                .map_err(|e| format!("vertical[{i}].channel: {e}"))?;
            if !col.selected_in(self.channels_of(slot)) {
                return Err(format!(
                    "vertical[{i}]: channel {} is not in channels",
                    v.channel
                ));
            }
        }
        self.measure.validate()?;
        for (i, c) in self.measure.channels.iter().enumerate() {
            c.validate()
                .map_err(|e| format!("measure.channels[{i}]: {e}"))?;
            let (slot, col) = self
                .resolve(c)
                .map_err(|e| format!("measure.channels[{i}]: {e}"))?;
            if !col.selected_in(self.channels_of(slot)) {
                return Err(format!("measure.channels[{i}]: {c} is not in channels"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tap_config_tests {
    use super::*;

    fn window(n: u32) -> TapConfig {
        TapConfig {
            window_samples: n,
            refresh_hz: 30.0,
            stats: vec![],
            mode: None,
            channels: vec![],
        }
    }

    #[test]
    fn 老配置不带新字段照样加载_新字段取缺省() {
        let c: TapConfig =
            serde_json::from_str(r#"{"window_samples":1024,"refresh_hz":30,"stats":[]}"#).unwrap();
        assert!(c.channels.is_empty(), "空 = 整口全部列");
        assert_eq!(c.effective_mode(), TapMode::Window);
        assert!(c.validate().is_ok());

        let f: TapConfig = serde_json::from_str(r#"{"window_samples":0,"refresh_hz":5}"#).unwrap();
        assert_eq!(f.effective_mode(), TapMode::Frame, "--window 0 就是 frame");
    }

    #[test]
    fn 带触发的旧_tap_段照样读_触发被丢() {
        // tap 退回纯订阅：触发搬去了示波器。旧绑定里的 trigger 字段不认、不报错
        // ——它只存在于没合过的分支上。
        let c: TapConfig = serde_json::from_str(
            r#"{"window_samples":400,"refresh_hz":30,"stats":[],"channels":[{"column":"iq"}],"trigger":{"source":{"column":"z"},"level":0.5}}"#,
        )
        .unwrap();
        assert_eq!(c.channels.len(), 1);
        let s = serde_json::to_string(&c).unwrap();
        assert!(!s.contains("trigger"), "{s}");
    }

    #[test]
    fn 序列化永远带_channels() {
        let s = serde_json::to_string(&window(1024)).unwrap();
        assert!(s.contains(r#""channels":[]"#), "{s}");
    }

    #[test]
    fn 列引用三种文本形式往返_json形状按声明() {
        let cases: [(&str, ColumnRef, &str); 3] = [
            (
                "iq",
                ColumnRef::Column {
                    column: "iq".into(),
                    group: None,
                },
                r#"{"column":"iq"}"#,
            ),
            (
                "2/iq",
                ColumnRef::Column {
                    column: "iq".into(),
                    group: Some(2),
                },
                r#"{"column":"iq","group":2}"#,
            ),
            ("@3", ColumnRef::Channel { channel: 3 }, r#"{"channel":3}"#),
        ];
        for (text, want, json) in cases {
            let parsed: ColumnRef = text.parse().unwrap();
            assert_eq!(parsed, want, "{text}");
            assert_eq!(parsed.to_string(), text, "Display 要能回到 CLI 写法");
            assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
            let back: ColumnRef = serde_json::from_str(json).unwrap();
            assert_eq!(back, want, "{json}");
        }
        assert!("".parse::<ColumnRef>().is_err());
        assert!("@x".parse::<ColumnRef>().is_err());
        assert!("a/iq".parse::<ColumnRef>().is_err(), "组序要是数字");
        assert!("2/".parse::<ColumnRef>().is_err());
    }

    #[test]
    fn 列引用的相等是写法相等_不是指同一列() {
        // 单组口上这两个指着同一路，PartialEq 却判不等——sigflow-core 的触发源下拉
        // 就栽在这上面：CLI 写 `0/v`、控件按形状省成 `v`，按写法取键两边对不上。
        // 拿 ColumnRef 做身份的代码要先落到具体通道再比。
        let bare: ColumnRef = "v".parse().unwrap();
        let g0: ColumnRef = "0/v".parse().unwrap();
        assert_ne!(bare, g0);
        assert!(
            g0.selected_in(&[bare.clone()]),
            "选列集里不带组序的覆盖所有组"
        );
        assert!(
            !bare.selected_in(&[g0.clone()]),
            "反过来不成立：指定了组，'每组'不是子集"
        );
    }

    #[test]
    fn 选列集的包含关系_空集是整口_不带组序覆盖所有组() {
        let sel: Vec<ColumnRef> = ["iq", "2/id", "@3"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let r = |s: &str| s.parse::<ColumnRef>().unwrap();
        assert!(r("iq").selected_in(&sel));
        assert!(r("1/iq").selected_in(&sel), "不带组序的 iq 覆盖每一组");
        assert!(r("2/id").selected_in(&sel));
        assert!(!r("1/id").selected_in(&sel), "带组序的只覆盖那一组");
        assert!(!r("id").selected_in(&sel), "选的是 2/id，裸 id 不算在里面");
        assert!(r("@3").selected_in(&sel));
        assert!(!r("@4").selected_in(&sel));
        assert!(r("anything").selected_in(&[]), "空集 = 整口");
    }

    #[test]
    fn 显式_window_但窗长_0_要拒() {
        let mut c = window(0);
        c.mode = Some(TapMode::Window);
        assert!(c.validate().is_err());
        let mut z = window(1024);
        z.refresh_hz = 0.0;
        assert!(z.validate().is_err());
    }

    #[test]
    fn 几何是模式加窗长加列_其余就地生效() {
        let a = window(1024);
        let mut b = a.clone();
        b.refresh_hz = 5.0;
        b.stats = vec!["rms".into()];
        assert!(a.same_geometry(&b), "刷新率/统计变了不重建");

        let mut c = a.clone();
        c.channels = vec!["iq".parse().unwrap()];
        assert!(!a.same_geometry(&c), "选列变了环布局就变了");

        let mut d = a.clone();
        d.window_samples = 2048;
        assert!(!a.same_geometry(&d));

        let mut e = a.clone();
        e.mode = Some(TapMode::Window);
        assert!(a.same_geometry(&e));
    }
}

#[cfg(test)]
mod scope_config_tests {
    use super::*;

    const SETUP: &str = r#"
channels = [{ column = "iu" }, { column = "iv" }, { column = "iw" }, { column = "z" }]
depth_bytes = 1073741824
budget_bytes_per_s = 400000000
refresh_hz = 30
timebase = { span_s = 0.002, position = 0.5 }
roll_threshold_s = 0.5

[trigger]
kind = "edge"
source = { column = "z" }
level = 0.5
slope = "rising"
hysteresis = 0
holdoff_samples = 0
mode = "auto"

[acq]
mode = "persist"
average_n = 16
persist_decay = 0.9

[[vertical]]
channel = { column = "iu" }
v_div = 50
offset = 0
on = true
coupling = "dc"
interp = "linear"
bw_limit_hz = 0

[measure]
channels = [{ column = "iu" }]
gate = "screen"
"#;

    fn setup() -> ScopeConfig {
        toml::from_str(SETUP).unwrap()
    }

    #[test]
    fn 文档里的_toml_形状能读_且过校验() {
        let c = setup();
        assert_eq!(c.channels.len(), 4);
        assert_eq!(c.timebase.span_s, Some(0.002));
        assert_eq!(c.timebase.position, 0.5);
        assert_eq!(c.trigger.as_ref().unwrap().kind, TrigKind::Edge);
        assert_eq!(c.acq.mode, AcqMode::Persist);
        assert_eq!(c.vertical[0].interp, Interp::Linear);
        assert_eq!(c.measure.gate, Gate::Screen);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn 缺省项可省_只写必填() {
        let c: ScopeConfig = toml::from_str(
            r#"
depth_bytes = 268435456
timebase = { span_scans = 4000 }
"#,
        )
        .unwrap();
        assert_eq!(c.budget_bytes_per_s, 400_000_000);
        assert_eq!(c.refresh_hz, 30.0);
        assert_eq!(c.roll_threshold_s, 0.5);
        assert_eq!(c.timebase.position, 0.5, "触发点缺省屏中");
        assert_eq!(c.acq, AcqSetting::default());
        assert_eq!(c.acq.average_n, 16);
        assert_eq!(c.acq.persist_decay, 0.9);
        assert!(c.trigger.is_none() && c.vertical.is_empty() && c.measure.channels.is_empty());
        assert!(c.validate().is_ok());
        let s = serde_json::to_string(&c).unwrap();
        assert!(
            s.contains(r#""channels":[]"#) && !s.contains("trigger"),
            "{s}"
        );
    }

    #[test]
    fn 跨度二选一_必填() {
        let mut c = setup();
        c.timebase.span_scans = Some(10);
        assert!(c.validate().unwrap_err().contains("not both"));
        c.timebase.span_s = None;
        c.timebase.span_scans = None;
        assert!(c.validate().unwrap_err().contains("required"));
        c.timebase.span_s = Some(0.0);
        assert!(c.validate().unwrap_err().contains("> 0"));
        c.timebase.span_s = Some(1.0);
        c.timebase.position = 1.5;
        assert!(c.validate().unwrap_err().contains("position"));
    }

    #[test]
    fn 垂直与测量的通道必须在选中列里() {
        let mut c = setup();
        c.vertical[0].channel = "iq".parse().unwrap();
        let e = c.validate().unwrap_err();
        assert!(
            e.contains("vertical[0]") && e.contains("not in channels"),
            "{e}"
        );

        let mut c = setup();
        c.measure.channels = vec!["nope".parse().unwrap()];
        assert!(c.validate().unwrap_err().contains("measure.channels[0]"));

        // 空选列 = 整口：什么通道都算在里面
        let mut c = setup();
        c.channels.clear();
        c.vertical[0].channel = "iq".parse().unwrap();
        assert!(c.validate().is_ok());

        // 触发源不必在选中列里（外触发）
        let mut c = setup();
        c.trigger.as_mut().unwrap().source = "iq_ref".parse().unwrap();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn 数值边界() {
        let mut c = setup();
        c.depth_bytes = 0;
        assert!(c.validate().unwrap_err().contains("depth_bytes"));
        let mut c = setup();
        c.budget_bytes_per_s = 0;
        assert!(c.validate().unwrap_err().contains("budget"));
        let mut c = setup();
        c.vertical[0].v_div = 0.0;
        assert!(c.validate().unwrap_err().contains("v_div"));
        let mut c = setup();
        c.vertical[0].bw_limit_hz = -1.0;
        assert!(c.validate().unwrap_err().contains("bw_limit_hz"));
        let mut c = setup();
        c.acq.persist_decay = 1.5;
        assert!(c.validate().unwrap_err().contains("persist_decay"));
        let mut c = setup();
        c.acq.persist_decay = 1.0;
        assert!(c.validate().is_ok(), "1 = 不衰减，合法");
        let mut c = setup();
        c.acq.average_n = 1;
        assert!(c.validate().unwrap_err().contains("average_n"));
        let mut c = setup();
        c.trigger.as_mut().unwrap().hysteresis = -0.1;
        assert!(c.validate().unwrap_err().contains("hysteresis"));
    }

    #[test]
    fn 光标门要有光标_且两条不重合() {
        let mut c = setup();
        c.measure.gate = Gate::Cursors;
        assert!(c.validate().unwrap_err().contains("measure.cursors"));
        c.measure.cursors = Some(CursorPair { a: -40, b: -40 });
        assert!(c.validate().unwrap_err().contains("differ"));
        c.measure.cursors = Some(CursorPair { a: -40, b: 12 });
        assert!(c.validate().is_ok());
        let s = serde_json::to_string(&c.measure).unwrap();
        assert!(s.contains(r#""cursors":{"a":-40,"b":12}"#), "{s}");
        // screen 门不带光标也行
        let m: MeasureSetting = serde_json::from_str(r#"{"channels":[],"gate":"screen"}"#).unwrap();
        assert!(m.cursors.is_none() && m.validate().is_ok());
    }

    #[test]
    fn average_要有触发() {
        let mut c = setup();
        c.acq.mode = AcqMode::Average;
        assert!(c.validate().is_ok());
        c.trigger = None;
        assert!(c.validate().unwrap_err().contains("average"));
    }

    #[test]
    fn 几何是选列加深度加预算() {
        let a = setup();
        let mut b = a.clone();
        b.refresh_hz = 10.0;
        b.trigger = None;
        b.acq.mode = AcqMode::Normal;
        b.timebase.span_s = Some(1.0);
        assert!(a.same_geometry(&b));
        let mut c = a.clone();
        c.depth_bytes *= 2;
        assert!(!a.same_geometry(&c));
        let mut d = a.clone();
        d.channels.pop();
        assert!(!a.same_geometry(&d));
        let mut e = a.clone();
        e.budget_bytes_per_s = 1;
        assert!(!a.same_geometry(&e));
    }

    const MULTI: &str = r#"
clock = { mode = "fixed", fs_hz = 1000000 }
depth_bytes = 1073741824
timebase = { span_s = 0.01 }
[[sources]]
port = "loop"
channels = [{ column = "iu" }, { column = "iv" }]
[[sources]]
node = "foc"
port = "rotor"
channels = [{ column = "theta", group = 0 }]
[trigger]
source = { source = "foc/rotor", column = "theta", group = 0 }
level = 0.5
[[vertical]]
channel = { source = "loop", column = "iu" }
v_div = 50
[measure]
channels = [{ source = "loop", column = "iv" }]
"#;

    fn multi() -> ScopeConfig {
        toml::from_str(MULTI).unwrap()
    }

    #[test]
    fn 多源的源名缺省是口_带节点是节点斜杠口() {
        let c = multi();
        assert_eq!(c.source_names(), vec!["loop", "foc/rotor"]);
        assert_eq!(c.source_count(), 2);
        assert!(c.validate().is_ok());

        // name 显式给了就用它
        let mut c2 = c.clone();
        c2.sources[1].name = Some("enc".to_string());
        c2.trigger.as_mut().unwrap().source.source = Some("enc".to_string());
        assert_eq!(c2.source_names(), vec!["loop", "enc"]);
        assert!(c2.validate().is_ok());
    }

    #[test]
    fn 通道引用按源名落到槽位_不指名时两个以上源是错() {
        let c = multi();
        let (slot, col) = c.resolve(&c.trigger.as_ref().unwrap().source).unwrap();
        assert_eq!(slot, 1);
        assert_eq!(col.to_string(), "0/theta");
        let (slot, _) = c.resolve(&c.vertical[0].channel).unwrap();
        assert_eq!(slot, 0);

        // 不指名：两个源以上不猜第 0 个
        let bare: ChanRef = "iu".parse().unwrap();
        let e = c.resolve(&bare).unwrap_err();
        assert!(
            e.contains("2 个源") && e.contains("loop / foc/rotor"),
            "{e}"
        );

        // 名字打错：当场报，并把有哪些源列出来
        let wrong: ChanRef = "rotor:theta".parse().unwrap();
        let e = c.resolve(&wrong).unwrap_err();
        assert!(e.contains("找不到源") && e.contains("foc/rotor"), "{e}");

        // 只有一个源时可以不指名
        let mut one = c.clone();
        one.sources.pop();
        one.trigger.as_mut().unwrap().source = "iu".parse().unwrap();
        one.measure.channels = vec!["iv".parse().unwrap()];
        one.vertical[0].channel = "iu".parse().unwrap();
        assert_eq!(one.resolve(&one.vertical[0].channel).unwrap().0, 0);
        assert!(one.validate().is_ok());
    }

    #[test]
    fn 源重名要报_因为引用按名字指路() {
        let mut c = multi();
        c.sources[1].node = None;
        c.sources[1].port = "loop".to_string();
        let e = c.validate().unwrap_err();
        assert!(e.contains("同一个名字") && e.contains("name"), "{e}");
    }

    #[test]
    fn 单源简写与多源二选一_简写下不许指名源() {
        let mut c = multi();
        c.channels = vec!["iu".parse().unwrap()];
        assert!(c.validate().unwrap_err().contains("二选一"));

        let mut c = setup(); // 简写
        c.trigger.as_mut().unwrap().source = "loop:z".parse().unwrap();
        let e = c.validate().unwrap_err();
        assert!(e.contains("单源简写") && e.contains("sources"), "{e}");
    }

    #[test]
    fn native_只能一个源_也不接受_fs_hz() {
        let mut c = multi();
        c.clock = ScopeClock::default();
        let e = c.validate().unwrap_err();
        assert!(e.contains("2 个源") && e.contains("fixed"), "{e}");

        c.sources.pop();
        c.trigger.as_mut().unwrap().source = "iu".parse().unwrap();
        c.measure.channels = vec!["iv".parse().unwrap()];
        c.vertical[0].channel = "iu".parse().unwrap();
        assert!(c.validate().is_ok(), "native + 一个显式源是合法的");

        c.clock.fs_hz = Some(1e6);
        assert!(c.validate().unwrap_err().contains("不接受 fs_hz"));

        let mut c = multi();
        c.clock.fs_hz = Some(0.0);
        assert!(c.validate().unwrap_err().contains("fs_hz"));
        c.clock.fs_hz = None; // auto
        assert!(c.validate().is_ok());
    }

    #[test]
    fn fixed_下触发源必须进环_native_下不必() {
        // fixed：环里只有选中的列，外触发无处可跑
        let mut c = multi();
        c.trigger.as_mut().unwrap().source = "loop:iw".parse().unwrap();
        let e = c.validate().unwrap_err();
        assert!(
            e.contains("trigger.source") && e.contains("必须进环"),
            "{e}"
        );

        // native：源列在写环时读整行，外触发照旧
        let mut c = setup();
        c.trigger.as_mut().unwrap().source = "iq_ref".parse().unwrap();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn 带源的通道引用_文本形式往返_序列化摊平不动老形状() {
        let r: ChanRef = "foc/rotor:2/iq".parse().unwrap();
        assert_eq!(r.source.as_deref(), Some("foc/rotor"));
        assert_eq!(
            r.column,
            ColumnRef::Column {
                column: "iq".into(),
                group: Some(2)
            }
        );
        assert_eq!(r.to_string(), "foc/rotor:2/iq");
        assert_eq!("@3".parse::<ChanRef>().unwrap().to_string(), "@3");
        assert!(":iq".parse::<ChanRef>().is_err(), "空源名要报");

        // 不指源的引用序列化后和老的 ColumnRef 一模一样
        let bare: ChanRef = "iu".parse().unwrap();
        assert_eq!(serde_json::to_string(&bare).unwrap(), r#"{"column":"iu"}"#);
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"source":"foc/rotor","column":"iq","group":2}"#
        );
        // 老写法照旧读得进来
        let old: ChanRef = serde_json::from_str(r#"{"column":"z"}"#).unwrap();
        assert_eq!(old, ChanRef::from("z".parse::<ColumnRef>().unwrap()));
    }

    #[test]
    fn 多源_setup_能写回_toml_再读回来() {
        // 绑定要落进 node.toml：摊平的引用必须能序列化成 TOML 再读回来
        let c = multi();
        let s = toml::to_string(&c).expect("serialize");
        let back: ScopeConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(back, c, "{s}");
        assert!(back.validate().is_ok());
    }

    #[test]
    fn 时钟与源进几何() {
        let a = multi();
        let mut b = a.clone();
        b.refresh_hz = 5.0;
        assert!(a.same_geometry(&b));
        let mut c = a.clone();
        c.clock.fs_hz = Some(2e6);
        assert!(!a.same_geometry(&c), "改示波器时钟要重建环");
        let mut d = a.clone();
        d.sources[0].channels.pop();
        assert!(!a.same_geometry(&d));
    }

    #[test]
    fn 绑定里的_scope_段随_widget_binding_往返() {
        let b: WidgetBinding = toml::from_str(&format!(
            "kind = \"port\"\ntarget = \"loop\"\n[scope]\n{}",
            SETUP
                .replace("[trigger]", "[scope.trigger]")
                .replace("[acq]", "[scope.acq]")
                .replace("[[vertical]]", "[[scope.vertical]]")
                .replace("[measure]", "[scope.measure]")
        ))
        .unwrap();
        let sc = b.scope.as_ref().expect("scope section");
        assert_eq!(sc.channels.len(), 4);
        assert!(b.tap.is_none());
        let s = serde_json::to_string(&b).unwrap();
        assert!(s.contains("\"scope\"") && !s.contains("\"tap\""), "{s}");
    }
}
