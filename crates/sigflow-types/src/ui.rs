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
    ///
    /// **读不了也不许丢**，见 [`ScopeSetup`]：拿 [`WidgetBinding::scope_config`]
    /// 取解析好的那份，拿 [`WidgetBinding::scope_error`] 取出错的原话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, as = "Option<ScopeConfig>"))]
    pub scope: Option<ScopeSetup>,
}

impl WidgetBinding {
    /// 解析好的示波器 setup；坏的（读不了的）返回 None——**坏的绝不能被当成配置用**。
    pub fn scope_config(&self) -> Option<&ScopeConfig> {
        match &self.scope {
            Some(ScopeSetup::Ok(c)) => Some(c),
            _ => None,
        }
    }

    /// 这份 setup 读不了的原话（能读就是 None）。控件拿它显示，并据此**关掉回写**。
    pub fn scope_error(&self) -> Option<&str> {
        match &self.scope {
            Some(ScopeSetup::Broken { error, .. }) => Some(error),
            _ => None,
        }
    }

    /// 有一份读不了的 setup 在这儿。
    pub fn scope_is_broken(&self) -> bool {
        self.scope_error().is_some()
    }
}

/// 绑定里的示波器 setup：**读得了就是配置，读不了就原样留着**。
///
/// 为什么不是 `Option<ScopeConfig>`（docs/scope-contract.md §9.4）：`bind.scope` 读不
/// 出来时 serde 会让整个 `node.toml` 解析失败，于是**整个节点起不来，数据面跟着没
/// 了**——一个调试用的示波器声明不该拖住电机控制器。所以坏的那份在这一层被接住：
/// 节点照常起，这台示波器标成坏的、大声报错（永久类失败，立刻喊，不等第一帧）。
///
/// **宽容读不许变成有损写**：坏的那份连原始的键值一起留着、序列化时一字不改地写回
/// 去，控件在用户明确重设之前**不许回写**——否则控件用缺省 setup 起来、用户随手拨一
/// 下旋钮，就把原来那份（源、触发、垂直全在里面）覆盖没了。**丢配置比起不来更糟：
/// 起不来看得见，覆盖看不见。**
///
/// 这是**发布前**的兜底，不是长期方案：真发布之后破坏性改动要走版本与迁移
/// （`scope.version` + 升级路径），不能靠"读不了就先留着"。
#[derive(Debug, Clone, PartialEq)]
pub enum ScopeSetup {
    Ok(Box<ScopeConfig>),
    /// 读不了：`raw` 是原样的值（写回去一字不改），`error` 是 serde 的原话。
    Broken {
        raw: serde_json::Value,
        error: String,
    },
}

impl ScopeSetup {
    pub fn as_config(&self) -> Option<&ScopeConfig> {
        match self {
            ScopeSetup::Ok(c) => Some(c),
            ScopeSetup::Broken { .. } => None,
        }
    }
}

impl From<ScopeConfig> for ScopeSetup {
    fn from(c: ScopeConfig) -> Self {
        ScopeSetup::Ok(Box::new(c))
    }
}

impl Serialize for ScopeSetup {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            ScopeSetup::Ok(c) => c.serialize(ser),
            // 原样写回去：宽容读不许变成有损写
            ScopeSetup::Broken { raw, .. } => raw.serialize(ser),
        }
    }
}

impl<'de> Deserialize<'de> for ScopeSetup {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        // 先接成通用值（这一步不会失败），再试着解析成配置——**失败也要把原值留住**
        let raw = serde_json::Value::deserialize(de)?;
        match ScopeConfig::deserialize(&raw) {
            Ok(c) => Ok(ScopeSetup::Ok(Box::new(c))),
            Err(e) => Ok(ScopeSetup::Broken {
                raw,
                error: e.to_string(),
            }),
        }
    }
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
    /// **有没有可能指着同一路**——不是"写法相等"（那是 `PartialEq`）。
    ///
    /// 单组口上 `{ column = "v" }`（每组）与 `{ column = "v", group = 0 }`（第 0
    /// 组）指着同一路却判不等,所以凡是要回答"这两个引用是不是同一路"的地方都不能
    /// 用 `==`。这里保守：**重合就算可能相同**（不带组序的覆盖该列所有组）。下标
    /// 与列名之间这一层分不开（要列契约才知道 `@0` 是不是 `v`),交给壳体。
    pub fn may_be_same(&self, other: &ColumnRef) -> bool {
        match (self, other) {
            (ColumnRef::Channel { channel: a }, ColumnRef::Channel { channel: b }) => a == b,
            (
                ColumnRef::Column { column: ca, group: ga },
                ColumnRef::Column { column: cb, group: gb },
            ) => ca == cb && (ga.is_none() || gb.is_none() || ga == gb),
            // 一个按下标、一个按列名：这一层看不出来,不拒（壳体落到通道后再判）
            _ => false,
        }
    }

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

