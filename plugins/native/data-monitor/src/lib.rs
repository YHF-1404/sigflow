//! Data monitor plugin: a sink node that consumes f32 sample data, tracks
//! statistics (min, max, mean, sample/frame counts), and optionally streams the
//! samples to [VOFA+](https://www.vofa.plus/) over UDP for live waveform view.
//!
//! VOFA+ uses the **JustFloat** wire format: for each sample row, `channels`
//! little-endian f32 followed by the 4-byte frame tail `00 00 80 7f`. The input
//! data is already channel-interleaved per sample, so rows map straight through.
//! Channel count is taken from the `channels` param, or auto-derived from the
//! frame header (`total_f32 / n_samples`) when `channels == 0`.
//!
//! This is a sink node (no output ports). Contract lives in `manifest.toml`.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

use sigflow_plugin_sdk::{Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome};

/// VOFA+ JustFloat frame tail: the bytes of `+inf` little-endian, used as the
/// per-row delimiter VOFA+ scans for.
const VOFA_TAIL: [u8; 4] = [0x00, 0x00, 0x80, 0x7f];

/// Max UDP payload per datagram; split on row boundaries to stay under a typical
/// 1500-byte Ethernet MTU and avoid IP fragmentation.
const MAX_DGRAM: usize = 1400;

pub struct DataMonitor {
    enabled: bool,
    sample_count: u64,
    min: f32,
    max: f32,
    sum: f64,
    frame_count: u64,

    // --- VOFA+ UDP streaming ---
    vofa_enabled: bool,
    vofa_addr: String,
    /// Channel count for JustFloat rows; 0 = auto-derive from the frame header.
    channels: u32,
    sock: Option<UdpSocket>,
    target: Option<SocketAddr>,
    /// Reusable JustFloat encode buffer.
    vofa_buf: Vec<u8>,
    /// Set after the first bind/resolve/send error so we log it only once.
    warned: bool,
}

/// Encode channel-interleaved f32 `data` into VOFA+ JustFloat rows into `out`:
/// for each of `rows = (data.len()/4)/channels` samples, `channels` little-endian
/// f32 then the 4-byte tail. `out` is cleared first.
fn justfloat_encode(data: &[u8], channels: usize, out: &mut Vec<u8>) {
    out.clear();
    let channels = channels.max(1);
    let row_bytes = channels * 4;
    let rows = (data.len() / 4) / channels;
    out.reserve(rows * (row_bytes + 4));
    for r in 0..rows {
        let off = r * row_bytes;
        out.extend_from_slice(&data[off..off + row_bytes]);
        out.extend_from_slice(&VOFA_TAIL);
    }
}

impl DataMonitor {
    /// Lazily bind the UDP socket and resolve the target address. Returns false
    /// (logging once) if either fails, so streaming silently no-ops until fixed.
    fn ensure_vofa(&mut self) -> bool {
        if self.sock.is_none() {
            match UdpSocket::bind("0.0.0.0:0") {
                Ok(s) => {
                    // Non-blocking: a full socket buffer drops samples rather
                    // than stalling the shell's processing tick.
                    let _ = s.set_nonblocking(true);
                    self.sock = Some(s);
                }
                Err(e) => {
                    if !self.warned {
                        eprintln!("data_monitor: VOFA UDP bind failed: {e}");
                        self.warned = true;
                    }
                    return false;
                }
            }
        }
        if self.target.is_none() {
            match self.vofa_addr.to_socket_addrs().map(|mut it| it.next()) {
                Ok(Some(a)) => self.target = Some(a),
                _ => {
                    if !self.warned {
                        eprintln!(
                            "data_monitor: VOFA target '{}' did not resolve (want host:port)",
                            self.vofa_addr
                        );
                        self.warned = true;
                    }
                    return false;
                }
            }
        }
        true
    }

