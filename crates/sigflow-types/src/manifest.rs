use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::param::{DisplayHint, ParamGuard, ParamRange, ParamType, ParamValue};
use crate::plugin::{PluginCategory, RuntimeConfig};
use crate::semantic::{BackpressurePolicy, BatchSpec, PayloadSchemaId, SemanticType};
use crate::ui::BindKind;

/// Complete plugin manifest declared by the plugin author.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub category: PluginCategory,
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub ports: Vec<PortDescriptor>,
    #[serde(default)]
    pub parameters: Vec<ParameterDescriptor>,
    #[serde(default)]
    pub actions: Vec<ActionDescriptor>,
    /// 源语义节点即使带辅助消费口也每 tick 驱动（默认 false = 无输入口才
    /// 当源）。例：touch_source 的 dst_result 回传口可不连，不设此标志
    /// 会被输入门控饿死（process 永不运行）。
    #[serde(default)]
    pub tick_driven: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub min_shell_version: Option<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Optional source metadata. Reserved for future remote-mirror support;
    /// MVP does not consume this but the schema is forward-compatible.
    #[serde(default, skip_serializing_if = "PluginSource::is_empty")]
    pub source: PluginSource,
    /// Configuration documents this plugin consumes. Each declares a
    /// [`crate::schema::DocSchema`] file shipped inside the plugin package
    /// and the parameter holding the document's path, which together let a
    /// generic editor render a plugin-specific configuration without knowing
    /// anything about the domain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<DocumentDecl>,
    /// Widget-specific declarations. Only meaningful when
    /// `category == PluginCategory::UiWidget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub widget: Option<WidgetManifest>,
}

/// A configuration document a plugin reads.
///
/// ```toml
/// [[documents]]
/// id = "params"
/// schema_file = "motor-params.schema.toml"
/// path_param = "config_path"
/// label = "电机参数"
/// ```
///
/// The schema is a separate file rather than an inline table because a real
/// machine schema runs to hundreds of lines; expressed as nested TOML
/// arrays-of-tables inside `manifest.toml` it is unreadable and unreviewable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct DocumentDecl {
    /// Unique within the plugin. A widget binds to a document by this id.
    pub id: String,
    /// Schema file, relative to the plugin package root.
    pub schema_file: String,
    /// Id of the plugin parameter holding the document's file path. Empty
    /// value there = the plugin has no document yet, which is a legal state:
    /// a node can be created before its configuration exists.
    pub path_param: String,
    /// Sidecar the plugin writes after acting on the document — what it sent
    /// to a device, what the device said back, and any read-back values.
    /// Relative to the document's directory. Absent = no status channel.
    ///
    /// A file rather than a call into the plugin, for two reasons. A process
    /// plugin is a separate process with no symbol to call, so an ABI entry
    /// point would serve only half the plugins. And the answer to "what is
    /// this machine actually running" is worth being able to read on a
    /// headless device with `cat`, after the editor has gone home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub status_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
}

/// Declarations specific to UI widget plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct WidgetManifest {
    /// Short human-readable type (e.g. `slider`, `knob`, `fft`).
    pub widget_type: String,
    /// What this widget can be bound to. Empty = bind nothing (display-only).
    #[serde(default)]
    pub bindable_to: Vec<BindableTarget>,
    /// Schema for the widget instance's `config` map.
    #[serde(default)]
    pub config_schema: HashMap<String, ConfigField>,
    /// How the host renders this widget. Defaults to [`Presentation::Face`],
    /// which is every widget that existed before this field.
    #[serde(default)]
    pub presentation: Presentation,
}

/// Where a widget is rendered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum Presentation {
    /// On the node's face on the graph canvas, sized by its `Layout`. The
    /// default and the right answer for a knob, a readout, a chart.
    #[default]
    Face,
    /// A full page of its own, opened from an entry on the node's face.
    ///
    /// Some surfaces do not shrink: a tree of four axes beside a form of
    /// twenty fields beside a comparison table needs a page, and squeezing it
    /// onto a canvas card produces something nobody will use for the job it
    /// exists to do.
    Page,
}

/// A binding target this widget accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct BindableTarget {
    pub kind: BindKind,
    /// Constrains the param_type (for `Param`) or semantic_type.kind (for
    /// `Port`). `None` = any. Examples: `"f64"`, `"audio.pcm"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub data_type: Option<String>,
}