/// 示波器的时钟：一台示波器的采样率是**推导出来的**，不是设死的。
///
/// 真示波器上没有"采样率"这个旋钮——你拨时基、选深度档，采样率是算出来的结果。
/// 三种模式：
///
/// - `auto`（**缺省**）：按控制律推导（见 [`ScopeConfig`] 顶上那段），随时基 /
///   深度档 / 通道数变。这是仪器的行为。
/// - `fixed { fs_hz }`：把率钉死，留给"我就是要这个率"的高级用法。**必须给
///   `fs_hz`**——原来"省略 = 取各源最快的率"那个语义作废了，按时基推导比它有用。
/// - `native`：环跟着**唯一**的源走——存源的原生 dtype、存储率 = `fs_native / D`
///   （D 整数、超预算走入口峰值检测、每 scan 存一对 min/max）。保留它是因为
///   100 MS/s 下峰值检测能保住 1 拍宽的毛刺，**任何重采样都会把毛刺滤掉**：抓毛刺
///   就得走这条。只能有一个源。
///
/// `auto` / `fixed` 下任何率、任何 dtype 的源都重采样到这根时钟上，于是多口、多节点
/// （同主机）的通道能进同一个环、同一次触发、同一屏；环每 scan 每列存**一个值**
/// （不存对），列 `kind` 是 enum / bitfield / counter 的一律 ZOH 不插值（给状态量插出
/// 中间值是画假）。
///
/// `fs_max_hz` 是**这台仪器的最大采样率**（推导的第一个上限，实际率还要除以通道
/// 数——真机上通道共用 ADC 就是这样）。省略 = 用壳体的上限（环境变量
/// `SIGFLOW_SCOPE_MAX_FS`）。上限归壳体、声明层只查 > 0，同 [`ScopeDepth`] 的分工。
///
/// **能照实读幅度的带宽只到 0.8 × fs/2**（≈ `fs/2.5`）：重采样的抗混叠滤波器在带
/// 顶要滚降（实测 0.8 × 奈奎斯特 93.8%、0.9 × 奈奎斯特 73.1%，见 docs/scope-contract.md
/// §8.4）。要量某个频率的峰峰值，时钟至少取它的 2.5 倍。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ScopeClock {
    #[serde(default)]
    pub mode: ClockMode,
    /// 只在 `fixed` 下有意义，且必须给。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub fs_hz: Option<f64>,
    /// 这台仪器的最大采样率（`auto` 推导用）。省略 = 壳体上限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub fs_max_hz: Option<f64>,
    /// 环里存什么（`auto` / `fixed`；`native` 存源的原生 dtype，这一项不看）。
    /// 省略 = `f32`，只许 `f32` 或 `i16`。
    ///
    /// **`i16` 的量化标度跟垂直档走**（[`VerticalSetting`]）：
    /// `标度 = v_div × 5 / 32767`、零点 = 该通道的 `offset`——屏幕 8 格，存 ±5 格
    /// 留一圈余量（真机的 ADC 覆盖比屏幕大一圈，同一个道理）。**代价是 `v_div` /
    /// `offset` 不再只是画法**：改了存进去的码就不是那个意思了，要清环重采
    /// （真机上改垂直档也是重新采集）。**所以 `i16` 下量程不许自适应**——没写
    /// [`VerticalSetting`] 的通道取列 `bound` 的满量程 / 8 格，没有 `bound` 就
    /// 1.0/格；拿首帧观测去自适应会在第一个观众到场时清掉常驻示波器攒的一切
    /// （同 docs/scope-contract.md §8.5 那条：**观测不许混进几何**）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub dtype: Option<crate::semantic::Dtype>,
}

/// 时钟模式，见 [`ScopeClock`]。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum ClockMode {
    /// 按控制律推导（缺省）。
    #[default]
    Auto,
    /// 钉死 `fs_hz`。
    Fixed,
    /// 跟着唯一的源走（抓毛刺）。
    Native,
}

impl ScopeClock {
    /// 重采样到示波器自己的时钟上（`auto` 或 `fixed`）——多源、混合率、i16 存储
    /// 都只在这两种下成立。
    pub fn is_resampled(&self) -> bool {
        self.mode != ClockMode::Native
    }

    pub fn is_fixed(&self) -> bool {
        self.mode == ClockMode::Fixed
    }

    /// 环里存的 dtype（`native` 下由源决定，返回 None）。
    pub fn ring_dtype(&self) -> Option<crate::semantic::Dtype> {
        self.is_resampled()
            .then(|| self.dtype.unwrap_or(crate::semantic::Dtype::F32))
    }

    pub fn validate(&self) -> Result<(), String> {
        match (self.mode, self.fs_hz) {
            (ClockMode::Fixed, None) => {
                return Err(
                    "clock: mode = fixed 要给 fs_hz（原来\"省略 = 取各源最快的率\"作废了——\
                            按时基推导写 mode = \"auto\"）"
                        .to_string(),
                )
            }
            (ClockMode::Auto, Some(_)) => return Err(
                "clock: mode = auto 的率是推导出来的，不接受 fs_hz（要钉死写 mode = \"fixed\"）"
                    .to_string(),
            ),
            (ClockMode::Native, Some(_)) => {
                return Err("clock: mode = native 跟着源走，不接受 fs_hz".to_string())
            }
            (_, Some(f)) if !(f.is_finite() && f > 0.0) => {
                return Err(format!("clock.fs_hz must be > 0 (got {f})"))
            }
            _ => {}
        }
        if let Some(f) = self.fs_max_hz {
            if !(f.is_finite() && f > 0.0) {
                return Err(format!("clock.fs_max_hz must be > 0 (got {f})"));
            }
        }
        match self.dtype {
            None => {}
            Some(d) if !self.is_resampled() => {
                return Err(format!(
                    "clock.dtype = {} 只对 auto / fixed 有意义：native 存源的原生 dtype",
                    d.as_str()
                ))
            }
            Some(crate::semantic::Dtype::F32) | Some(crate::semantic::Dtype::I16) => {}
            Some(d) => {
                return Err(format!(
                    "clock.dtype 只许 f32 或 i16（got {}）——环里存的是重采样后的物理量",
                    d.as_str()
                ))
            }
        }
        Ok(())
    }
}

/// 存储深度：**按采样点算，不按字节**——仪器上写的是"100 Mpts"，不是"400 MB"。
///
/// `points` 是**所有通道合起来**的点数（真机的存储深度也是共用的）：
/// `每通道 = floor(points / 通道数)`。深度档是 UI 的事——控件按 `max_points` 摆一
/// 排档（十分之一、1-2-5、随便），**setup 里存的是选出来的点数本身**，不是档序号：
/// 档序号的含义是一条公式，公式一旦在控件和壳体里各写一份就会悄悄对不上，屏幕上
/// 写的深度和环里的深度不是一个数（同 [`ColumnRef`] 的"写法相等"、§8.5 的"观测混进
/// 几何"是同一族账）。
///
/// 字节数的上限归壳体（环境变量），这里只查点数自洽。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ScopeDepth {
    /// 这台示波器的存储深度规格（点数，所有通道合计）。
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub max_points: u64,
    /// 当前选的深度档（点数，≤ `max_points`）。省略 = 用满。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "number"))]
    pub points: Option<u64>,
}

impl ScopeDepth {
    /// 当前档的点数（所有通道合计）。
    pub fn points(&self) -> u64 {
        self.points.unwrap_or(self.max_points).min(self.max_points)
    }

