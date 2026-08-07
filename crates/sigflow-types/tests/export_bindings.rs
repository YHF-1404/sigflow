//! Single source of truth for the frontend's TypeScript types.
//!
//! The Rust types in `sigflow-types` are the authority; this test regenerates
//! their TS equivalents via ts-rs so the web frontend never hand-mirrors a Rust
//! enum again (which previously drifted silently — see `web/widgets/slider.js`).
//!
//! Run from the monorepo root (regenerates `web/generated/*.ts`):
//!     SIGFLOW_TS_OUT="$PWD/web/generated" cargo test --manifest-path public/Cargo.toml -p sigflow-types --features ts export_bindings
//!
//! The output is committed; a type change shows up as a reviewable `git diff`
//! under `web/generated/`. Only compiled under the `ts` feature, so a normal
//! `cargo test` is a no-op here.
#![cfg(feature = "ts")]

use std::path::PathBuf;

use ts_rs::TS;

use sigflow_types::frame::FrameHeader;
use sigflow_types::manifest::{
    ActionDescriptor, BindableTarget, ConfigField, Direction, ParameterDescriptor, PluginManifest,
    PluginSource, PortDescriptor, WidgetManifest,
};
use sigflow_types::node::{
    Connection, ControlConnection, NodeConfig, NodeDisplay, NodeMeta, NodePosition, ParentPortDecl,
    PluginRef, Viewport,
};
use sigflow_types::param::{ControlScalar, ParamRange, ParamType, ParamValue};
use sigflow_types::plugin::{
    NativeRuntimeConfig, PluginCategory, PluginId, ProcessRuntimeConfig, RuntimeConfig, RuntimeType,
};
use sigflow_types::rpc::{
    ConnectRequest, ControlConnectRequest, DisconnectRequest, GetGraphResponse, GetParamRequest,
    GetParamResponse, GraphNode, InvokeActionRequest, ListNodesRequest, ListPluginsResponse,
    NodeStateEvent, ParamChangedEvent, PluginInfo, RpcError, RpcMessage, RpcNotification,
    RpcRequest, RpcResponse, SetNodeLayoutRequest, SetParamRequest, SubscribeRequest,
};
use sigflow_types::semantic::{BackpressurePolicy, BatchSpec, PayloadSchemaId, SemanticType};
use sigflow_types::ui::{BindKind, Layout, TapConfig, UiWidget, WidgetBinding};

/// Export each listed type plus its transitive dependencies into `dir`.
macro_rules! export_all {
    ($dir:expr; $($t:ty),+ $(,)?) => {{
        $(
            <$t as TS>::export_all_to($dir)
                .unwrap_or_else(|e| panic!("export {}: {e}", stringify!($t)));
        )+
    }};
}

#[test]
fn export_bindings() {
    // Output dir: $SIGFLOW_TS_OUT when set (the monorepo drift gate
    // tools/check-ts-bindings.sh points it at web/generated/), else
    // bindings/ under the public workspace root.
    let dir: PathBuf = match std::env::var_os("SIGFLOW_TS_OUT") {
        Some(d) => PathBuf::from(d),
        None => [env!("CARGO_MANIFEST_DIR"), "..", "..", "bindings"]
            .iter()
            .collect(),
    };

    // Regenerate from scratch so a renamed/removed type can't leave a stale file.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create bindings output dir");

    export_all!(&dir;
        // param
        ParamValue, ParamRange, ParamType, ControlScalar,
        // semantic / type system
        SemanticType, PayloadSchemaId, BatchSpec, BackpressurePolicy,
        // ui widgets
        UiWidget, WidgetBinding, BindKind, Layout, TapConfig,
        // rpc envelope + method payloads
        RpcMessage, RpcRequest, RpcResponse, RpcError, RpcNotification,
        SetParamRequest, GetParamRequest, GetParamResponse, InvokeActionRequest,
        SubscribeRequest, ConnectRequest, DisconnectRequest, ControlConnectRequest,
        ListNodesRequest, NodeStateEvent, ParamChangedEvent,
        // graph editor: snapshot, layout persistence, plugin catalog
        GraphNode, GetGraphResponse, SetNodeLayoutRequest, PluginInfo, ListPluginsResponse,
        // node db (graph)
        NodeConfig, NodeMeta, PluginRef, Connection, ControlConnection, ParentPortDecl,
        NodePosition, NodeDisplay, Viewport,
        // plugin manifest
        PluginManifest, WidgetManifest, BindableTarget, ConfigField, PluginSource,
        Direction, PortDescriptor, ParameterDescriptor, ActionDescriptor,
        // plugin identity / runtime
        PluginId, PluginCategory, RuntimeType, RuntimeConfig, NativeRuntimeConfig, ProcessRuntimeConfig,
        // frame header (data-plane metadata; borrowed Frame<'a> is intentionally not exported)
        FrameHeader,
    );

    eprintln!("ts-rs bindings written to {}", dir.display());
}