/// A field in the widget's instance `config` schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ConfigField {
    /// `"string"`, `"f64"`, `"u32"`, `"i32"`, `"bool"`, `"enum"`.
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub default: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enum_values: Vec<String>,
}

/// Source metadata reserved for future remote-mirror lookup.
///
/// All fields are optional; an empty `PluginSource` is treated as if absent.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct PluginSource {
    /// Optional remote mirror base URL. Not consumed in MVP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub mirror: Option<String>,
}

impl PluginSource {
    /// Returns true if no source fields are populated.
    pub fn is_empty(&self) -> bool {
        self.mirror.is_none()
    }
}

/// Port direction from the plugin's perspective.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum Direction {
    Producer,
    Consumer,
}

/// Declares a data port on a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct PortDescriptor {
    pub id: String,
    pub direction: Direction,
    pub semantic_type: SemanticType,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub payload_schema_id: Option<PayloadSchemaId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub declared_rate_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub batch_spec: Option<BatchSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "number"))]
    pub max_latency_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub backpressure: Option<BackpressurePolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub max_subscribers: Option<u32>,
    /// 列契约：交织口里每一列是什么、怎么读（见 [`ColumnDecl`]）。空 = 没
    /// 声明，消费方只能按下标猜、按曲线画。
    ///
    /// 空也照样序列化成 `[]`（不 skip）：前端类型里它是必填项，让 JSON
    /// 和类型说一样的话；只有认不得这个字段的老 shell 会整个不发——前端
    /// 读的时候仍按「可能没有」兜底。
    #[serde(default)]
    pub columns: Vec<ColumnDecl>,
    /// `columns` 描述的是**一组**；组按从站/通道重复时在这里说（见
    /// [`ColumnGroups`]）。None = 只有一组，总列数 = `columns.len()`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub column_groups: Option<ColumnGroups>,
}

/// 列契约：一个多列交织口里，某一列是什么、怎么读。
///
/// 没有它，列只活在 manifest 注释和页面脚本的 `labels=` 串里。画曲线够用
/// ——曲线不在乎一列是什么——状态控件不够：状态字是枚举不是量，把它画
/// 成曲线会在两个状态之间画一条穿过不存在状态的斜线；跟随误差要画在窗
/// 旁边才有意义，而窗是多少只有生产数据的那一方知道。所以**谁生产数据，
/// 谁声明它怎么读**；控件只认 `kind`，不认识 CiA402。
///
/// 约定：**NaN = 本拍不适用**（闭环没跑时的 d/q 电流、老固件没有的列）。
/// 一个「不适用」的零和一个「确实为零」的零长得一模一样；NaN 让控件置灰、
/// 曲线断开，而不是画一个谁也分不清的 0。
///
/// ```toml
/// [[ports]]
/// id = "status"
/// direction = "producer"
/// semantic_type = { kind = "timeseries.signal", dtype = "f32" }
/// column_groups = { repeat_by = "slaves", label = "s{i}" }
///
/// [[ports.columns]]
/// id = "sw"
/// label = "状态字"
/// kind = "enum"
/// decode = [
///   { mask = 0x6f, value = 0x27, name = "Operation enabled", tone = "good" },
///   { mask = 0x6f, value = 0x23, name = "Switched on" },
///   { mask = 0x4f, value = 0x08, name = "Fault", tone = "crit" },
/// ]
/// bits = [ { bit = 11, name = "限幅", tone = "warn" }, { bit = 3, name = "故障", tone = "crit" } ]
///
/// [[ports.columns]]
/// id = "ferr"
/// label = "跟随误差"
/// unit = "counts"
/// kind = "bounded"
/// bound = { max = { column = "ferr_lim" }, bipolar = true }
///
/// [[ports.columns]]
/// id = "dropouts"
/// kind = "counter"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ColumnDecl {
    /// 组内唯一；控件配置里按它选列（`column = "ferr"`），不按下标。
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub unit: Option<String>,
    /// 默认读法——决定控件推断（曲线 / 徽章 / 灯 / 计数卡 / 量表）。
    #[serde(default)]
    pub kind: ColumnKind,
    /// 枚举解码：按声明顺序逐条试 `(round(v) & mask) == value`，先中先赢；
    /// 一条都不中显示原值。`kind = enum` 的主读法；别的 kind 也可以带。
    #[serde(default)]
    pub decode: Vec<EnumCase>,
    /// 位名：`kind = bitfield` 的主读法；一个状态字可以同时带 `decode`
    /// （状态机位）和 `bits`（标志位），两种控件各取所需。
    #[serde(default)]
    pub bits: Vec<BitDecl>,
    /// 界：`kind = bounded` 时量表把它画在值旁边。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub bound: Option<Bound>,
}