    /// Encode the frame as JustFloat and send it to VOFA+ in MTU-sized datagrams.
    fn send_vofa(&mut self, input: &Frame) {
        // 只看样本载荷——带尾随注解的帧（FLAG_ANNOTATED）直接用 input.data
        // 会把注解字节当样本编码，且 total/n_samples 除不尽会错判单通道。
        let data = input.samples();
        let total_f32 = data.len() / 4;
        if total_f32 == 0 {
            return;
        }
        // Channel count: explicit param, else derive from the per-channel sample
        // count in the header; fall back to mono if it does not divide evenly.
        let channels = if self.channels > 0 {
            self.channels as usize
        } else {
            let ns = input.header.n_samples as usize;
            if ns > 0 && total_f32 % ns == 0 {
                total_f32 / ns
            } else {
                1
            }
        }
        .max(1);
        if total_f32 < channels {
            return;
        }
        if !self.ensure_vofa() {
            return;
        }

        justfloat_encode(data, channels, &mut self.vofa_buf);

        let row_stride = channels * 4 + 4;
        let chunk = (MAX_DGRAM / row_stride).max(1) * row_stride;
        let target = self.target.unwrap();
        let sock = self.sock.as_ref().unwrap();
        let mut err = None;
        let mut pos = 0;
        while pos < self.vofa_buf.len() {
            let end = (pos + chunk).min(self.vofa_buf.len());
            match sock.send_to(&self.vofa_buf[pos..end], target) {
                Ok(_) => pos = end,
                // Socket buffer momentarily full: drop this tick, not an error.
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = err {
            if !self.warned {
                eprintln!("data_monitor: VOFA UDP send to {target} failed: {e}");
                self.warned = true;
            }
        }
    }
}

impl Plugin for DataMonitor {
    fn new(_manifest: &PluginManifest) -> Self {
        DataMonitor {
            enabled: true,
            sample_count: 0,
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            sum: 0.0,
            frame_count: 0,
            vofa_enabled: false,
            vofa_addr: "127.0.0.1:1347".to_string(),
            channels: 0,
            sock: None,
            target: None,
            vofa_buf: Vec::new(),
            warned: false,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("enabled", ParamValue::Bool(v)) => self.enabled = *v,
            ("vofa_enabled", ParamValue::Bool(v)) => self.vofa_enabled = *v,
            ("vofa_addr", ParamValue::String(s)) => {
                self.vofa_addr = s.clone();
                self.target = None; // re-resolve on next send
                self.warned = false;
            }
            ("channels", ParamValue::U32(v)) => self.channels = *v,
            _ => {}
        }
    }

    fn process(&mut self, inputs: &[Frame], _outputs: &mut [FrameOut]) -> ProcessOutcome {
        if !self.enabled {
            return ProcessOutcome::Ok;
        }
        if let Some(input) = inputs.first() {
            let data = input.samples(); // 统计同样只算样本载荷，不吃注解字节
            let num_samples = data.len() / 4;
            for i in 0..num_samples {
                let offset = i * 4;
                let sample = f32::from_le_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]);
                self.sample_count += 1;
                self.sum += sample as f64;
                if sample < self.min {
                    self.min = sample;
                }
                if sample > self.max {
                    self.max = sample;
                }
            }
            self.frame_count += 1;

            if self.vofa_enabled {
                self.send_vofa(input);
            }
        }
        ProcessOutcome::Ok
    }

    fn invoke_action(&mut self, id: &str) {
        if id == "reset_stats" {
            self.sample_count = 0;
            self.min = f32::INFINITY;
            self.max = f32::NEG_INFINITY;
            self.sum = 0.0;
            self.frame_count = 0;
        }
    }
}

