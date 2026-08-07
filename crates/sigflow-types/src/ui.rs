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
    /// Param id, port id, action id, or empty string for `State`.
    pub target: String,
    /// Only meaningful when `kind == Port` — tap subscription config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub tap: Option<TapConfig>,
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
}

impl BindKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BindKind::Param => "param",
            BindKind::Port => "port",
            BindKind::State => "state",
            BindKind::Action => "action",
            BindKind::Logs => "logs",
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

/// Configuration for a data-stream tap (shared per port; configs from
/// multiple widgets on the same port are merged at runtime).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct TapConfig {
    /// `> 0`: streaming mode — ring of the latest N samples, emitted at
    /// `refresh_hz`. `0`: frame mode — every produced frame is forwarded
    /// verbatim (event-shaped ports: heatmaps, estimates).
    pub window_samples: u32,
    pub refresh_hz: f32,
    /// Built-in statistics to compute: `rms`, `peak`, `min`, `max`, `mean`.
    #[serde(default)]
    pub stats: Vec<String>,
    /// Explicit mode override. `None` keeps the `window_samples` heuristic.
    /// `stream` forwards every *new* sample batched at `refresh_hz`, tagged
    /// with sample-axis metadata (index/channels/discontinuity) so a client
    /// can accumulate a scrollable, gap-aware history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub mode: Option<TapMode>,
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