/// 一列的默认读法。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum ColumnKind {
    /// 连续量：画曲线。
    #[default]
    Signal,
    /// 枚举码：值本身没有大小意义，只有「是哪一个」。
    Enum,
    /// 位域：每一位独立成立。
    Bitfield,
    /// 单调计数：有意义的是增量和速率，不是绝对值。
    Counter,
    /// 有界连续量：离界多远比值本身重要。
    Bounded,
}

/// 语义色。与控件自己的强调色无关——good/warn/crit 说的是**事**的状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum Tone {
    #[default]
    Neutral,
    Good,
    Warn,
    Crit,
    /// 灰：关着、不在、不适用。
    Off,
}

/// 枚举解码的一条：`(round(v) & mask) == value`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct EnumCase {
    /// 缺省 = 全 1（整值相等）。CiA402 状态机位那种「看低 7 位」用 0x6f；
    /// `mask = 0` 匹配一切——放在最后当兜底（「其它」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub mask: Option<u32>,
    pub value: u32,
    pub name: String,
    #[serde(default)]
    pub tone: Tone,
}

/// 位域里的一位。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct BitDecl {
    pub bit: u8,
    pub name: String,
    /// 该位为 1 时的语义色（为 0 时一律灰）。
    #[serde(default)]
    pub tone: Tone,
}

/// 有界量的界。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct Bound {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub max: Option<BoundRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub min: Option<BoundRef>,
    /// 双极：界对 |v| 生效，零居中画；`min` 缺省取 `-max`。
    #[serde(default)]
    pub bipolar: bool,
}

/// 界从哪来：一个常数，或同组里另一列——生效的界只有生产方知道（驱动器
/// 里现在是多少、换工况之后是多少），让它每拍发布出来，控件就永远画的是
/// 真值，而不是页面脚本里某次手填的 4096。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum BoundRef {
    Value(f64),
    Column { column: String },
}

/// 列组重复：`columns` 描述一组，整组按从站/通道重复，列 = 组序 × 组内序
/// （组序在外、列序在内，与 cia402-csx 遥测口一致）。
///
/// 组数不在这里声明——运行时由帧的通道数除以组内列数得出，帧永远是对的，
/// 参数值只是它的来历。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ColumnGroups {
    /// 组数来自哪个参数（如 `slaves`）。只为说明来历，消费方不靠它算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub repeat_by: Option<String>,
    /// 组名模板，`{i}` = 组序号（0 起）。缺省 `"{i}"`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    /// 身份/模式跟进组标题：这些列（组内 id）的当前值渲染在组名后面。
    /// 为「轴身份一眼可见」而生——别名列跟上标题，接错线/刻错别名当场露馅；
    /// 开环轴打「开环」徽章，「怎么没有跟随误差条」从疑惑变成自解释。
    ///
    /// 渲染规则（全声明驱动，控件零语义）：值 NaN 不显（离线/按位置回落只剩
    /// 组名）；有 `decode` 的列命中且 name 非空才按 name+tone 显成徽章，
    /// **不命中不显**——所以模式列只声明非缺省态（`{ value = 1, name = "开环" }`），
    /// 缺省态干干净净；无 `decode` 的列显 `{label}{值}`（`label = "别名"` →
    /// 「s2 · 别名3」）。
    ///
    /// ```toml
    /// column_groups = { repeat_by = "slaves", label = "s{i}", title_columns = ["alias", "drive_mode"] }
    /// ```
    ///
    /// 不写 = 空。序列化永远带 `[]`（与 `columns` 同例——前端类型必填，
    /// JSON 得和类型说一样的话）。
    #[serde(default)]
    pub title_columns: Vec<String>,
}

