//! Rust SDK for writing native sigflow plugins.
//!
//! The plugin's contract (ports, parameters, actions) lives in a sibling
//! `manifest.toml` file that ships next to the compiled dylib. Plugin code
//! does **not** declare its own manifest — the shell loads `manifest.toml`
//! on install/load and hands it to the plugin via the FFI `create` call.
//! This eliminates the historic "Rust code vs TOML" double-source problem
//! and keeps the runtime path symmetric with the Python (Process) SDK.
//!
//! # Quick start
//!
//! ```ignore
//! use sigflow_plugin_sdk::{Plugin, PluginManifest, ParamValue, Frame, FrameOut, ProcessOutcome, export_plugin};
//!
//! struct MyPlugin;
//! impl Plugin for MyPlugin {
//!     fn new(_manifest: &PluginManifest) -> Self { MyPlugin }
//!     fn set_param(&mut self, _id: &str, _v: &ParamValue) {}
//!     fn process(&mut self, _i: &[Frame], _o: &mut [FrameOut]) -> ProcessOutcome {
//!         ProcessOutcome::Ok
//!     }
//! }
//! export_plugin!(MyPlugin);
//! ```

pub mod process;

pub use sigflow_types::frame::{
    decode_capture_window_annotation, decode_rate_annotation, encode_capture_window_annotation,
    encode_rate_annotation, parse_annotation, Frame, FrameHeader, FrameOut, ProcessOutcome,
    CAPTURE_WINDOW_ANNOTATION_SCHEMA, FLAG_ANNOTATED, FLAG_DISCONTINUITY, FLAG_TRIG_INDEXED,
    RATE_ANNOTATION_SCHEMA,
};
pub use sigflow_types::manifest::{
    ActionDescriptor, Direction, ParameterDescriptor, PluginManifest, PortDescriptor,
};
pub use sigflow_types::param::ParamValue;
pub use sigflow_types::plugin::{NativeRuntimeConfig, PluginCategory, RuntimeConfig};
pub use sigflow_types::PLUGIN_ABI_VERSION;
pub use sigflow_types::time::mono_ns;

// Re-export serde_json so that the macro expansion can reference it
// without requiring plugin crates to add serde_json as a direct dependency.
#[doc(hidden)]
pub use serde_json;

/// Trait that native plugin authors implement.
///
/// Note: there is no `manifest()` method anymore — the manifest is the
/// `manifest.toml` file on disk, and the shell hands a parsed copy to
/// `new` so the plugin can introspect its own contract if needed (e.g.
/// pre-allocate buffers sized to declared ports).
pub trait Plugin: Send {
    /// Construct a new plugin instance. The manifest argument is the one
    /// the shell parsed from `manifest.toml` — same content the user
    /// installed. Most plugins ignore it.
    fn new(manifest: &PluginManifest) -> Self
    where
        Self: Sized;

    /// Set a parameter value.
    fn set_param(&mut self, id: &str, value: &ParamValue);

    /// Called when the node enters the Running state, before the first
    /// `process()`. Sources acquire/start hardware here (e.g. `pipe_start`).
    /// Return [`ProcessOutcome::Fault`] to refuse starting.
    fn start(&mut self) -> ProcessOutcome {
        ProcessOutcome::Ok
    }

