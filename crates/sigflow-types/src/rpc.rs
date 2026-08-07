use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::manifest::{ParameterDescriptor, PluginManifest, PortDescriptor};
use crate::node::{Connection, ControlConnection, NodePosition, ParentPortDecl, PluginRef, Viewport};
use crate::param::ParamValue;
use crate::semantic::BackpressurePolicy;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Core envelope types (JSON-RPC 2.0 style)
// ---------------------------------------------------------------------------

/// Top-level RPC message (request, response, or notification).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum RpcMessage {
    Request(RpcRequest),
    Response(RpcResponse),
    Notification(RpcNotification),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct RpcRequest {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct RpcResponse {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// Server-initiated event push (no id).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct RpcNotification {
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Method-specific request/response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct SetParamRequest {
    pub node: String,
    pub param: String,
    pub value: ParamValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct GetParamRequest {
    pub node: String,
    pub param: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct GetParamResponse {
    pub node: String,
    pub param: String,
    pub value: ParamValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct InvokeActionRequest {
    pub node: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct SubscribeRequest {
    pub node: String,
    pub events: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ConnectRequest {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct DisconnectRequest {
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    pub to_port: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ControlConnectRequest {
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    pub to_param: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub queue_depth: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ListNodesRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct NodeStateEvent {
    pub node: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ParamChangedEvent {
    pub node: String,
    pub param: String,
    pub value: ParamValue,
}

// ---------------------------------------------------------------------------
// Graph editor: aggregated snapshot, layout persistence, plugin catalog
// ---------------------------------------------------------------------------

/// One node in a container's graph snapshot, with everything the editor needs
/// to render the card and its connectable handles in a single round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct GraphNode {
    /// Child node id (unique within the container).
    pub id: String,
    /// Human-readable name (from the child's `node.toml` meta).
    pub name: String,
    /// Whether the child process is currently alive.
    pub running: bool,
    /// Coarse lifecycle hint for the status dot: "running" | "stopped".
    /// Precise per-node state is available via `get_status` on demand.
    pub state: String,
    /// Plugin assigned to this node, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub plugin: Option<PluginRef>,
    /// True if this child is itself a container (has children or parent ports).
    /// Such nodes are drilled into rather than wired at this level (Phase 3).
    pub is_container: bool,
    /// Connectable ports, resolved from the assigned plugin's manifest in the
    /// catalog (available even when the node is not running). Empty for
    /// container children and nodes without a plugin.
    pub ports: Vec<PortDescriptor>,
    /// Parameters targetable by control connections (port -> param), resolved
    /// from the plugin manifest. Empty for container children / no plugin.
    pub parameters: Vec<ParameterDescriptor>,
    /// For a container child: the boundary ports it exposes (its own
    /// `parent_ports`). These are the handles the parent level can wire to;
    /// empty for leaf nodes. Lets a container be connected without drilling in.
    pub parent_ports: Vec<ParentPortDecl>,
    /// Canvas position, if the editor has previously placed this node.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub position: Option<NodePosition>,
    /// RPC port of this child's own shell, when running. The editor connects
    /// directly here for the tap data plane (high-rate binary frames that must
    /// not be bubbled through the parent). `None` when the node is not running.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub rpc_port: Option<u16>,
    /// Widget instances on this node's face (from its own `node.toml`), so the
    /// parent canvas can render the face without extra routed `widget_list`
    /// round trips. Orphan status is not included — the face editor asks
    /// `widget_list` when it needs diagnostics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widgets: Vec<crate::ui::UiWidget>,
}

/// Aggregated snapshot of one container's editable graph level — the editor's
/// first-screen payload (replaces `list_children` + N×`describe` +
/// `list_connections` + `list_control_connections` + `list_parent_ports`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct GetGraphResponse {
    /// The container being viewed.
    pub node_id: String,
    pub node_name: String,
    /// Child nodes rendered on the canvas.
    pub nodes: Vec<GraphNode>,
    /// Data-plane edges (port -> port).
    pub connections: Vec<Connection>,
    /// Control-plane edges (port -> parameter).
    pub control_connections: Vec<ControlConnection>,
    /// This container's own boundary ports (for nesting / drill-up).
    pub parent_ports: Vec<ParentPortDecl>,
    /// Saved canvas viewport (pan + zoom).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub viewport: Option<Viewport>,
}

/// Persist editor canvas layout. `positions` is merged into the container's
/// stored layout (send only the nodes that moved); `viewport` replaces the
/// saved viewport when present.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct SetNodeLayoutRequest {
    #[serde(default)]
    pub positions: HashMap<String, NodePosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub viewport: Option<Viewport>,
}

/// One installed plugin available to assign to a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    /// Full sha256 content hash (the third leg of the plugin's triple id).
    pub hash: String,
}

/// Response for `list_plugins`: the installable compute plugins in the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ListPluginsResponse {
    pub plugins: Vec<PluginInfo>,
}
