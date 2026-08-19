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
