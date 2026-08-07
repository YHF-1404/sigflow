//! Subprocess (Process runtime) entry point for Rust plugins.
//!
//! A Rust plugin can run **in-process** as a native cdylib (via
//! [`crate::export_plugin!`]) or **out-of-process** as a standalone binary
//! (via [`crate::process_main!`]). Both implement the same [`crate::Plugin`]
//! trait; this module provides the subprocess harness: it mmaps the shell's
//! shared-memory data buffers and runs the stdin/stdout JSON-line control
//! loop, mirroring the Python SDK's `_runtime.py` + `_shm.py`.
//!
//! Protocol (control messages, one JSON object per line):
//!   plugin → shell: `ready` (with manifest), `initialized`, `processed`,
//!                   `started`, `stopped`
//!   shell → plugin: `init`, `set_param`, `start`, `process`, `stop`, `shutdown`
//!
//! Bulk sample data travels through SHM; per-frame [`FrameHeader`]s travel in
//! the `process` / `processed` control messages.

use std::io::{BufRead, Write};

use memmap2::MmapMut;
use serde_json::{json, Value};

use crate::{Frame, FrameHeader, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome};

const SHM_HEADER_SIZE: usize = 64;
const PORT_ENTRY_SIZE: usize = 24;

/// Maps the shell-created shared-memory file and exposes per-port buffers.
/// Layout matches `sigflow_shell::process_runtime::SharedMemory`.
struct Shm {
    mmap: MmapMut,
    num_inputs: usize,
    num_outputs: usize,
}

impl Shm {
    fn open(path: &str) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        let num_inputs = u16::from_le_bytes(mmap[8..10].try_into().unwrap()) as usize;
        let num_outputs = u16::from_le_bytes(mmap[10..12].try_into().unwrap()) as usize;
        Ok(Shm {
            mmap,
            num_inputs,
            num_outputs,
        })
    }

    /// (buffer_offset, capacity, frame_size) for a port-table index.
    fn port_entry(&self, port_index: usize) -> (usize, usize, usize) {
        let base = SHM_HEADER_SIZE + port_index * PORT_ENTRY_SIZE;
        let off = u64::from_le_bytes(self.mmap[base..base + 8].try_into().unwrap()) as usize;
        let cap = u64::from_le_bytes(self.mmap[base + 8..base + 16].try_into().unwrap()) as usize;
        let fs = u64::from_le_bytes(self.mmap[base + 16..base + 24].try_into().unwrap()) as usize;
        (off, cap, fs)
    }

    fn set_frame_size(&mut self, port_index: usize, n: usize) {
        let base = SHM_HEADER_SIZE + port_index * PORT_ENTRY_SIZE + 16;
        self.mmap[base..base + 8].copy_from_slice(&(n as u64).to_le_bytes());
    }

    /// Copy an input buffer's valid bytes (length = frame_size) into a Vec.
    fn read_input(&self, input_index: usize) -> Vec<u8> {
        let (off, _cap, fs) = self.port_entry(input_index);
        self.mmap[off..off + fs].to_vec()
    }

    /// Write `data` into output buffer `out_i` and record its frame_size.
    fn write_output(&mut self, out_i: usize, data: &[u8]) {
        let port_index = self.num_inputs + out_i;
        let (off, cap, _) = self.port_entry(port_index);
        let len = data.len().min(cap);
        self.mmap[off..off + len].copy_from_slice(&data[..len]);
        self.set_frame_size(port_index, len);
    }

    fn output_capacity(&self, out_i: usize) -> usize {
        let (_off, cap, _fs) = self.port_entry(self.num_inputs + out_i);
        cap
    }
}

fn send(tx: &mut impl Write, msg: &Value) {
    let line = serde_json::to_string(msg).expect("serialize control message");
    let _ = tx.write_all(line.as_bytes());
    let _ = tx.write_all(b"\n");
    let _ = tx.flush();
}

/// Read one JSON-line control message. `None` on EOF (shell closed stdin).
fn read_line(rx: &mut impl BufRead) -> Option<Value> {
    let mut line = String::new();
    match rx.read_line(&mut line) {
        Ok(0) => None, // EOF
        Ok(_) => serde_json::from_str(&line).ok(),
        Err(_) => None,
    }
}

fn health_json(outcome: &ProcessOutcome) -> Value {
    serde_json::to_value(outcome).unwrap_or_else(|_| json!({ "status": "ok" }))
}