sigflow_plugin_sdk::export_plugin!(DataMonitor);

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_manifest() -> PluginManifest {
        let toml = include_str!("../manifest.toml");
        toml::from_str(toml).expect("manifest.toml should parse")
    }

    /// Feed one input frame (no outputs — this is a sink).
    fn feed(mon: &mut DataMonitor, input: &[u8]) {
        use sigflow_plugin_sdk::FrameHeader;
        let inputs = [Frame::new(FrameHeader::ZERO, input)];
        mon.process(&inputs, &mut []);
    }

    #[test]
    fn manifest_toml_parses_with_expected_shape() {
        let m = dummy_manifest();
        assert_eq!(m.name, "sigflow.core.data_monitor");
        assert_eq!(m.ports.len(), 1);
        assert_eq!(m.ports[0].id, "signal_in");
        // enabled + vofa_enabled + vofa_addr + channels
        assert_eq!(m.parameters.len(), 4);
        let ids: Vec<&str> = m.parameters.iter().map(|p| p.id.as_str()).collect();
        for want in ["enabled", "vofa_enabled", "vofa_addr", "channels"] {
            assert!(ids.contains(&want), "missing param {want}");
        }
        assert_eq!(m.actions.len(), 1);
        assert_eq!(m.actions[0].id, "reset_stats");
    }

    #[test]
    fn process_updates_stats_correctly() {
        let mut mon = DataMonitor::new(&dummy_manifest());

        let samples: Vec<f32> = vec![-0.5, 0.0, 0.25, 1.0];
        let mut input_bytes = Vec::new();
        for s in &samples {
            input_bytes.extend_from_slice(&s.to_le_bytes());
        }

        feed(&mut mon, &input_bytes);

        assert_eq!(mon.sample_count, 4);
        assert_eq!(mon.frame_count, 1);
        assert_eq!(mon.min, -0.5);
        assert_eq!(mon.max, 1.0);
        let expected_mean = (-0.5 + 0.0 + 0.25 + 1.0) / 4.0;
        let actual_mean = mon.sum / mon.sample_count as f64;
        assert!(
            (actual_mean - expected_mean).abs() < 1e-10,
            "mean should be {expected_mean}, got {actual_mean}"
        );
    }

    #[test]
    fn process_accumulates_across_frames() {
        let mut mon = DataMonitor::new(&dummy_manifest());

        let frame1 = 0.5_f32.to_le_bytes();
        feed(&mut mon, &frame1);

        let frame2 = (-0.3_f32).to_le_bytes();
        feed(&mut mon, &frame2);

        assert_eq!(mon.sample_count, 2);
        assert_eq!(mon.frame_count, 2);
        assert_eq!(mon.min, -0.3);
        assert_eq!(mon.max, 0.5);
    }

    #[test]
    fn disabled_monitor_skips_processing() {
        let mut mon = DataMonitor::new(&dummy_manifest());
        mon.set_param("enabled", &ParamValue::Bool(false));

        let sample = 1.0_f32.to_le_bytes();
        feed(&mut mon, &sample);

        assert_eq!(mon.sample_count, 0);
        assert_eq!(mon.frame_count, 0);
    }

    #[test]
    fn reset_stats_clears_all_counters() {
        let mut mon = DataMonitor::new(&dummy_manifest());

        let sample = 0.42_f32.to_le_bytes();
        feed(&mut mon, &sample);
        assert_eq!(mon.sample_count, 1);

        mon.invoke_action("reset_stats");

        assert_eq!(mon.sample_count, 0);
        assert_eq!(mon.frame_count, 0);
        assert_eq!(mon.min, f32::INFINITY);
        assert_eq!(mon.max, f32::NEG_INFINITY);
        assert_eq!(mon.sum, 0.0);
    }

    #[test]
    fn empty_input_does_not_crash() {
        let mut mon = DataMonitor::new(&dummy_manifest());

        feed(&mut mon, &[]);

        assert_eq!(mon.frame_count, 1);
        assert_eq!(mon.sample_count, 0);

        // Zero input frames: nothing happens.
        mon.process(&[], &mut []);

        assert_eq!(mon.frame_count, 1);
    }

    #[test]
    fn justfloat_encode_two_channels() {
        // Two rows of 2 channels: [1,2],[3,4].
        let mut data = Vec::new();
        for v in [1.0f32, 2.0, 3.0, 4.0] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = Vec::new();
        justfloat_encode(&data, 2, &mut out);
        // 2 rows × (2×4 + 4 tail) = 24 bytes.
        assert_eq!(out.len(), 24);
        // Row 0: 1.0, 2.0, tail.
        assert_eq!(&out[0..4], &1.0f32.to_le_bytes());
        assert_eq!(&out[4..8], &2.0f32.to_le_bytes());
        assert_eq!(&out[8..12], &VOFA_TAIL);
        // Row 1: 3.0, 4.0, tail.
        assert_eq!(&out[12..16], &3.0f32.to_le_bytes());
        assert_eq!(&out[16..20], &4.0f32.to_le_bytes());
        assert_eq!(&out[20..24], &VOFA_TAIL);
    }

    #[test]
    fn justfloat_encode_drops_partial_row() {
        // 3 floats with channels=2 → 1 full row, trailing float dropped.
        let mut data = Vec::new();
        for v in [1.0f32, 2.0, 3.0] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = Vec::new();
        justfloat_encode(&data, 2, &mut out);
        assert_eq!(out.len(), 12, "one row only (2 ch + tail)");
    }

    #[test]
    fn vofa_default_off_no_socket() {
        let mon = DataMonitor::new(&dummy_manifest());
        assert!(!mon.vofa_enabled);
        assert!(mon.sock.is_none());
    }

    #[test]
    fn vofa_addr_change_resets_target() {
        let mut mon = DataMonitor::new(&dummy_manifest());
        mon.target = Some("127.0.0.1:9999".parse().unwrap());
        mon.set_param("vofa_addr", &ParamValue::String("127.0.0.1:1347".into()));
        assert_eq!(mon.vofa_addr, "127.0.0.1:1347");
        assert!(mon.target.is_none(), "addr change must force re-resolve");
    }
}