    /// 每通道能存多少点。通道数为 0 时返回 0。
    pub fn per_channel(&self, channels: usize) -> u64 {
        if channels == 0 {
            0
        } else {
            self.points() / channels as u64
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.max_points == 0 {
            return Err("depth.max_points must be > 0".to_string());
        }
        match self.points {
            Some(0) => Err("depth.points must be > 0（省略 = 用满 max_points）".to_string()),
            Some(p) if p > self.max_points => Err(format!(
                "depth.points {p} 超过这台的规格 max_points {}",
                self.max_points
            )),
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
///
/// **"记录"有两个来源，persist 说的是两个都算**（§9.5 之后就是这样）：触发扫描器
/// 出的那些，和**自由跑按屏长切出来的**那些。后者是 §9.5 为了"常驻示波器攒了一小时
/// 却一个入口都没有"补的，`roll` 档也照切。
///
/// 这句话曾经只兑现了一半：自由跑的分段落在引擎里一个**独立的分支**里，接上了分段
/// 翻页、没接上密度累加，于是自由跑 + persist 没有余辉。**已修**（sigflow-core
/// `bcef4e8`：自由跑切出的记录照样喂密度图，按算术级数取第 k 条、不先攒 Vec——那条
/// 采集线程不能拖）。留着这一段是因为**根因值得记**：`记录` 多了一个来源，而只接上
/// 了当时想着的那个消费者，没人回头问"还有谁在消费记录"（见 docs/scope-contract.md
/// §9.7 与 §13.7）。
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
    /// 束的沉积量（辉度）：画一步在余辉图上留下多少"墨"。
    ///
    /// **一步一份固定能量，摊在它扫过的格子上**——这是 CRT 的物理：束按**时间**沉积
    /// 能量，走得快的那一段（陡沿）把同一份能量摊到很多格子上，所以陡沿淡、平台亮。
    /// 早先两边都做反了：X-Y 每段按常数 alpha 画、Y-T 的密度图每个纵向格子 `+1`，
    /// 于是**跨 200 格的陡沿沉积了平台的 200 倍**，真机上恰好相反（sigflow-core
    /// `aae85f0` 改的；受控对照：旧 圆 145 / 转场 155 = 0.9，新 255 / 3 = 85）。
    ///
    /// **放 `acq` 不放 `display`**：它管的是**怎么累**（跟 `persist_decay` 一伙），
    /// 不是怎么投影。而且它**必须两种画法同一个数**——辉度旋钮在 X-Y 和 Y-T 上意思
    /// 不同，就是我们记了一路的那种病。
    ///
    /// 改它**要清密度图**（旧计数是按旧标度沉积的，混在一起没有意义），**不动环**
    /// ——跟改 `v_div` 同一条规矩，见 docs/scope-contract.md §13.1。
    #[serde(default = "default_beam_ink")]
    pub beam_ink: f32,
}

fn default_average_n() -> u32 {
    16
}

fn default_persist_decay() -> f32 {
    0.9
}

/// 现在引擎里写死的那个值——换成字段是为了让它可调,不是为了换默认。
fn default_beam_ink() -> f32 {
    64.0
}

impl Default for AcqSetting {
    fn default() -> Self {
        AcqSetting {
            mode: AcqMode::Normal,
            average_n: default_average_n(),
            persist_decay: default_persist_decay(),
            beam_ink: default_beam_ink(),
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
        // 上界不设：辉度拧到顶是"糊成一片白",难看但不是错,而且真机也让你拧到糊。
        // 0 是合法的（不留墨 = 关掉余辉的另一种说法）；负数和 NaN 不是。
        if !(self.beam_ink.is_finite() && self.beam_ink >= 0.0) {
            return Err(format!(
                "acq.beam_ink must be finite and >= 0 (got {})",
                self.beam_ink
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

/// 逐通道的垂直设置，键是列引用。没写的通道取缺省。
///
/// **`clock.dtype = i16` 时这里不再只是画法**：环里存的码按 `v_div × 5 / 32767`
/// 量化、以 `offset` 为零点（屏幕 8 格，存 ±5 格留一圈余量），所以改 `v_div` /
/// `offset` **要清环**——存着的码不再是那个意思了（真机上改垂直档也是重新采集）。
/// 相应地 **i16 下量程不许自适应**：没写这条的通道取列 `bound` 的满量程 / 8 格、
/// 没有 `bound` 就 1.0/格，都是**声明**；拿首帧观测去自适应会在第一个观众到场时把
/// 常驻示波器攒的一切清掉。`f32` 下（缺省）它仍然只是画法，自适应随便。
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

/// 屏幕画法：Y-T（横轴时间，缺省）还是 X-Y（一路当横轴、一路当纵轴）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum DisplayMode {
    /// 横轴是时间——普通示波器。
    #[default]
    Yt,
    /// 横轴是一路信号：李萨如、矢量画、示波器音乐走这条。
    Xy,
}

/// 画法设置。**它只改画，不改采集**——同一帧数据，换个画法而已，所以它
/// **不进 [`ScopeConfig::same_geometry`]**：在 Y-T 和 X-Y 之间切不该重建环、
/// 不该把攒下的历史清掉（拿这条当判据的那一族账见 docs/scope-contract.md §11）。
///
/// X-Y 靠"同一拍"成立：一帧里每一路按拍对齐，第 i 个 x 与第 i 个 y 是同一拍。
/// **但这只在视图发原始样本时为真**——视图每像素列不足两拍才发原始样本
/// （`span < 2 × px`，且 ≤ 65536 拍），再长就退化成每列的 (min, max)，那时
/// `x[i]` 与 `y[i]` 是**一段窗口里的极值**而不是同一拍，连出来的不是轨迹。
/// 所以 X-Y 要把一屏收短（48 kHz 的音频、px = 1024 时，一屏 20–40 ms）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct DisplaySetting {
    #[serde(default)]
    pub mode: DisplayMode,
    /// 横轴那一路，`mode = "xy"` 时必填。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub x: Option<ChanRef>,
    /// 纵轴那一路，`mode = "xy"` 时必填。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub y: Option<ChanRef>,
    /// 辉光强度 `[0, 1]`，0 = 关。玻璃和荧光粉的散射，**看的时候加的**。
    ///
    /// **缺省 0.55 / 2.5 不是"新功能默认关"，是"缺键 = 以前那样"**：辉光在契约里是
    /// 新的，在屏幕上不是——它早就以写死的常数在前端无条件叠加（`Oscilloscope.svelte`
    /// 的 `BLOOM_GAIN = 0.55` / `BLOOM_PX = 2.5`），hml 正是看着那个效果说"不错"才要
    /// 求把它提成滑块的。缺省定 0 的话，他现在看到的辉光会在下次部署后消失，而**配置
    /// 文件里什么都没变**——那是最难查的一种。要关就显式写 0。
    #[serde(default = "default_bloom_gain")]
    pub bloom_gain: f32,
    /// 辉光半径，**屏幕像素**，0 = 关。
    ///
    /// 按屏幕像素给、不按纹理格子给：密度纹理的分辨率随时基和通道数变，按格子给的话
    /// **同一个设置在不同时基下糊出来的圈会一大一小**——那是把一个显示量交给了一个
    /// 会自己变的标度（同 §13.2 那条"别写死一个会漂的数"）。
    #[serde(default = "default_bloom_px")]
    pub bloom_px: f32,
}

impl Default for DisplaySetting {
    fn default() -> Self {
        DisplaySetting {
            mode: DisplayMode::default(),
            x: None,
            y: None,
            bloom_gain: default_bloom_gain(),
            bloom_px: default_bloom_px(),
        }
    }
}

/// = 前端 `BLOOM_GAIN`，把写死的常数提成字段,缺省取它本来的值。
fn default_bloom_gain() -> f32 {
    0.55
}

/// = 前端 `BLOOM_PX`。
fn default_bloom_px() -> f32 {
    2.5
}

impl DisplaySetting {
    pub fn is_xy(&self) -> bool {
        self.mode == DisplayMode::Xy
    }

    /// 辉光开着吗——两项都要有效才算开（半径 0 或强度 0 都是关）。
    pub fn bloom_on(&self) -> bool {
        self.bloom_gain > 0.0 && self.bloom_px > 0.0
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(self.bloom_gain.is_finite() && (0.0..=1.0).contains(&self.bloom_gain)) {
            return Err(format!(
                "display.bloom_gain must be within [0, 1] (got {})",
                self.bloom_gain
            ));
        }
        // 半径上界不设：糊成一团是难看,不是错。负数和 NaN 是错。
        if !(self.bloom_px.is_finite() && self.bloom_px >= 0.0) {
            return Err(format!(
                "display.bloom_px must be finite and >= 0 (got {})",
                self.bloom_px
            ));
        }
        Ok(())
    }
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
/// **采样率与存储点数是推导出来的，不是设死的**（`clock.mode = auto`，缺省）：
///
/// ```text
/// 窗口时间   = timebase.span_s                 （**一屏**；屏上十格，s/div = span_s / 10）
/// 每通道深度 = floor(depth.points / 通道数)
/// fs         = min( clock.fs_max_hz / 通道数,                 ← 通道交织，不取档
///                    1-2.5-5 向下取档( 每通道深度 / 窗口时间 ), ← 只有这一路取档
///                    源的率 )
/// 存储点数   = 窗口时间 × fs                    （≤ 每通道深度）
/// ```
///
/// 1 GSa/s、深度 100 M、单通道：1 ns/div → 率撞上限、一屏只存 10 点；10 ms/div →
/// 正好占满 100 M；20 ms/div → 率掉到 500 MSa/s；30 ms/div → 333.3 MSa/s 向下取到
/// **250 MSa/s、存 75 M**。
///
/// **控制律只有一份，在壳体里**：前端不许自己算一份，率和深度都从引擎的状态里读。
/// 两份实现迟早对不上，那时屏幕上写的和环里的不是一个数——同 [`ColumnRef`] 的"写法
/// 相等"、docs/scope-contract.md §8.5 的"观测混进几何"是同一族账。
///
/// 推导的输入**只许是声明**（时基、深度、通道数、`fs_max_hz`）加**已经稳定的观测**
/// （源的率）。观测那一项要跟环一起记住：认领时重算得到同一个数才不清环——第一个
/// 观众到场时把攒的清掉，正是 §8.5 修过的那个 bug。
///
/// 几何（改了要**重建**环）= `clock` + `channels` / `sources` + `depth` +
/// `budget_bytes_per_s`；率或 i16 的量化标度变了要**清环**（数据丢掉、分配不动）；
/// 其余就地生效。
///
/// **寿命看它是谁的**（docs/scope-contract.md §8.5）：`scope_create` 临时开的那台是
/// **会话的**，跟 WebSocket 连接走、断线即收；写在 `bind.scope` 里的这一份是**节点
/// 的**——声明就是拥有：节点起来它就起来、一直攒，浏览器刷新重新接上同一个环，
/// 节点停或绑定删了才释放。身份是 **(挂控件的节点, 控件 alias)**，不是 `scope_id`。
/// 深度在节点启动时就占住，装不下要在启动时报错，不许悄悄缩小。
///
/// ```toml
/// [widgets.bind.scope]
/// channels = [{ column = "iu" }, { column = "iv" }, { column = "iw" }, { column = "z" }]
/// depth = { max_points = 100000000 }        # 100 Mpts，四通道分摊
/// clock = { mode = "auto", fs_max_hz = 1e9 }
/// refresh_hz = 30
/// timebase = { span_s = 0.002, position = 0.5 }   # 一屏 2 ms = 200 µs/div
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
/// clock = { mode = "fixed", fs_hz = 1000000 }   # 钉死；按时基推导写 mode = "auto"
/// depth = { max_points = 100000000, points = 60000000 }   # 选了 60 Mpts 那一档
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
    ///
    /// **这是"采哪些"，不是"画哪些"**（2026-09-02，hml 定：跟真机一致）。通道栏
    /// 去掉勾选 = 那一路不进环，让出来的深度和采样率归其余通道（控制律里每通道
    /// 深度 = 总点数 ÷ 通道数）。于是每一路有三种状态，别把后两种混成一种：
    ///
    /// | 状态 | 在 `channels` | `vertical.on` | 含义 |
    /// |---|---|---|---|
    /// | 关 | 否 | —— | 不采集、不占深度和率；档位仍留在 `vertical` 里 |
    /// | 采而不画 | 是 | `false` | 进环、占深度；屏上不画。**触发源要藏就用这个** |
    /// | 开 | 是 | `true` | 进环并画 |
    ///
    /// **勾选是几何改动**：通道数变了 → 每通道深度变了 → 率变了 → 环重建，攒的
    /// 历史当场没。所以 UI 上**停下来之后勾选不能进引擎**（同"停下来改时基"那条：
    /// 停着的时候时基、垂直、勾选都只能是"看"的参数，走本地视图）——否则取消一个
    /// 勾选就把你正盯着的那一屏删了。
    #[serde(default)]
    pub channels: Vec<ColumnRef>,
    /// 多源：一台示波器收几个口（可跨节点，同主机）的通道。非空时以它为准，
    /// `channels` 必须空。两个以上源要 `clock.mode = fixed`。
    #[serde(default)]
    pub sources: Vec<ScopeSource>,
    /// 存储深度，**按采样点**（见 [`ScopeDepth`]）。字节上限归壳体。
    pub depth: ScopeDepth,
    /// 写环字节预算（B/s），**只对 `clock = native` 有意义**：超了先向生产方请求
    /// 降率（口声明了 `negotiable.rate_param`），不支持就入口峰值检测抽取——峰值
    /// 存储每 scan 存一对，抽取比按存储字节算。
    ///
    /// `auto` / `fixed` 下率是推导出来的（`fs_max / 通道数` 已经把吞吐封住了，
    /// `通道数 × fs × 位宽 ≡ fs_max × 位宽`），这一项不参与；机器扛不扛得住由壳体
    /// 说了算。
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
    /// **每通道的档位表**，不是"要画的曲线表"：允许包含此刻不在 `channels` 里的
    /// 通道——关掉一路再打开，V/div、偏移、耦合还是原来那份（真机就是这样）。
    /// 所以取消勾选时**不要删掉对应的 `vertical`**，删了档位就丢了。
    ///
    /// 引用一个没在采集的通道不是错，`validate` 跳过它；引用一个**列契约里根本
    /// 没有**的列仍然是错，由壳体解析时报（类型层看不见列契约，分不开这两种）。
    #[serde(default)]
    pub vertical: Vec<VerticalSetting>,
    /// 量哪些通道。与 `vertical` 同样的规则：没在采集的通道跳过（不出读数，也不
    /// 报错），列契约里没有的列由壳体当场报。
    #[serde(default)]
    pub measure: MeasureSetting,
    /// 画法（Y-T / X-Y）。只改画不改采集，**不进 `same_geometry`**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub display: Option<DisplaySetting>,
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
    /// 几何相同 = 环不用重建。
    ///
    /// **时基不在里面，但它会改推导出来的率**（`auto` 下），而率变了环里就是两种率
    /// 混着的数据——那要**清环**（数据丢掉、分配不动），不是重建。两件事别混：
    /// 重建是重新分配（深度 / 通道 / 时钟模式变了），清环是丢数据（率、i16 的量化
    /// 标度变了）。同一档内微调时基推不出新的率，就什么都不用做。
    pub fn same_geometry(&self, other: &ScopeConfig) -> bool {
        self.clock == other.clock
            && self.channels == other.channels
            && self.sources == other.sources
            && self.depth == other.depth
            && self.budget_bytes_per_s == other.budget_bytes_per_s
    }

    /// 声明层面的自洽性。列 id 存不存在、节点/口在不在图上、深度与
    /// `clock.fs_hz` 超不超壳体上限，要到壳体才知道。
    pub fn validate(&self) -> Result<(), String> {
        self.depth.validate().map_err(|e| format!("depth: {e}"))?;
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
        if self.sources.len() > 1 && !self.clock.is_resampled() {
            return Err(format!(
                "{} 个源要重采样到示波器自己的时钟上（clock.mode = \"auto\" 或 \"fixed\"）：\
                 native 是跟着唯一的源走（原生 dtype + 整数抽取），多源没有共同网格",
                self.sources.len()
            ));
        }
        self.timebase.validate()?;
        if self.clock.mode == ClockMode::Auto && self.timebase.span_scans.is_some() {
            return Err(
                "timebase: clock = auto 要按时间推导采样率（一屏 = 10 × 时基），所以跨度要写 span_s；\
                 span_scans 是给没有率的口用的，那种口配 mode = \"native\""
                    .to_string(),
            );
        }
        if let Some(t) = &self.trigger {
            t.validate().map_err(|e| format!("trigger: {e}"))?;
            let (slot, col) = self
                .resolve(&t.source)
                .map_err(|e| format!("trigger.source: {e}"))?;
            // native：源列在写环时读整行，外触发不必在选列里。
            // auto / fixed：环里只有选中的列，没进环就不在共同网格上，触发跑不了。
            if self.clock.is_resampled() && !col.selected_in(self.channels_of(slot)) {
                return Err(format!(
                    "trigger.source: {} 不在选列里——重采样的时钟（auto / fixed）下环里只有选中的列，\
                     没进环的列不在共同网格上：把它选进 channels（不想看就 vertical.on = false），\
                     或者用 clock = \"native\" 走外触发",
                    t.source
                ));
            }
        }
        self.acq.validate()?;
        if self.acq.mode == AcqMode::Average && self.trigger.is_none() {
            return Err("acq.mode = average needs a trigger".to_string());
        }
        // vertical / measure 引用**没在采集的通道**不是错：`channels` 是"采哪些"，
        // `vertical` 是每通道的档位表，关掉的通道档位要留着（真机关掉 CH2 再打开，
        // 还是原来那个 V/div）。这里只校验引用本身立得住：源名在、写法合法。
        //
        // **写错列名谁来抓**：不是这里。类型层手里没有口的列契约，"合法但这轮没勾
        // 选"和"根本不存在的列"在这一层长得一模一样（第七条那族：单个观察与多种状
        // 态相容，再怎么盯也分不开）。壳体解析 spec 时对着列契约查，查不到当场报，
        // **不许静默跳过**——放开这条的代价，就是那半张网必须在壳体那边补上。
        for (i, v) in self.vertical.iter().enumerate() {
            v.validate().map_err(|e| format!("vertical[{i}]: {e}"))?;
            self.resolve(&v.channel)
                .map_err(|e| format!("vertical[{i}].channel: {e}"))?;
        }
        self.measure.validate()?;
        for (i, c) in self.measure.channels.iter().enumerate() {
            c.validate()
                .map_err(|e| format!("measure.channels[{i}]: {e}"))?;
            self.resolve(c)
                .map_err(|e| format!("measure.channels[{i}]: {e}"))?;
        }
        if let Some(d) = &self.display {
            d.validate().map_err(|e| format!("display: {e}"))?;
            if d.is_xy() {
                let (x, y) = match (&d.x, &d.y) {
                    (Some(x), Some(y)) => (x, y),
                    _ => {
                        return Err(
                            "display: mode = \"xy\" 要给 x 和 y（横轴、纵轴各一路）".to_string()
                        )
                    }
                };
                let (sx, cx) = self
                    .resolve(x)
                    .map_err(|e| format!("display.x: {e}"))?;
                let (sy, cy) = self
                    .resolve(y)
                    .map_err(|e| format!("display.y: {e}"))?;
                // **不能拿写法相等来判"是不是同一路"**（ColumnRef 的 PartialEq 是写法
                // 相等）：单组口上 `{column="l"}` 与 `{column="l", group=0}` 指着同一
                // 路却判不等。这里判的是"**有没有可能是同一路**"——重合就拒，宁可保守。
                // 落到具体通道之后的最终判定归壳体（它才有列契约，见 §12.3）。
                if sx == sy && cx.may_be_same(cy) {
                    return Err(format!(
                        "display: x 与 y 是同一路（{x} / {y}）——X-Y 的两轴要两路不同的信号，\
                         同一路连出来只会是一条对角线"
                    ));
                }
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
depth = { max_points = 100000000 }
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
        // 没有率的口：跨度按拍数，时钟只能跟源
        let c: ScopeConfig = toml::from_str(
            r#"
depth = { max_points = 20000000, points = 10000000 }
clock = { mode = "native" }
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
    fn 辉光是显示设置_缺省等于以前那样_两项都要有效才算开() {
        // bloom 是**看的时候**加的(玻璃和荧光粉的散射),不进累积层——所以它跟
        // beam_ink 正相反：改它连密度图都不用重来。当然更不该进 same_geometry。
        let a = setup();
        let mut b = setup();
        b.display = Some(DisplaySetting { bloom_gain: 0.6, bloom_px: 3.0, ..Default::default() });
        assert!(b.validate().is_ok(), "{:?}", b.validate());
        assert!(a.same_geometry(&b), "辉光不该进 same_geometry");

        // **缺省 = 以前那样**,不是"新功能默认关"：辉光早就以写死的常数在前端无条件
        // 叠加(BLOOM_GAIN 0.55 / BLOOM_PX 2.5),缺省定 0 会让屏上已有的辉光在下次
        // 部署后消失、而配置文件一个字没变。要关就显式写 0。
        let d = DisplaySetting::default();
        assert_eq!((d.bloom_gain, d.bloom_px), (0.55, 2.5));
        assert!(d.bloom_on(), "缺省就是开的");
        // 老 setup(没有 display 段)读进来也该是这个值
        let old: ScopeConfig = toml::from_str(
            "depth = { max_points = 1000 }\nclock = { mode = \"native\" }\n\
             timebase = { span_scans = 100 }\n[display]\nmode = \"xy\"\n\
             x = { column = \"iu\" }\ny = { column = \"iv\" }\n",
        )
        .unwrap();
        let od = old.display.as_ref().unwrap();
        assert_eq!((od.bloom_gain, od.bloom_px), (0.55, 2.5), "缺键要解成以前那样");
        // 显式关得写出来
        let off: ScopeConfig = toml::from_str(
            "depth = { max_points = 1000 }\nclock = { mode = \"native\" }\n\
             timebase = { span_scans = 100 }\n[display]\nbloom_gain = 0.0\n",
        )
        .unwrap();
        assert!(!off.display.as_ref().unwrap().bloom_on());

        // 两项都要有效才算开——只给一项等于没开,别让人以为拧了强度就该亮
        assert!(!DisplaySetting { bloom_gain: 0.9, bloom_px: 0.0, ..Default::default() }.bloom_on());
        assert!(!DisplaySetting { bloom_gain: 0.0, bloom_px: 8.0, ..Default::default() }.bloom_on());
        assert!(DisplaySetting { bloom_gain: 0.1, bloom_px: 1.0, ..Default::default() }.bloom_on());

        // 强度有上界(它是个比例),半径没有(糊成一团是难看不是错)
        for bad in [-0.1f32, 1.1, f32::NAN] {
            let mut c = setup();
            c.display = Some(DisplaySetting { bloom_gain: bad, ..Default::default() });
            assert!(c.validate().is_err(), "bloom_gain = {bad} 该拒");
        }
        for bad in [-1.0f32, f32::NAN, f32::INFINITY] {
            let mut c = setup();
            c.display = Some(DisplaySetting { bloom_px: bad, ..Default::default() });
            assert!(c.validate().is_err(), "bloom_px = {bad} 该拒");
        }
        let mut c = setup();
        c.display = Some(DisplaySetting { bloom_gain: 1.0, bloom_px: 1e4, ..Default::default() });
        assert!(c.validate().is_ok(), "半径不设上界");
    }

    #[test]
    fn 辉度是采集设置_可为零_不设上界_而且不进几何() {
        // beam_ink 管"怎么累"(跟 persist_decay 一伙),不是"怎么投影"——所以它跟
        // vertical / display 一样：能让密度图重来,但不动环。
        let a = setup();
        let mut b = setup();
        b.acq.beam_ink = 200.0;
        assert!(b.validate().is_ok());
        assert!(a.same_geometry(&b), "辉度不该进 same_geometry");

        // 0 合法：不留墨 = 关掉余辉的另一种说法
        let mut c = setup();
        c.acq.beam_ink = 0.0;
        assert!(c.validate().is_ok());

        // 负数和 NaN 不合法;上界不设(拧到糊是难看,不是错)
        for bad in [-1.0f32, f32::NAN, f32::INFINITY] {
            let mut c = setup();
            c.acq.beam_ink = bad;
            assert!(c.validate().is_err(), "beam_ink = {bad} 该拒");
        }
        let mut c = setup();
        c.acq.beam_ink = 1e6;
        assert!(c.validate().is_ok(), "上界不设");

        // 老 setup 没有这一项 → 缺省 64（换成字段不是为了换默认）
        let d: ScopeConfig = toml::from_str(
            "depth = { max_points = 1000 }\nclock = { mode = \"native\" }\n\
             timebase = { span_scans = 100 }\n[acq]\nmode = \"persist\"\n",
        )
        .unwrap();
        assert_eq!(d.acq.beam_ink, 64.0);
    }

    #[test]
    fn xy_画法_两轴必填且不能是同一路_但它不进几何() {
        // 画法只改画不改采集：切 Y-T / X-Y 不该重建环（那会把攒的历史清掉）
        let a = setup();
        let mut b = setup();
        b.display = Some(DisplaySetting {
            mode: DisplayMode::Xy,
            x: Some("iu".parse().unwrap()),
            y: Some("iv".parse().unwrap()),
            ..Default::default()
        });
        assert!(b.validate().is_ok(), "{:?}", b.validate());
        assert!(a.same_geometry(&b), "display 不该进 same_geometry");

        // xy 要两轴都给
        let mut c = setup();
        c.display = Some(DisplaySetting {
            mode: DisplayMode::Xy,
            x: Some("iu".parse().unwrap()),
            y: None,
            ..Default::default()
        });
        assert!(c.validate().unwrap_err().contains("x 和 y"));

        // 同一路：写法一样的
        let mut c = setup();
        c.display = Some(DisplaySetting {
            mode: DisplayMode::Xy,
            x: Some("iu".parse().unwrap()),
            y: Some("iu".parse().unwrap()),
            ..Default::default()
        });
        assert!(c.validate().unwrap_err().contains("同一路"));

        // 同一路：写法**不一样**但指着同一路（不带组序 vs 带组序）——拿 == 判会漏掉
        let mut c = setup();
        c.display = Some(DisplaySetting {
            mode: DisplayMode::Xy,
            x: Some("iu".parse().unwrap()),
            y: Some("0/iu".parse().unwrap()),
            ..Default::default()
        });
        assert_ne!(
            c.display.as_ref().unwrap().x,
            c.display.as_ref().unwrap().y,
            "这两个引用写法不等——正是 == 判不出来的那种"
        );
        assert!(
            c.validate().unwrap_err().contains("同一路"),
            "写法不等但指着同一路,也要拒"
        );

        // 不同组是两路,别误伤
        let mut c = setup();
        c.display = Some(DisplaySetting {
            mode: DisplayMode::Xy,
            x: Some("0/iu".parse().unwrap()),
            y: Some("1/iu".parse().unwrap()),
            ..Default::default()
        });
        assert!(c.validate().is_ok(), "{:?}", c.validate());

        // yt（缺省）不要求 x/y
        let mut c = setup();
        c.display = Some(DisplaySetting::default());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn 关掉的通道留着档位不算错_但源名写错照旧报() {
        // `channels` 是"采哪些"：去掉勾选那一路不进环，而它的档位要留在 vertical
        // 里（关掉 CH2 再打开还是原来那个 V/div）。所以引用一个没勾选的通道 = 对。
        let mut c = setup();
        c.vertical[0].channel = "iq".parse().unwrap();
        assert!(
            c.validate().is_ok(),
            "没在采集的通道留着档位不该被拒：{:?}",
            c.validate()
        );

        let mut c = setup();
        c.measure.channels = vec!["iq".parse().unwrap()];
        assert!(c.validate().is_ok(), "measure 同理");

        // 但引用立不住仍然当场报：源名不在 sources 里。列名写错这一层看不见
        // （类型层没有列契约），那半张网在壳体解析 spec 时补。
        let mut c = setup();
        c.channels.clear();
        c.sources = vec![
            ScopeSource { name: None, node: None, port: "loop".into(), channels: vec![] },
            ScopeSource { name: None, node: None, port: "rotor".into(), channels: vec![] },
        ];
        c.clock.mode = ClockMode::Fixed;
        c.clock.fs_hz = Some(20000.0);
        c.trigger.as_mut().unwrap().source = "loop:iu".parse().unwrap();
        c.vertical[0].channel = "nosuch:iu".parse().unwrap();
        let e = c.validate().unwrap_err();
        assert!(
            e.contains("vertical[0].channel") && e.contains("nosuch"),
            "{e}"
        );

        // 空选列 = 整口：什么通道都算在里面
        let mut c = setup();
        c.channels.clear();
        c.vertical[0].channel = "iq".parse().unwrap();
        assert!(c.validate().is_ok());

        // 外触发（源列不在选中列里）只在 native 下成立：重采样的环里只有选中的列
        let mut c = setup();
        c.trigger.as_mut().unwrap().source = "iq_ref".parse().unwrap();
        let e = c.validate().unwrap_err();
        assert!(e.contains("trigger.source") && e.contains("native"), "{e}");
        c.clock.mode = ClockMode::Native;
        c.timebase.span_s = None;
        c.timebase.span_scans = Some(2000);
        assert!(c.validate().is_ok(), "native 下外触发照旧");
    }

    #[test]
    fn 数值边界() {
        let mut c = setup();
        c.depth.max_points = 0;
        assert!(c.validate().unwrap_err().contains("max_points"));
        let mut c = setup();
        c.depth.points = Some(c.depth.max_points + 1);
        assert!(c.validate().unwrap_err().contains("max_points"));
        let mut c = setup();
        c.depth.points = Some(0);
        assert!(c.validate().unwrap_err().contains("points"));
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
        c.depth.max_points *= 2;
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
depth = { max_points = 100000000 }
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
    fn 多源要重采样的时钟_native_只能一个源() {
        // 缺省是 auto——也是重采样，多源在它下面成立
        let mut c = multi();
        c.clock = ScopeClock::default();
        assert_eq!(c.clock.mode, ClockMode::Auto, "缺省是推导，不是跟源");
        assert!(c.validate().is_ok(), "auto 也能多源");

        c.clock.mode = ClockMode::Native;
        let e = c.validate().unwrap_err();
        assert!(e.contains("2 个源") && e.contains("native"), "{e}");

        c.sources.pop();
        c.trigger.as_mut().unwrap().source = "iu".parse().unwrap();
        c.measure.channels = vec!["iv".parse().unwrap()];
        c.vertical[0].channel = "iu".parse().unwrap();
        assert!(c.validate().is_ok(), "native + 一个显式源是合法的");
    }

    #[test]
    fn 三种时钟各自认什么() {
        // fixed 必须给 fs_hz（原来"省略 = 取各源最快的率"作废）
        let mut c = multi();
        c.clock = ScopeClock {
            mode: ClockMode::Fixed,
            fs_hz: None,
            ..Default::default()
        };
        assert!(c.validate().unwrap_err().contains("要给 fs_hz"));
        c.clock.fs_hz = Some(0.0);
        assert!(c.validate().unwrap_err().contains("fs_hz"));
        c.clock.fs_hz = Some(1e6);
        assert!(c.validate().is_ok());

        // auto 不接受 fs_hz（率是推导出来的）；native 也不接受
        for m in [ClockMode::Auto, ClockMode::Native] {
            let mut c = multi();
            c.sources.truncate(1);
            c.trigger.as_mut().unwrap().source = "iu".parse().unwrap();
            c.measure.channels = vec!["iv".parse().unwrap()];
            c.vertical[0].channel = "iu".parse().unwrap();
            c.clock = ScopeClock {
                mode: m,
                fs_hz: Some(1e6),
                ..Default::default()
            };
            assert!(c.validate().unwrap_err().contains("不接受 fs_hz"), "{m:?}");
        }

        // fs_max_hz 只查有限且 > 0（真上限归壳体）
        let mut c = multi();
        c.clock.fs_max_hz = Some(-1.0);
        assert!(c.validate().unwrap_err().contains("fs_max_hz"));
        c.clock.fs_max_hz = Some(1e9);
        assert!(c.validate().is_ok());

        // 环里的 dtype：只许 f32 / i16，且 native 下没意义
        use crate::semantic::Dtype;
        let mut c = multi();
        c.clock.dtype = Some(Dtype::I16);
        assert!(c.validate().is_ok());
        assert_eq!(c.clock.ring_dtype(), Some(Dtype::I16));
        c.clock.dtype = Some(Dtype::I32);
        assert!(c.validate().unwrap_err().contains("只许 f32 或 i16"));
        let mut c = multi();
        c.sources.truncate(1);
        c.trigger.as_mut().unwrap().source = "iu".parse().unwrap();
        c.measure.channels = vec!["iv".parse().unwrap()];
        c.vertical[0].channel = "iu".parse().unwrap();
        c.clock = ScopeClock {
            mode: ClockMode::Native,
            dtype: Some(Dtype::I16),
            ..Default::default()
        };
        assert!(c
            .validate()
            .unwrap_err()
            .contains("native 存源的原生 dtype"));
        assert_eq!(
            ScopeClock {
                mode: ClockMode::Native,
                ..Default::default()
            }
            .ring_dtype(),
            None
        );
    }

    #[test]
    fn auto_下跨度要按时间_深度按点数分摊() {
        let mut c = multi();
        c.clock = ScopeClock::default();
        c.timebase.span_s = None;
        c.timebase.span_scans = Some(4000);
        let e = c.validate().unwrap_err();
        assert!(e.contains("span_s") && e.contains("native"), "{e}");

        // 深度是所有通道合计，每通道按通道数分摊（控制律的第二个上限）
        let d = ScopeDepth {
            max_points: 100_000_000,
            points: Some(60_000_000),
        };
        assert_eq!(d.points(), 60_000_000);
        assert_eq!(d.per_channel(4), 15_000_000);
        assert_eq!(d.per_channel(3), 20_000_000);
        assert_eq!(d.per_channel(0), 0, "没有通道就没有每通道深度，不许除零");
        let full = ScopeDepth {
            max_points: 100_000_000,
            points: None,
        };
        assert_eq!(full.points(), 100_000_000, "省略 = 用满");
    }

    #[test]
    fn 重采样的时钟下触发源必须进环_native_下不必() {
        // auto / fixed：环里只有选中的列，外触发无处可跑
        let mut c = multi();
        c.trigger.as_mut().unwrap().source = "loop:iw".parse().unwrap();
        let e = c.validate().unwrap_err();
        assert!(
            e.contains("trigger.source") && e.contains("把它选进 channels"),
            "{e}"
        );

        // native：源列在写环时读整行，外触发照旧
        let mut c = setup();
        c.clock.mode = ClockMode::Native;
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

    /// 读不了的 setup 不许让整份配置解析失败（否则**整个节点起不来**），也不许被
    /// 悄悄丢掉或改写——原样留着 + 出错的原话。
    #[test]
    fn 读不了的_setup_不拖垮解析_而且原样留着() {
        // 老写法：depth_bytes 已经没了，depth 是必填
        let old = r#"
kind = "port"
target = "loop"
[scope]
channels = [{ column = "iu" }]
depth_bytes = 268435456
refresh_hz = 30
timebase = { span_s = 0.002 }
[scope.trigger]
source = { column = "z" }
level = 0.5
"#;
        let b: WidgetBinding = toml::from_str(old).expect("整份绑定照样读得进来——节点要起得来");
        assert_eq!(b.target, "loop");
        assert!(b.scope_config().is_none(), "坏的绝不能被当成配置用");
        assert!(b.scope_is_broken());
        let e = b.scope_error().expect("要有出错的原话");
        assert!(e.contains("depth"), "原话要指到真正缺的那个字段：{e}");

        // 写回去一字不改：宽容读不许变成有损写
        let back = toml::to_string(&b).unwrap();
        assert!(
            back.contains("depth_bytes = 268435456"),
            "原来的键值要留着：{back}"
        );
        assert!(!back.contains("\ndepth ="), "不许凭空补一个 depth：{back}");
        assert!(back.contains("level = 0.5"), "整段都要留着：{back}");
        // 再读一遍还是同一份坏的（往返稳定，不会越写越走样）
        let again: WidgetBinding = toml::from_str(&back).unwrap();
        assert_eq!(again.scope, b.scope);

        // JSON 那条路一样
        let j = serde_json::to_string(&b).unwrap();
        assert!(j.contains("\"depth_bytes\":268435456"), "{j}");
        let jb: WidgetBinding = serde_json::from_str(&j).unwrap();
        assert!(jb.scope_is_broken());
    }

    #[test]
    fn 能读的_setup_照旧是配置_不受宽容影响() {
        let b: WidgetBinding = toml::from_str(&format!(
            "kind = \"port\"\ntarget = \"loop\"\n[scope]\n{}",
            SETUP
                .replace("[trigger]", "[scope.trigger]")
                .replace("[acq]", "[scope.acq]")
                .replace("[[vertical]]", "[[scope.vertical]]")
                .replace("[measure]", "[scope.measure]")
        ))
        .unwrap();
        let c = b.scope_config().expect("能读");
        assert!(c.validate().is_ok());
        assert_eq!(
            b.scope,
            Some(ScopeSetup::from(c.clone())),
            "Ok 分支就是配置本身"
        );
        // 序列化出来跟直接序列化配置一模一样（没有多包一层）
        let a = serde_json::to_value(b.scope.as_ref().unwrap()).unwrap();
        assert_eq!(a, serde_json::to_value(c).unwrap());
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
        let sc = b.scope_config().expect("scope section");
        assert_eq!(sc.channels.len(), 4);
        assert!(b.scope_error().is_none() && !b.scope_is_broken());
        assert!(b.tap.is_none());
        let s = serde_json::to_string(&b).unwrap();
        assert!(s.contains("\"scope\"") && !s.contains("\"tap\""), "{s}");
    }
}
