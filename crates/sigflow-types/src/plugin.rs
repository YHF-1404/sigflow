use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

/// Triple identifier used to reference a plugin from node DB.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct PluginId {
    pub name: String,
    pub version: String,
    pub content_hash: String,
}

/// Whether the plugin provides compute logic or a UI widget.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum PluginCategory {
    Compute,
    UiWidget,
}

/// How the shell loads and communicates with the plugin.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum RuntimeType {
    Native,
    Process,
}

/// Runtime configuration (tagged by type).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum RuntimeConfig {
    Native(NativeRuntimeConfig),
    Process(ProcessRuntimeConfig),
}

/// Config for native (C ABI dlopen) plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct NativeRuntimeConfig {
    /// Shared library filename, e.g. "libpassthrough.so"
    pub library: String,
    /// C ABI entry symbol name
    pub entry: String,
}

/// Config for subprocess-based plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ProcessRuntimeConfig {
    /// Executable command, e.g. "python3"
    pub command: String,
    /// Script or module path
    pub script: String,
    /// IPC protocol version
    #[serde(default = "default_protocol")]
    pub protocol: String,
}

fn default_protocol() -> String {
    "v1".to_string()
}