/// Run the subprocess control loop for plugin type `P`. `manifest_toml` is the
/// plugin's `manifest.toml` contents (typically via `include_str!`).
///
/// This blocks until the shell sends `shutdown` or closes stdin, then returns.
pub fn run<P: Plugin>(manifest_toml: &str) {
    let shm_path = std::env::args()
        .find_map(|a| a.strip_prefix("--shm-path=").map(str::to_string))
        .expect("plugin requires --shm-path=<file> argument");

    let manifest: PluginManifest =
        toml::from_str(manifest_toml).expect("embedded manifest.toml must parse");

    let mut shm = Shm::open(&shm_path).expect("failed to mmap SHM file");

    let stdin = std::io::stdin();
    let mut rx = stdin.lock();
    let stdout = std::io::stdout();
    let mut tx = stdout.lock();

    // Handshake: announce readiness + manifest.
    send(
        &mut tx,
        &json!({
            "method": "ready",
            "params": { "protocol": "sigflow-plugin-v1", "manifest": manifest },
        }),
    );

    // Expect `init`, apply initial params.
    let mut plugin = P::new(&manifest);
    if let Some(init) = read_line(&mut rx) {
        if let Some(params) = init
            .get("params")
            .and_then(|p| p.get("params"))
            .and_then(|v| v.as_object())
        {
            for (id, value) in params {
                if let Ok(pv) = serde_json::from_value::<ParamValue>(value.clone()) {
                    plugin.set_param(id, &pv);
                }
            }
        }
    }
    send(&mut tx, &json!({ "method": "initialized" }));

    // Main control loop.
    while let Some(msg) = read_line(&mut rx) {
        match msg.get("method").and_then(|v| v.as_str()).unwrap_or("") {
            "set_param" => {
                let params = msg.get("params");
                let id = params
                    .and_then(|p| p.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(value) = params.and_then(|p| p.get("value")) {
                    if let Ok(pv) = serde_json::from_value::<ParamValue>(value.clone()) {
                        plugin.set_param(id, &pv);
                    }
                }
            }
            "start" => {
                let outcome = plugin.start();
                send(
                    &mut tx,
                    &json!({ "method": "started", "params": { "health": health_json(&outcome) } }),
                );
            }
            "stop" => {
                plugin.stop();
                send(&mut tx, &json!({ "method": "stopped" }));
            }
            "process" => {
                // Copy input bytes out of SHM first (avoids aliasing the mmap
                // when we later write outputs).
                let in_data: Vec<Vec<u8>> = (0..shm.num_inputs).map(|i| shm.read_input(i)).collect();
                let in_headers: Vec<FrameHeader> = msg
                    .get("params")
                    .and_then(|p| p.get("in_headers"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|h| serde_json::from_value(h.clone()).unwrap_or_default())
                            .collect()
                    })
                    .unwrap_or_default();

                // Shell-measured ingress latency per input frame (parallel to
                // in_headers); absent → 0.
                let in_latencies: Vec<i64> = msg
                    .get("params")
                    .and_then(|p| p.get("in_latencies"))
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(|v| v.as_i64().unwrap_or(0)).collect())
                    .unwrap_or_default();

                let in_frames: Vec<Frame> = in_data
                    .iter()
                    .enumerate()
                    .map(|(i, d)| {
                        let mut f =
                            Frame::new(in_headers.get(i).copied().unwrap_or_default(), d.as_slice());
                        f.latency_ns = in_latencies.get(i).copied().unwrap_or(0);
                        f
                    })
                    .collect();

                // Output staging buffers sized to each port's capacity.
                let mut out_bufs: Vec<Vec<u8>> =
                    (0..shm.num_outputs).map(|i| vec![0u8; shm.output_capacity(i)]).collect();
                let outcome;
                let mut out_headers: Vec<FrameHeader> = Vec::with_capacity(shm.num_outputs);
                let mut out_written: Vec<usize> = Vec::with_capacity(shm.num_outputs);
                {
                    let mut out_frames: Vec<FrameOut> =
                        out_bufs.iter_mut().map(|b| FrameOut::new(b.as_mut_slice())).collect();
                    outcome = plugin.process(&in_frames, &mut out_frames);
                    for fo in &out_frames {
                        out_headers.push(fo.header);
                        out_written.push(fo.written());
                    }
                } // out_frames dropped here, releasing the borrow of out_bufs

                // Write each output's valid bytes to SHM.
                for i in 0..shm.num_outputs {
                    let buf = &out_bufs[i];
                    let n = out_written[i].min(buf.len());
                    let bytes = buf[..n].to_vec();
                    shm.write_output(i, &bytes);
                }

                send(
                    &mut tx,
                    &json!({
                        "method": "processed",
                        "params": {
                            "out_headers": out_headers,
                            "health": health_json(&outcome),
                        }
                    }),
                );
            }
            "shutdown" => {
                plugin.shutdown();
                send(&mut tx, &json!({ "method": "shutdown_complete" }));
                break;
            }
            _ => {}
        }
    }
}
