//! TCP source plugin: connects to a TCP server and emits the raw byte stream.
//!
//! This is a **source** node (no input ports) and is deliberately
//! format-agnostic — it knows nothing about the application protocol on the
//! wire. Each `process()` does a non-blocking drain of whatever bytes have
//! arrived and emits them as one Opaque byte frame, stamped with host arrival
//! time. A downstream parser (e.g. `touch_tracker`) reassembles records.
//!
//! The byte stream is modeled as an `i8` sample stream: `n_samples` = bytes in
//! this frame, `sample_index` = cumulative byte count since connect. That keeps
//! the self-anchored-frame contract (a transport drop would show as a
//! `sample_index` jump) even though the data is opaque.
//!
//! Connection lifecycle: **lazy connect + auto-reconnect**. `start()` never
//! fails on connectivity — a network source must tolerate the server being
//! offline at deploy time, single-client (busy with another reader), or
//! dropping mid-run. The connection is established (and re-established) inside
//! `process()`, rate-limited, so the node comes up regardless and streams as
//! soon as the server is reachable. Connection errors are logged, never faulted
//! — faulting would abort the whole deploy / take down the rest of the graph.

use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

/// Per-attempt connect timeout. Short so a down server stalls only this node's
/// tick briefly (each node runs its own processing loop), not for seconds.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
/// Minimum spacing between connect attempts while disconnected (ns).
const RECONNECT_INTERVAL_NS: i64 = 1_000_000_000; // 1 s

pub struct TcpSource {
    host: String,
    port: u16,
    stream: Option<TcpStream>,
    seq: u64,
    /// Cumulative bytes since stream start — the i8 sample axis (kept monotonic
    /// across reconnects).
    byte_index: u64,
    /// Reusable read buffer, grown to the output capacity on first use.
    scratch: Vec<u8>,
    /// Monotonic ns of the last connect attempt (rate-limits reconnects).
    last_connect_attempt_ns: i64,
    /// Whether we were connected, to log connect/disconnect transitions once.
    was_connected: bool,
}

impl TcpSource {
    fn connect(&mut self) -> Result<TcpStream, String> {
        let addr = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| format!("resolve {}:{}: {e}", self.host, self.port))?
            .next()
            .ok_or_else(|| format!("no address for {}:{}", self.host, self.port))?;
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| format!("connect {addr}: {e}"))?;
        stream
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking: {e}"))?;
        Ok(stream)
    }
}

impl Plugin for TcpSource {
    fn new(_manifest: &PluginManifest) -> Self {
        TcpSource {
            host: "127.0.0.1".to_string(),
            port: 9000,
            stream: None,
            seq: 0,
            byte_index: 0,
            scratch: Vec::new(),
            last_connect_attempt_ns: 0,
            was_connected: false,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match id {
            "host" => {
                if let ParamValue::String(v) = value {
                    self.host = v.clone();
                }
            }
            "port" => {
                if let ParamValue::U32(v) = value {
                    self.port = (*v).clamp(1, 65535) as u16;
                }
            }
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        // Never fault on connectivity: come up regardless, connect lazily in
        // process(). last_connect_attempt_ns = 0 → first tick attempts at once.
        self.seq = 0;
        self.byte_index = 0;
        self.stream = None;
        self.last_connect_attempt_ns = 0;
        self.was_connected = false;
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        let Some(out) = outputs.first_mut() else {
            return ProcessOutcome::Ok;
        };

        // Ensure connected (lazy, rate-limited). Connection problems are logged,
        // never faulted — see the module docs.
        if self.stream.is_none() {
            let now = mono_ns();
            if now - self.last_connect_attempt_ns >= RECONNECT_INTERVAL_NS {
                self.last_connect_attempt_ns = now;
                match self.connect() {
                    Ok(s) => {
                        eprintln!("tcp_source: connected to {}:{}", self.host, self.port);
                        self.stream = Some(s);
                        self.was_connected = true;
                    }
                    Err(e) => {
                        // Log only on the connected→disconnected transition (and
                        // the very first attempt) to avoid once-a-second spam.
                        if self.was_connected || self.seq == 0 {
                            eprintln!(
                                "tcp_source: connect {}:{} failed: {e} (will retry)",
                                self.host, self.port
                            );
                        }
                        self.was_connected = false;
                    }
                }
            }
        }
        if self.stream.is_none() {
            return ProcessOutcome::Ok; // not connected yet — retry next tick
        }

        // Read up to the output buffer capacity in one non-blocking shot. The
        // disjoint-field borrow lets us drop `self.stream` in the error arms.
        let cap = out.capacity();
        if cap == 0 {
            return ProcessOutcome::Ok;
        }
        if self.scratch.len() < cap {
            self.scratch.resize(cap, 0);
        }
        let read_result = self.stream.as_mut().unwrap().read(&mut self.scratch[..cap]);
        match read_result {
            Ok(0) => {
                eprintln!("tcp_source: peer closed connection; will reconnect");
                self.stream = None;
                self.was_connected = false;
                ProcessOutcome::Ok
            }
            Ok(n) => {
                out.write(&self.scratch[..n]);
                out.header.seq = self.seq;
                out.header.sample_index = self.byte_index;
                out.header.n_samples = n as u32;
                out.header.t0_ns = mono_ns();
                self.seq += 1;
                self.byte_index += n as u64;
                ProcessOutcome::Ok
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No data this tick — emit nothing, do not advance the stream.
                ProcessOutcome::Ok
            }
            Err(e) => {
                eprintln!("tcp_source: read error: {e}; will reconnect");
                self.stream = None;
                self.was_connected = false;
                ProcessOutcome::Ok
            }
        }
    }

    fn stop(&mut self) {
        self.stream = None;
    }

    fn shutdown(&mut self) {
        self.stream = None;
    }
}

sigflow_plugin_sdk::export_plugin!(TcpSource);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    #[test]
    fn manifest_declares_one_producer_port() {
        let m = manifest();
        assert_eq!(m.ports.len(), 1);
        assert_eq!(m.ports[0].id, "bytes");
    }

    #[test]
    fn connects_and_streams_bytes_with_byte_axis() {
        // Spin a throwaway server that sends a known blob, then assert the
        // plugin emits it with a monotonic byte axis.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(&[1u8, 2, 3, 4, 5]).unwrap();
            sock.write_all(&[6u8, 7]).unwrap();
            // Hold the socket open briefly so the client can drain.
            std::thread::sleep(Duration::from_millis(100));
        });

        let mut p = TcpSource::new(&manifest());
        p.set_param("host", &ParamValue::String("127.0.0.1".into()));
        p.set_param("port", &ParamValue::U32(addr.port() as u32));
        assert!(matches!(p.start(), ProcessOutcome::Ok));

        // Drain a few ticks; collect everything emitted.
        let mut got: Vec<u8> = Vec::new();
        let mut last_index = 0u64;
        for _ in 0..50 {
            let mut buf = vec![0u8; 256];
            let mut outs = [FrameOut::new(&mut buf)];
            let _ = p.process(&[], &mut outs);
            let w = outs[0].written();
            if w > 0 {
                assert_eq!(outs[0].header.sample_index, last_index);
                assert_eq!(outs[0].header.n_samples as usize, w);
                last_index += w as u64;
                got.extend_from_slice(&outs[0].buffer_mut()[..w]);
            }
            if got.len() >= 7 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        p.stop();
        server.join().unwrap();
        assert_eq!(got, vec![1, 2, 3, 4, 5, 6, 7]);
    }
}
