use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::param::{ParamRange, ParamType, ParamValue};
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
    /// Widget-specific declarations. Only meaningful when
    /// `category == PluginCategory::UiWidget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub widget: Option<WidgetManifest>,
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