/// Declares a tunable parameter on a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ParameterDescriptor {
    pub id: String,
    pub param_type: ParamType,
    #[serde(default = "default_param_range")]
    pub range: ParamRange,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub default: Option<ParamValue>,
    /// Whether the parameter can be changed without pause-resume.
    #[serde(default = "default_true")]
    pub hot_change: bool,
    /// Whether the parameter may be written from outside the plugin (UI / control
    /// plane). `false` marks it read-only / device-determined: the shell rejects
    /// external writes and the UI should render it disabled. Defaults to `true`.
    #[serde(default = "default_true")]
    pub editable: bool,
    /// Human-readable reason shown when the parameter is not editable (e.g. why a
    /// value is device-determined). Only meaningful when `editable` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub editable_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    /// Heading this field belongs under, when its owner declares
    /// [`crate::schema::SectionDecl`]s. Unknown / absent = before the first
    /// heading.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub section: Option<String>,
    /// Why this value is what it is — the bench result, the failure it
    /// prevents, the neighbouring parameter it is coupled to.
    ///
    /// This exists because that knowledge currently lives in TOML comments
    /// that only the author reads. "1200 (crossover ~200Hz) oscillates on the
    /// bench, 600 is stable" is worth more at the moment of editing than any
    /// amount of form polish, and it costs one string to carry.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub note: Option<String>,
    /// The narrower band that has actually been proven, inside the legal
    /// `range`. Leaving it produces a warning, never a rejection — the point
    /// is to tell an operator they have left known-good territory, not to
    /// stop them from going there deliberately.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub recommended: Option<ParamRange>,
    /// Display-unit conversion; see [`DisplayHint`].
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub display: Option<DisplayHint>,
    /// Extra friction before this field can be changed; see [`ParamGuard`].
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub guard: Option<ParamGuard>,
    /// The value is read back from the device rather than stored in the
    /// document — a derived coefficient, a checksum, a state word. Renders as
    /// a read-only row and is skipped by everything that writes.
    ///
    /// Distinct from `editable = false`, which still means "a value the
    /// document owns, that this surface may not write".
    #[serde(default)]
    pub readback: bool,
    /// Conditional relevance: the UI only offers this parameter while **every**
    /// listed condition holds against the node's current parameter values.
    /// Absent / empty = always relevant (the default, and what every existing
    /// manifest means). This is a presentation hint only — the shell still
    /// accepts writes to a currently-hidden parameter, so a mode switch never
    /// strands a value.
    ///
    /// Declared next to the parameter, e.g. for a plugin whose `command_source`
    /// picks between a sine generator and waypoint streaming:
    ///
    /// ```toml
    /// [[parameters]]
    /// id = "wp_vmax"
    /// param_type = "f64"
    /// visible_when = [{ param = "command_source", equals = ["waypoint"] }]
    /// ```
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub visible_when: Option<Vec<VisibleWhen>>,
}

impl Default for ParameterDescriptor {
    /// A neutral descriptor whose values match what serde fills in for a
    /// manifest that omits the field — so `ParameterDescriptor { id, ..
    /// Default::default() }` and the equivalent TOML describe the same knob.
    ///
    /// Exists so that adding an optional declaration (as `visible_when`,
    /// `note` and `guard` each were) does not break every constructor in
    /// every plugin.
    fn default() -> Self {
        ParameterDescriptor {
            id: String::new(),
            param_type: ParamType::F64,
            range: ParamRange::None,
            default: None,
            hot_change: true,
            editable: true,
            editable_reason: None,
            unit: None,
            label: None,
            section: None,
            note: None,
            recommended: None,
            display: None,
            guard: None,
            readback: false,
            visible_when: None,
        }
    }
}

/// One condition of a parameter's [`ParameterDescriptor::visible_when`]: the
/// parameter is relevant while `param`'s current value is one of `equals`.
///
/// Values are compared as strings (the scalar rendered without its type tag),
/// so an enum variant, `"true"` / `"false"`, and `"2"` all work. A condition
/// naming a parameter the node does not have is ignored rather than hiding the
/// control — a typo must not make a knob unreachable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct VisibleWhen {
    /// Id of the parameter this one depends on (typically a mode selector).
    pub param: String,
    /// Values of `param` that make this parameter relevant.
    pub equals: Vec<String>,
}

fn default_param_range() -> ParamRange {
    ParamRange::None
}

fn default_true() -> bool {
    true
}

/// Declares a triggerable action on a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ActionDescriptor {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
}
