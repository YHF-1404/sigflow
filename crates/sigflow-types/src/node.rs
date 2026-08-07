use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::manifest::Direction;
use crate::param::ParamValue;
use crate::semantic::BackpressurePolicy;
use crate::ui::UiWidget;

/// Complete node.toml Rust representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct NodeConfig {
    pub meta: NodeMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub plugin: Option<PluginRef>,
    #[serde(default)]
    pub params: HashMap<String, ParamValue>,
    /// Parameters that were present in a previously loaded plugin but are not
    /// in the currently loaded one. Kept around so that switching back to the
    /// old plugin (or otherwise re-introducing the parameter id) restores the
    /// last-known value rather than the descriptor default.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub archived_params: HashMap<String, ParamValue>,
    /// UI widget instances. Old node.toml files used a `ui = []` key for the
    /// (now-removed) UIBinding schema — serde simply ignores unknown fields,
    /// so legacy files continue to load with an empty `widgets`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widgets: Vec<UiWidget>,
    /// Child node IDs (container nodes only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    /// Data connections between child nodes (stored in parent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connections: Vec<Connection>,
    /// Control connections: port output -> parameter input (stored in parent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control_connections: Vec<ControlConnection>,
    /// Ports exposed on the parent boundary (container nodes only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_ports: Vec<ParentPortDecl>,
    /// Graph-editor canvas layout: child node id -> position. A container-level
    /// concern stored alongside `connections` in the parent DB, so the visual
    /// arrangement travels with export/import and cross-machine redeploy
    /// (the DB stays the single source of truth — see [[architecture-decisions]]).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub node_layout: HashMap<String, NodePosition>,
    /// Graph-editor viewport (pan + zoom) for this container's canvas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub viewport: Option<Viewport>,
}

/// A node's position on the graph-editor canvas, plus how the parent renders
/// it. `w`/`h` and `display` only matter for `display = "face"` nodes (the
/// widget-panel face is resizable); plain cards ignore them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct NodePosition {
    pub x: f64,
    pub y: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub display: Option<NodeDisplay>,
}

/// Display density of a node on its parent's canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum NodeDisplay {
    /// Title + edge handles only.
    Collapsed,
    /// The regular card (default).
    Card,
    /// Card header + embedded widget panel.
    Face,
}

/// The graph-editor canvas viewport (pan offset + zoom level).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct Viewport {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

/// Node identity and bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct NodeMeta {
    pub id: String,
    pub name: String,
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub modified: Option<String>,
}

fn default_format_version() -> u32 {
    1
}

/// Reference to a plugin stored in node DB (triple: name, version, content_hash).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct PluginRef {
    pub name: String,
    pub version: String,
    pub content_hash: String,
}

/// Data connection between two child node ports (stored in parent DB).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct Connection {
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    pub to_port: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub backpressure: Option<BackpressurePolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub queue_depth: Option<u32>,
}

/// Control connection: a port output drives a parameter on another node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ControlConnection {
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    /// Target parameter id (addressed with @ in CLI).
    pub to_param: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub queue_depth: Option<u32>,
}

/// A port exposed on a container node's boundary, bound to a child port.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ParentPortDecl {
    pub name: String,
    pub direction: Direction,
    /// Child node id that owns the bound port.
    pub bind_node: String,
    /// Port id on the child node.
    pub bind_port: String,
}