    /// Process one batch: read input [`Frame`]s, write output [`FrameOut`]s.
    /// Each output's `header` and `written` length are read back by the shell.
    fn process(&mut self, inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome;

    /// Called when the node leaves the Running state. Sources stop hardware
    /// here (e.g. `pipe_stop`).
    fn stop(&mut self) {}

    /// Handle a triggered action.
    fn invoke_action(&mut self, _id: &str) {}

    /// Clean up before unloading.
    fn shutdown(&mut self) {}
}

/// Generates C ABI entry points for a [`Plugin`] implementation.
///
/// The following symbols are exported:
///
/// | Symbol | Purpose |
/// |---|---|
/// | `sigflow_plugin_abi_version` | ABI handshake — returns [`PLUGIN_ABI_VERSION`] |
/// | `sigflow_plugin_create` | Allocate a new instance, given the manifest JSON the shell parsed from `manifest.toml` |
/// | `sigflow_plugin_set_param` | Set a parameter (id + JSON value) |
/// | `sigflow_plugin_process` | Run the processing function |
/// | `sigflow_plugin_invoke_action` | Trigger a named action |
/// | `sigflow_plugin_destroy` | Shutdown + deallocate the instance |
///
/// `sigflow_plugin_describe` is **no longer exported** — the manifest is the
/// `manifest.toml` file, not anything embedded in the plugin binary.
///
/// # Usage
///
/// ```ignore
/// sigflow_plugin_sdk::export_plugin!(MyPluginStruct);
/// ```
#[macro_export]
macro_rules! export_plugin {
    ($plugin_type:ty) => {
        /// ABI handshake: the shell compares this against its own
        /// `PLUGIN_ABI_VERSION` before touching any other symbol.
        #[no_mangle]
        pub extern "C" fn sigflow_plugin_abi_version() -> u32 {
            $crate::PLUGIN_ABI_VERSION
        }

        /// Allocate a plugin instance. `manifest_json` is the manifest the
        /// shell loaded from `manifest.toml`; the macro deserializes it
        /// before calling `Plugin::new`. Returns null on invalid JSON.
        #[no_mangle]
        pub extern "C" fn sigflow_plugin_create(
            manifest_json: *const ::std::ffi::c_char,
        ) -> *mut ::std::ffi::c_void {
            if manifest_json.is_null() {
                return ::std::ptr::null_mut();
            }
            let json = unsafe { ::std::ffi::CStr::from_ptr(manifest_json) }
                .to_str()
                .unwrap_or("");
            let manifest: $crate::PluginManifest =
                match $crate::serde_json::from_str(json) {
                    Ok(m) => m,
                    Err(_) => return ::std::ptr::null_mut(),
                };
            let plugin = Box::new(<$plugin_type as $crate::Plugin>::new(&manifest));
            Box::into_raw(plugin) as *mut ::std::ffi::c_void
        }

        #[no_mangle]
        pub extern "C" fn sigflow_plugin_set_param(
            instance: *mut ::std::ffi::c_void,
            id: *const ::std::ffi::c_char,
            value_json: *const ::std::ffi::c_char,
        ) {
            unsafe {
                let plugin = &mut *(instance as *mut $plugin_type);
                let id = ::std::ffi::CStr::from_ptr(id).to_str().unwrap_or("");
                let json = ::std::ffi::CStr::from_ptr(value_json).to_str().unwrap_or("");
                if let Ok(value) =
                    $crate::serde_json::from_str::<$crate::ParamValue>(json)
                {
                    plugin.set_param(id, &value);
                }
            }
        }

        /// Run the processing function. Inputs carry a [`FrameHeader`] plus a
        /// data slice; outputs expose a writable buffer whose `header` and
        /// written byte-length the plugin fills and the shell reads back.
        /// Returns 0 on `ProcessOutcome::Ok`, 1 otherwise.
        ///
        /// # Safety
        /// All pointer arrays must be valid for `num_inputs` / `num_outputs`
        /// elements; `in_headers`/`out_headers`/`out_written` point at arrays
        /// of the corresponding length.
        #[no_mangle]
        pub extern "C" fn sigflow_plugin_process(
            instance: *mut ::std::ffi::c_void,
            in_headers: *const $crate::FrameHeader,
            inputs: *const *const u8,
            input_lens: *const usize,
            num_inputs: usize,
            out_headers: *mut $crate::FrameHeader,
            outputs: *const *mut u8,
            out_caps: *const usize,
            out_written: *mut usize,
            num_outputs: usize,
        ) -> i32 {
            unsafe {
                let plugin = &mut *(instance as *mut $plugin_type);
                let in_frames: ::std::vec::Vec<$crate::Frame> = (0..num_inputs)
                    .map(|i| {
                        $crate::Frame::new(
                            *in_headers.add(i),
                            ::std::slice::from_raw_parts(*inputs.add(i), *input_lens.add(i)),
                        )
                    })
                    .collect();
                let mut out_frames: ::std::vec::Vec<$crate::FrameOut> = (0..num_outputs)
                    .map(|i| {
                        let mut fo = $crate::FrameOut::new(::std::slice::from_raw_parts_mut(
                            *outputs.add(i),
                            *out_caps.add(i),
                        ));
                        fo.header = *out_headers.add(i);
                        fo
                    })
                    .collect();
                let outcome =
                    <$plugin_type as $crate::Plugin>::process(plugin, &in_frames, &mut out_frames);
                for (i, fo) in out_frames.iter().enumerate() {
                    *out_headers.add(i) = fo.header;
                    *out_written.add(i) = fo.written();
                }
                match outcome {
                    $crate::ProcessOutcome::Ok => 0,
                    _ => 1,
                }
            }
        }

        /// Enter the Running state (sources start hardware). Returns 0 on Ok.
        #[no_mangle]
        pub extern "C" fn sigflow_plugin_start(instance: *mut ::std::ffi::c_void) -> i32 {
            unsafe {
                let plugin = &mut *(instance as *mut $plugin_type);
                match <$plugin_type as $crate::Plugin>::start(plugin) {
                    $crate::ProcessOutcome::Ok => 0,
                    _ => 1,
                }
            }
        }

        /// Leave the Running state (sources stop hardware).
        #[no_mangle]
        pub extern "C" fn sigflow_plugin_stop(instance: *mut ::std::ffi::c_void) {
            unsafe {
                let plugin = &mut *(instance as *mut $plugin_type);
                <$plugin_type as $crate::Plugin>::stop(plugin);
            }
        }

        #[no_mangle]
        pub extern "C" fn sigflow_plugin_invoke_action(
            instance: *mut ::std::ffi::c_void,
            id: *const ::std::ffi::c_char,
        ) {
            unsafe {
                let plugin = &mut *(instance as *mut $plugin_type);
                let id = ::std::ffi::CStr::from_ptr(id).to_str().unwrap_or("");
                plugin.invoke_action(id);
            }
        }

        #[no_mangle]
        pub extern "C" fn sigflow_plugin_destroy(instance: *mut ::std::ffi::c_void) {
            if !instance.is_null() {
                unsafe {
                    let mut plugin = Box::from_raw(instance as *mut $plugin_type);
                    plugin.shutdown();
                    // Box drop handles deallocation
                }
            }
        }
    };
}

/// Generates a `main()` that runs a [`Plugin`] as a **subprocess** plugin
/// (Process runtime), the out-of-process counterpart to [`export_plugin!`].
///
/// Build the crate as a `bin` and the shell spawns it with `--shm-path=<file>`.
/// `$manifest_toml` is the plugin's `manifest.toml` contents, typically passed
/// via `include_str!("../manifest.toml")`.
///
/// ```ignore
/// sigflow_plugin_sdk::process_main!(MyPlugin, include_str!("../manifest.toml"));
/// ```
#[macro_export]
macro_rules! process_main {
    ($plugin_type:ty, $manifest_toml:expr) => {
        fn main() {
            $crate::process::run::<$plugin_type>($manifest_toml);
        }
    };
}
