//! USB ISO ADC acquisition plugin (Process runtime, Rust + rusb).
//!
//! A source node: opens the USB ADC device, configures it, and streams
//! interleaved f32 voltage samples with self-anchored frame headers. See
//! `memory/usb_adc_plugin.md` for the full design.
//!
//! Module map:
//!   protocol — device identity, vendor requests, capability/config wire format
//!   convert  — raw u16 → f32 voltage, forward-fill for lost packets
//!   device   — rusb control plane (open/reset/capability/config/pipe)
//!   iso      — libusb async ISO data path (pull-gated reap/resubmit)
//!
//! The device + iso layers require real hardware to exercise; the pure
//! protocol/convert logic is unit-tested in their modules.

mod bulk;
mod clock;
mod convert;
mod device;
mod iso;
mod protocol;

use std::collections::VecDeque;

use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

use bulk::BulkStream;
use clock::ClockRecovery;
use device::Device;
use iso::IsoStream;
use protocol::{AdcConfig, BlockHeader, DeviceState, Transport, BLOCK_FLAG_NEW_SESSION};

/// ISO transfer pool sizing. With an HS device at 125 µs service intervals,
/// 8 packets ≈ 1 ms; 16 transfers ≈ 16 ms of in-flight buffering. ISO has no
/// retry, so if every URB is completed-but-not-yet-resubmitted the host
/// controller silently drops incoming microframes — a deep pool absorbs drain
/// jitter (scheduler hiccups, the periodic control-transfer health check).
/// Matches the Python reference's 16 transfers. (Deriving these from the
/// endpoint `bInterval` is a refinement.)
const PACKETS_PER_TRANSFER: usize = 8;
const NUM_TRANSFERS: usize = 16;

/// How often (in process() ticks) to poll device health via GET_CAPABILITY.
const HEALTH_CHECK_EVERY: u64 = 200;
/// How often (in process() ticks) to sample the device clock (GET_DEVICE_TIME)
/// for rate recovery. ~100 ms at the 1 ms shell tick → ~25 s over the window.
const CLOCK_OBS_EVERY: u64 = 100;
/// How often (in process() ticks) to drain the device pen-trigger FIFO. ~10 ms:
/// the events carry their own latched sample index, so poll latency only delays
/// *delivery* of the marker, not its position — a modest cadence keeps the
/// control-transfer overhead low (one ~300 µs round trip per ~10 ms ≈ 3%).
const TRIG_POLL_EVERY: u64 = 10;
/// Bulk transfer pool sizing: 16 transfers × 2 blocks (1 KB) ≈ 24 ms of
/// in-flight buffering at 64 kHz. Transfers carry no timeout and are never
/// canceled while streaming (see `bulk.rs` — canceling against a delivering
/// endpoint loses ACKed packets); each completes only when full (the device
/// pads every block to the 512 max-packet, so no short packets exist) and is
/// resubmitted inside its completion callback.
const BULK_TRANSFER_BYTES: usize = 2 * protocol::BLOCK_TOTAL;
const BULK_NUM_TRANSFERS: usize = 16;

/// Number of pens (PB4/PB5 → trigger_pen0/trigger_pen1).
const NUM_PENS: usize = 2;
/// Cap on per-pen queued events awaiting emission (one marker emits per tick per
/// port; this only matters if the reference pairing is not yet established).
const MAX_PENDING_TRIG: usize = 64;

struct UsbAdc {
    // Device parameters (sent to firmware via SET_ADC_CONFIG).
    sample_rate: u32,
    channel_mask: u16,
    samplebits: u8,
    // Host parameters.
    vref: f32,
    reset_on_open: bool,

    // Device/stream state (drop order: streams before device).
    iso: Option<IsoStream>,
    bulk: Option<BulkStream>,
    device: Option<Device>,

    // Self-anchored stream bookkeeping.
    seq: u64,
    sample_index: u64,
    ticks: u64,
    /// Per-channel sample rate the device *actually* runs at, read back from
    /// the capability at start(). The firmware ignores the configured rate in
    /// ISO mode (it free-runs at a fixed rate), so t0 must be derived from this,
    /// not from the host `sample_rate` parameter. 0 until start().
    effective_rate: u32,
    /// Stream-axis index of the sample `stream_t0_ns` anchors. ISO counts
    /// received samples from 0 (so this is 0); bulk carries the device's
    /// absolute counts in-band, so the first received block anchors at its
    /// `first_sample_count`.
    stream_first_index: u64,
    /// Bulk continuity: expected `first_sample_count` of the next block.
    bulk_expect_count: Option<u64>,
    /// Bulk continuity: expected `seq` of the next block.
    bulk_expect_seq: Option<u16>,
    /// Reusable staging for drained bulk bytes (whole 512 B blocks).
    bulk_scratch: Vec<u8>,
    /// Last observed resubmit-failure count (log-on-change).
    bulk_submit_failures_seen: u64,
    /// Absolute `CLOCK_MONOTONIC` ns anchoring sample 0 of the stream. Set on
    /// the *first* process() that drains real samples — not at pipe_start —
    /// because the device free-runs and only begins delivering ~0.5–0.75 s
    /// after the pipe opens; anchoring to the first arrival keeps t0 honest.
    /// `None` until then. Frame t0 = this + sample_index / effective_rate.
    stream_t0_ns: Option<i64>,
    /// Device-clock recovery: estimates the true host-ns-per-sample rate from
    /// periodic GET_DEVICE_TIME observations, replacing the nominal rate in t0
    /// so long runs do not accumulate the crystal-offset drift.
    clock: ClockRecovery,

    /// Reference pairing `(device_sample_count, host_sample_index)` used to map a
    /// pen trigger's *device* sample index onto this stream's *host* sample axis:
    /// `s_trig = host_si_ref + (c_trig − device_count_ref)`.
    ///
    /// Primary source: read in `start()` while the pipe is still stopped —
    /// counter frozen, zero in flight — pairing the count exactly with host
    /// sample 0 (the firmware's flow control pauses the ADC on backpressure,
    /// never drops, so the first received sample IS that count). Exact modulo
    /// mid-session ISO loss (which Bulk transport will eliminate).
    ///
    /// Fallback (pre-start read failed): paired lazily at the first clock
    /// observation, which bakes the instantaneous USB in-flight backlog
    /// (~ms) into every marker as a constant early bias.
    trig_ref: Option<(u64, u64)>,
    /// Per-pen queue of latched device sample indices awaiting marker emission
    /// (one marker per tick per port).
    pending_trig: [VecDeque<u64>; NUM_PENS],
    /// Per-pen monotonic marker sequence.
    trig_seq: [u64; NUM_PENS],

    // Reusable staging buffer for raw ISO bytes.
    raw: Vec<u8>,
}

impl UsbAdc {
    fn channels(&self) -> u32 {
        self.channel_mask.count_ones()
    }

    fn current_config(&self) -> AdcConfig {
        AdcConfig {
            sample_rate: self.sample_rate,
            channel_mask: self.channel_mask,
            samplebits: self.samplebits,
        }
    }

    /// Push the current config to the device (used both at start and whenever a
    /// device param changes while configured).
    fn push_config(&self) -> Result<(), String> {
        if let Some(dev) = &self.device {
            dev.set_adc_config(&self.current_config())?;
        }
        Ok(())
    }

    /// Host ns per device sample for t0 derivation: the clock-recovered slope
    /// once locked and sane (within 5% of nominal — guards against a bad fit),
    /// else the nominal rate from the device capability. Caller ensures
    /// `effective_rate > 0`.
    fn ns_per_sample(&self) -> f64 {
        let nominal = 1e9 / self.effective_rate as f64;
        match self.clock.ns_per_sample() {
            Some(b) if (b - nominal).abs() < nominal * 0.05 => b,
            _ => nominal,
        }
    }
}

impl Plugin for UsbAdc {
    fn new(_manifest: &PluginManifest) -> Self {
        UsbAdc {
            sample_rate: 16000,
            channel_mask: 0x001F,
            samplebits: 12,
            vref: 3.3,
            reset_on_open: true,
            iso: None,
            bulk: None,
            device: None,
            seq: 0,
            sample_index: 0,
            ticks: 0,
            effective_rate: 0,
            stream_first_index: 0,
            bulk_expect_count: None,
            bulk_expect_seq: None,
            bulk_scratch: Vec::new(),
            bulk_submit_failures_seen: 0,
            stream_t0_ns: None,
            clock: ClockRecovery::new(),
            trig_ref: None,
            pending_trig: [VecDeque::new(), VecDeque::new()],
            trig_seq: [0; NUM_PENS],
            raw: Vec::new(),
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("sample_rate", ParamValue::U32(v)) => self.sample_rate = *v,
            ("channel_mask", ParamValue::U32(v)) => self.channel_mask = *v as u16,
            ("samplebits", ParamValue::U32(v)) => self.samplebits = *v as u8,
            ("vref", ParamValue::F64(v)) => self.vref = *v as f32,
            ("reset_on_open", ParamValue::Bool(v)) => self.reset_on_open = *v,
            _ => return,
        }
        // Device config params: re-push the whole struct while configured
        // (device must be in STOP — these are hot_change=false so the shell
        // rejects changes while Running). vref/reset_on_open are host-only.
        if matches!(id, "sample_rate" | "channel_mask" | "samplebits") {
            let _ = self.push_config();
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        // Open the device on first start (once per process → reset happens
        // once); start/stop cycles within a process reuse the open handle.
        if self.device.is_none() {
            match Device::open(self.reset_on_open) {
                Ok(dev) => self.device = Some(dev),
                Err(e) => {
                    return ProcessOutcome::Fault {
                        reason: format!("open device: {e}"),
                    }
                }
            }
            // Validate the connected device against our static manifest range.
            if let Err(e) = self.validate_capability() {
                return ProcessOutcome::Fault { reason: e };
            }
        }

        let dev = self.device.as_mut().unwrap();

        // Configure: confirm STOP, push config, verify, select ISO bandwidth.
        match dev.get_capability() {
            Ok(cap) if cap.state == DeviceState::Running => {
                let _ = dev.pipe_stop();
            }
            Ok(_) => {}
            Err(e) => return ProcessOutcome::Fault { reason: e },
        }
        if let Err(e) = dev.set_adc_config(&AdcConfig {
            sample_rate: self.sample_rate,
            channel_mask: self.channel_mask,
            samplebits: self.samplebits,
        }) {
            return ProcessOutcome::Fault { reason: e };
        }

        // Determine packet size and the device's *actual* sample rate from the
        // capability (the firmware ignores the configured rate in ISO mode), then
        // start the ISO pipe.
        let (packet_size, effective_rate) = match dev.get_capability() {
            Ok(cap) => ((cap.packet_size as usize).max(1), cap.current_sample_rate),
            Err(e) => return ProcessOutcome::Fault { reason: e },
        };

        // Exact device↔host axis pairing, read while the pipe is still stopped:
        // the device sample counter is frozen (ADC paused) and nothing is in
        // flight, so this count is precisely the device index of the first
        // sample the host will receive (the firmware's PHASE flow control
        // pauses the ADC instead of dropping on backpressure, so no session-
        // start loss). This kills the constant "in-flight" early-bias the old
        // lazy pairing baked in: pairing at the first clock observation set
        // trig_ref = (device count, host count) at an instant when ~ms of
        // produced-but-undelivered samples sat in the USB pipeline, shifting
        // every trigger marker early by that backlog.
        let trig_ref_at_start = match dev.transport() {
            // Bulk: block headers carry the device's absolute sample counts,
            // so the host stream axis IS the device axis — identity mapping,
            // exact by construction (no read, no in-flight, nothing to drift).
            Transport::Bulk => Some((0u64, 0u64)),
            Transport::Iso => match dev.get_device_time() {
                Ok(obs) => Some((obs.sample_count, 0u64)),
                Err(e) => {
                    eprintln!(
                        "usb_adc: pre-start device-time read failed ({e}); \
                         falling back to lazy trigger pairing (markers may carry a \
                         constant in-flight bias)"
                    );
                    None
                }
            },
        };

        if let Err(e) = dev.pipe_start() {
            return ProcessOutcome::Fault { reason: e };
        }

        // Both transports use a standing async URB pool (see bulk.rs for why
        // synchronous reads with timeouts are unsound here).
        match dev.transport() {
            Transport::Iso => {
                let stream = unsafe {
                    IsoStream::start(
                        dev.raw_context(),
                        dev.raw_handle(),
                        dev.ep_in(),
                        packet_size,
                        PACKETS_PER_TRANSFER,
                        NUM_TRANSFERS,
                    )
                };
                match stream {
                    Ok(s) => self.iso = Some(s),
                    Err(e) => {
                        let _ = dev.pipe_stop();
                        return ProcessOutcome::Fault {
                            reason: format!("ISO start: {e}"),
                        };
                    }
                }
            }
            Transport::Bulk => {
                let stream = unsafe {
                    BulkStream::start(
                        dev.raw_context(),
                        dev.raw_handle(),
                        dev.ep_in(),
                        BULK_TRANSFER_BYTES,
                        BULK_NUM_TRANSFERS,
                        protocol::BLOCK_TOTAL,
                    )
                };
                match stream {
                    Ok(s) => self.bulk = Some(s),
                    Err(e) => {
                        let _ = dev.pipe_stop();
                        return ProcessOutcome::Fault {
                            reason: format!("bulk start: {e}"),
                        };
                    }
                }
            }
        }

        // Fresh stream anchor.
        self.seq = 0;
        self.sample_index = 0;
        self.ticks = 0;
        self.effective_rate = effective_rate;
        self.stream_first_index = 0;
        self.bulk_expect_count = None;
        self.bulk_expect_seq = None;
        self.stream_t0_ns = None; // anchored on the first real sample arrival
        self.clock.reset();
        self.trig_ref = trig_ref_at_start;
        for q in &mut self.pending_trig {
            q.clear();
        }
        self.trig_seq = [0; NUM_PENS];
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        self.ticks += 1;

        // Periodic health check: a control failure or FAULT state means the
        // device is gone/faulted → let the shell supervisor restart us.
        if self.ticks % HEALTH_CHECK_EVERY == 0 {
            if let Some(dev) = &self.device {
                match dev.get_capability() {
                    Ok(cap) if cap.state == DeviceState::Fault => {
                        return ProcessOutcome::Fault {
                            reason: "device reported FAULT".to_string(),
                        }
                    }
                    Err(e) => {
                        return ProcessOutcome::Fault {
                            reason: format!("device health check failed: {e}"),
                        }
                    }
                    Ok(_) => {}
                }
            }
        }

        // Periodically sample the device clock to recover its true rate. The
        // nominal rate gates out the startup ramp and glitches.
        if self.ticks % CLOCK_OBS_EVERY == 0 && self.effective_rate > 0 {
            let obs = self.device.as_ref().and_then(|d| d.get_device_time().ok());
            if let Some(obs) = obs {
                self.clock.record(obs, 1e9 / self.effective_rate as f64);
                // Fallback trigger pairing (pre-start read failed in start()):
                // pairs production count vs reception count mid-flight, so it
                // carries the in-flight backlog as a constant marker bias.
                if self.trig_ref.is_none() {
                    self.trig_ref = Some((obs.sample_count, self.sample_index));
                }
            }
        }

        // Periodically drain the device pen-trigger FIFO into per-pen queues.
        // Events carry their own latched device sample index, so they are placed
        // precisely regardless of when we poll.
        if self.ticks % TRIG_POLL_EVERY == 0 && (self.iso.is_some() || self.bulk.is_some()) {
            if let Some(dev) = &self.device {
                if let Ok(ev) = dev.get_trigger_events() {
                    if ev.dropped > 0 {
                        eprintln!("usb_adc: device dropped {} trigger event(s) (FIFO overflow)", ev.dropped);
                    }
                    for e in ev.events {
                        let p = (e.pen as usize).min(NUM_PENS - 1);
                        // One line per pen tap so node.log directly shows whether
                        // the device→host trigger path is delivering events.
                        eprintln!(
                            "usb_adc: pen{p} trigger @ device_sample={} (host_si={}, queued={})",
                            e.sample_count,
                            self.sample_index,
                            self.pending_trig[p].len() + 1,
                        );
                        if self.pending_trig[p].len() < MAX_PENDING_TRIG {
                            self.pending_trig[p].push_back(e.sample_count);
                        } else {
                            eprintln!("usb_adc: pen{p} pending queue full ({MAX_PENDING_TRIG}), dropping event");
                        }
                    }
                }
            }
        }

        // Drain the data pipe into the raw staging buffer. Both paths leave
        // interleaved u16 samples in self.raw; gap_samples > 0 flags a
        // detected discontinuity (bulk: in-band count jump; iso: transfer
        // error with unknown gap).
        self.raw.clear();
        let is_bulk = self.device.as_ref().map(|d| d.transport()) == Some(Transport::Bulk);
        let gap_samples: u64;
        if is_bulk {
            if self.effective_rate == 0 {
                return ProcessOutcome::Ok; // not started
            }
            gap_samples = match self.drain_bulk() {
                Ok(g) => g,
                Err(e) => return ProcessOutcome::Fault { reason: e },
            };
        } else {
            let error_packets = match self.iso.as_mut() {
                Some(s) => s.drain(&mut self.raw),
                None => return ProcessOutcome::Ok, // not streaming
            };
            gap_samples = if error_packets > 0 { u64::MAX } else { 0 };
        }

        let channels = self.channels().max(1) as usize;
        let out = match outputs.first_mut() {
            Some(o) => o,
            None => return ProcessOutcome::Ok,
        };

        // Convert raw interleaved u16 → interleaved f32 voltage.
        let n_f32 = convert::convert_block(&self.raw, out.buffer_mut(), self.samplebits, self.vref);
        out.set_written(n_f32 * 4);

        // Per-channel sample count for the self-anchored header.
        let n_samples = (n_f32 / channels) as u32;

        // Anchor the stream to the monotonic clock on the first frame that
        // actually carries samples (the device starts delivering well after
        // pipe_start). The anchored index is this frame's first sample: 0 for
        // ISO (host-relative counting), the device's absolute count for bulk.
        if self.stream_t0_ns.is_none() && n_samples > 0 {
            self.stream_t0_ns = Some(mono_ns());
            self.stream_first_index = self.sample_index;
        }

        out.header.seq = self.seq;
        out.header.sample_index = self.sample_index;
        out.header.n_samples = n_samples;
        // t0 = absolute monotonic anchor of the stream's first sample + this
        // frame's offset from it, using the device's *actual* rate (the
        // firmware ignores the configured rate), not the host `sample_rate`.
        out.header.t0_ns = match self.stream_t0_ns {
            Some(t0) if self.effective_rate > 0 => {
                t0 + ((self.sample_index - self.stream_first_index) as f64
                    * self.ns_per_sample()) as i64
            }
            _ => 0,
        };
        // Discontinuity: bulk reports the exact in-band count jump (u64::MAX
        // marks the ISO transfer-error case where the gap size is unknowable
        // in byte-stream mode → flag with 0 per the header contract).
        if gap_samples > 0 {
            out.header
                .set_discontinuity(if gap_samples == u64::MAX { 0 } else { gap_samples });
        }

        self.seq += 1;
        self.sample_index += n_samples as u64;

        // Emit one pen-trigger marker per port from the per-pen queues. A marker
        // is a header-only frame (n_samples=0) whose t0 uses the SAME anchor +
        // rate as the data frames, so it lands on the right sample (PLL error
        // cancels). Untouched ports keep their ZERO header → the shell skips
        // them, so a real marker is never coalesced away by last-wins.
        if let (Some(t0_anchor), Some((c_ref, si_ref))) = (self.stream_t0_ns, self.trig_ref) {
            let nsps = self.ns_per_sample();
            for p in 0..NUM_PENS {
                let c_trig = match self.pending_trig[p].pop_front() {
                    Some(c) => c,
                    None => continue,
                };
                let out = match outputs.get_mut(1 + p) {
                    Some(o) => o,
                    None => continue,
                };
                // Trigger's host sample index on this stream's axis, then the
                // shared f(s) = stream_t0 + s · ns_per_sample mapping.
                let s_trig = si_ref as i64 + (c_trig as i64 - c_ref as i64);
                let marker_t0 = t0_anchor
                    + ((s_trig - self.stream_first_index as i64) as f64 * nsps) as i64;
                out.set_written(0);
                out.header.seq = self.trig_seq[p];
                // The marker carries its *stream axis* sample index (flagged),
                // so the capture plugin can align by index directly instead of
                // round-tripping through t0 — the time path re-derives the
                // index against the consumer's own anchor + inferred rate,
                // whose error grows with anchor age and jumps when the PLL
                // slope updates (observed as occasional multi-ms outliers).
                // t0 stays populated as the cross-stream fallback.
                out.header.sample_index = s_trig.max(0) as u64;
                out.header.set_trig_indexed();
                out.header.n_samples = 0;
                out.header.t0_ns = marker_t0;
                self.trig_seq[p] += 1;
            }
        }

        ProcessOutcome::Ok
    }

    fn stop(&mut self) {
        // Cancel URBs (drop the streams) and stop the pipe, but keep the
        // device open so a later start() does not re-reset.
        self.iso = None;
        self.bulk = None;
        if let Some(dev) = &self.device {
            let _ = dev.pipe_stop();
        }
    }

    fn shutdown(&mut self) {
        self.iso = None;
        self.bulk = None;
        self.device = None; // Device::drop releases the interface
    }
}

impl UsbAdc {
    /// Drain the bulk pipe: read framed blocks (16 B header + payload, one
    /// short-packet transfer each) until a timeout or the per-tick cap,
    /// appending payload bytes to `self.raw`. Returns the total in-band
    /// sample-count gap detected (0 = contiguous — the norm: bulk retransmits,
    /// so a gap means device-side overrun/restart, not wire loss).
    ///
    /// The first block of a tick sets `self.sample_index` to its
    /// `first_sample_count`: the host stream carries the device's absolute
    /// axis, which is what makes trigger alignment an identity mapping.
    fn drain_bulk(&mut self) -> Result<u64, String> {
        let frame_size = (self.channels().max(1) as usize) * 2;
        let mut gap: u64 = 0;

        // Pump the standing pool; completed (always-full) transfers land in
        // scratch in stream order. Transfer errors drop their bytes — the
        // resulting hole shows up exactly in the in-band counts below.
        let mut scratch = std::mem::take(&mut self.bulk_scratch);
        scratch.clear();
        let (errors, err_status, salvaged) = match self.bulk.as_mut() {
            Some(s) => s.drain(&mut scratch),
            None => {
                self.bulk_scratch = scratch;
                return Ok(0); // not streaming
            }
        };
        if errors > 0 {
            eprintln!(
                "usb_adc: {errors} bulk transfer error(s), last status={err_status}, \
                 salvaged {salvaged} B"
            );
        }
        // 池缩水监控：回调内重提交失败会让常驻池永久少一个传输。
        let sf = self.bulk.as_ref().map(|s| s.submit_failures()).unwrap_or(0);
        if sf != self.bulk_submit_failures_seen {
            eprintln!(
                "usb_adc: bulk resubmit failures now {sf} (pool shrunk!)"
            );
            self.bulk_submit_failures_seen = sf;
        }
        debug_assert!(scratch.len() % protocol::BLOCK_TOTAL == 0);

        for raw_blk in scratch.chunks_exact(protocol::BLOCK_TOTAL) {
            let (h, payload) = match BlockHeader::parse(raw_blk, frame_size) {
                Ok(v) => v,
                Err(e) => {
                    self.bulk_scratch = Vec::new();
                    return Err(e);
                }
            };
            if h.flags & BLOCK_FLAG_NEW_SESSION != 0 {
                // Device re-anchored (pipe restart): drop continuity state
                // and re-anchor t0 on this frame.
                self.stream_t0_ns = None;
                self.bulk_expect_count = None;
                self.bulk_expect_seq = None;
            }
            if let Some(expect) = self.bulk_expect_count {
                if h.first_sample_count != expect {
                    // Mid-tick gaps leave this frame internally
                    // non-contiguous; the discontinuity flag makes
                    // downstream re-anchor anyway.
                    gap += h.first_sample_count.saturating_sub(expect);
                    eprintln!(
                        "usb_adc: bulk count jump: expected {expect}, got {} (seq {}, prev_seq {:?})",
                        h.first_sample_count, h.seq,
                        self.bulk_expect_seq.map(|x| x.wrapping_sub(1))
                    );
                }
            }
            if self.raw.is_empty() {
                // First block this tick anchors the frame on the device axis.
                self.sample_index = h.first_sample_count;
            }
            self.raw.extend_from_slice(payload);
            self.bulk_expect_count = Some(h.first_sample_count + h.n_frames as u64);
            self.bulk_expect_seq = Some(h.seq.wrapping_add(1));
        }
        self.bulk_scratch = scratch;
        Ok(gap)
    }

    /// Check the connected device's reported capability is consistent with the
    /// configured params (a wrong device / firmware → refuse, go Degraded).
    fn validate_capability(&self) -> Result<(), String> {
        let dev = self.device.as_ref().ok_or("no device")?;
        let cap = dev.get_capability()?;
        if self.channel_mask & !cap.channel_mask_avail != 0 {
            return Err(format!(
                "channel_mask {:#06x} not supported by device (avail {:#06x})",
                self.channel_mask, cap.channel_mask_avail
            ));
        }
        if self.sample_rate < cap.sample_rate_min || self.sample_rate > cap.sample_rate_max {
            return Err(format!(
                "sample_rate {} outside device range {}..{}",
                self.sample_rate, cap.sample_rate_min, cap.sample_rate_max
            ));
        }
        if !cap.samplebits_list.is_empty() && !cap.samplebits_list.contains(&self.samplebits) {
            return Err(format!(
                "samplebits {} not in device list {:?}",
                self.samplebits, cap.samplebits_list
            ));
        }
        Ok(())
    }

    /// Diagnostic: read back what the device reports it is doing (used by the
    /// hardware report test to compare configured vs. effective settings).
    #[cfg(test)]
    fn debug_capability(&self) -> Option<protocol::Capability> {
        self.device.as_ref().and_then(|d| d.get_capability().ok())
    }

    /// Diagnostic: failed ISO transfer resubmissions (pool-shrink indicator).
    #[cfg(test)]
    fn debug_submit_failures(&self) -> u64 {
        self.iso.as_ref().map(|s| s.submit_failures()).unwrap_or(0)
    }

    /// Diagnostic: the host-ns-per-sample currently used for t0 (recovered or
    /// nominal), plus whether the clock has locked.
    #[cfg(test)]
    fn debug_clock(&self) -> (f64, bool) {
        (self.ns_per_sample(), self.clock.ns_per_sample().is_some())
    }
}

sigflow_plugin_sdk::process_main!(UsbAdc, include_str!("../manifest.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest.toml parses")
    }

    #[test]
    fn manifest_shape() {
        let m = manifest();
        assert_eq!(m.name, "sigflow.io.usb_adc");
        // adc_out + two pen-trigger ports, in the order process() indexes them.
        assert_eq!(m.ports.len(), 3);
        assert_eq!(m.ports[0].id, "adc_out");
        assert_eq!(m.ports[1].id, "trigger_pen0");
        assert_eq!(m.ports[2].id, "trigger_pen1");
        for p in &m.ports {
            assert!(
                matches!(p.direction, sigflow_plugin_sdk::Direction::Producer),
                "{} must be a producer",
                p.id
            );
        }
        // device params are hot_change=false
        for id in ["sample_rate", "channel_mask", "samplebits"] {
            let p = m.parameters.iter().find(|p| p.id == id).unwrap();
            assert!(!p.hot_change, "{id} must be hot_change=false");
        }
        // sample_rate is read-only in ISO mode (device-determined), with a reason.
        let sr = m.parameters.iter().find(|p| p.id == "sample_rate").unwrap();
        assert!(!sr.editable, "sample_rate must be editable=false (device-fixed in ISO)");
        assert!(sr.editable_reason.is_some(), "sample_rate needs an editable_reason");
        // other params remain editable
        for id in ["channel_mask", "vref"] {
            assert!(
                m.parameters.iter().find(|p| p.id == id).unwrap().editable,
                "{id} should stay editable"
            );
        }
    }

    #[test]
    fn constructs_and_accepts_params() {
        let mut p = UsbAdc::new(&manifest());
        p.set_param("sample_rate", &ParamValue::U32(32000));
        p.set_param("channel_mask", &ParamValue::U32(0x000F));
        p.set_param("vref", &ParamValue::F64(5.0));
        assert_eq!(p.sample_rate, 32000);
        assert_eq!(p.channels(), 4);
        assert!((p.vref - 5.0).abs() < 1e-6);
    }

    // Real device streaming is validated manually on hardware (archlinux + the
    // board + udev rules):
    //   cargo build -p sigflow-plugin-usb-adc
    //   sigflow plugin install plugins/process/usb-adc/...   # once packaged
    //   attach node, set params, start, observe adc_out
    #[test]
    #[ignore = "requires the physical USB ADC device"]
    fn hardware_stream_smoke() {
        let mut p = UsbAdc::new(&manifest());
        assert!(matches!(p.start(), ProcessOutcome::Ok), "device must be attached");
        let mut buf = vec![0u8; 1 << 16];
        let mut outs = [FrameOut::new(&mut buf)];
        let _ = p.process(&[], &mut outs);
        p.stop();
        p.shutdown();
    }

    // Prove clock recovery tracks the *true* device rate, not nominal: drive the
    // full plugin (process() polls GET_DEVICE_TIME and fits the slope) for ~18 s,
    // then compare the recovered rate against an independent device-time
    // measurement, and confirm it differs from nominal by the real crystal
    // offset. Run (needs the GET_DEVICE_TIME firmware):
    //   cargo test --release -p sigflow-plugin-usb-adc hardware_clock_recovery \
    //       -- --ignored --nocapture
    #[test]
    #[ignore = "requires the physical USB ADC device (new GET_DEVICE_TIME firmware)"]
    fn hardware_clock_recovery() {
        use std::time::Instant;

        let mut p = UsbAdc::new(&manifest());
        assert!(matches!(p.start(), ProcessOutcome::Ok), "device must be attached");
        let nominal_hz = p.effective_rate as f64;

        let mut buf = vec![0u8; 1 << 16];
        let drive = |p: &mut UsbAdc, buf: &mut [u8]| {
            let mut outs = [FrameOut::new(buf)];
            let _ = p.process(&[], &mut outs);
        };

        // Warm up past the startup ramp and let the window fill.
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < 3.0 {
            drive(&mut p, &mut buf);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let ref1 = p.device.as_ref().unwrap().get_device_time().expect("device time #1");

        // Run long enough for a tight rate fit.
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < 15.0 {
            drive(&mut p, &mut buf);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let ref2 = p.device.as_ref().unwrap().get_device_time().expect("device time #2");
        let (ns_per_sample, locked) = p.debug_clock();
        p.stop();
        p.shutdown();

        let ref_hz = (ref2.sample_count - ref1.sample_count) as f64
            / ((ref2.host_mono_mid_ns - ref1.host_mono_mid_ns) as f64 / 1e9);
        let recovered_hz = 1e9 / ns_per_sample;
        let off_ppm = (recovered_hz / nominal_hz - 1.0) * 1e6;
        eprintln!("--- clock recovery ---");
        eprintln!("nominal: {nominal_hz:.1} Hz/ch");
        eprintln!("reference (15 s GET_DEVICE_TIME span): {ref_hz:.2} Hz/ch");
        eprintln!("recovered (PLL slope): {recovered_hz:.2} Hz/ch  ({off_ppm:+.0} ppm vs nominal)  locked={locked}");

        assert!(locked, "clock must have locked");
        assert!((recovered_hz - ref_hz).abs() / ref_hz < 0.001,
                "recovered {recovered_hz:.2} deviates >0.1% from reference {ref_hz:.2}");
    }

    // Pen IO-trigger path: stream for ~15 s while you tap the pens wired to
    // PB4/PB5, and confirm header-only markers appear on the trigger_pen0 /
    // trigger_pen1 outputs with t0 values that fall inside the live ADC stream's
    // [t0_first, t0_last] window (i.e. they map onto real samples). Run:
    //   cargo test --release -p sigflow-plugin-usb-adc hardware_pen_trigger \
    //       -- --ignored --nocapture
    // then physically tap PB4 (pen0) and PB5 (pen1) a few times during the run.
    #[test]
    #[ignore = "requires the physical USB ADC device + pen trigger wiring (PB4/PB5)"]
    fn hardware_pen_trigger() {
        use std::time::Instant;

        let mut p = UsbAdc::new(&manifest());
        p.set_param("reset_on_open", &ParamValue::Bool(false));
        match p.start() {
            ProcessOutcome::Ok => {}
            other => panic!("start failed (device must be attached): {other:?}"),
        }

        // Three output buffers: adc_out, trigger_pen0, trigger_pen1.
        let mut adc = vec![0u8; 1 << 18];
        let mut t0buf = [0u8; 64];
        let mut t1buf = [0u8; 64];

        let mut data_t0_first: Option<i64> = None;
        let mut data_t0_last: i64 = 0;
        let mut markers: [Vec<i64>; NUM_PENS] = [Vec::new(), Vec::new()];

        eprintln!("--- pen trigger: tap PB4 (pen0) and PB5 (pen1) over ~15 s ---");
        let started = Instant::now();
        while started.elapsed().as_secs_f64() < 15.0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            // Reborrow the three buffers as fresh FrameOuts each tick.
            let (ah, p0, p1) = {
                let mut outs = [
                    FrameOut::new(&mut adc),
                    FrameOut::new(&mut t0buf),
                    FrameOut::new(&mut t1buf),
                ];
                match p.process(&[], &mut outs) {
                    ProcessOutcome::Ok => {}
                    other => panic!("process faulted: {other:?}"),
                }
                (outs[0].header, outs[1].header, outs[2].header)
            };
            if ah.n_samples > 0 {
                if data_t0_first.is_none() {
                    data_t0_first = Some(ah.t0_ns);
                }
                data_t0_last = ah.t0_ns;
            }
            // A marker is a header-only frame with a nonzero t0.
            if p0.t0_ns != 0 && p0.n_samples == 0 {
                eprintln!("pen0 marker: t0={} ns", p0.t0_ns);
                markers[0].push(p0.t0_ns);
            }
            if p1.t0_ns != 0 && p1.n_samples == 0 {
                eprintln!("pen1 marker: t0={} ns", p1.t0_ns);
                markers[1].push(p1.t0_ns);
            }
        }
        p.stop();
        p.shutdown();

        let t0_first = data_t0_first.expect("no ADC data frames seen");
        eprintln!(
            "data window: [{t0_first}, {data_t0_last}] ns ({} ms)",
            (data_t0_last - t0_first) / 1_000_000
        );
        eprintln!("markers: pen0={} pen1={}", markers[0].len(), markers[1].len());
        assert!(
            markers[0].len() + markers[1].len() > 0,
            "no pen markers — tap PB4/PB5 during the run, check wiring/polarity"
        );
        // Each marker must land within the live data window (maps to a real sample).
        for (pen, ts) in markers.iter().enumerate() {
            for &t in ts {
                assert!(
                    t >= t0_first && t <= data_t0_last,
                    "pen{pen} marker t0={t} outside data window [{t0_first}, {data_t0_last}]"
                );
            }
        }
    }

    // Isolation harness: drive the raw Device + IsoStream directly (no process()
    // path, no health check, no conversion), tight-looping drain for ~3 s, to
    // measure the ISO layer's throughput ceiling against the device clock.
    //   cargo test --release -p sigflow-plugin-usb-adc hardware_raw_iso_ceiling \
    //       -- --ignored --nocapture
    #[test]
    #[ignore = "requires the physical USB ADC device"]
    fn hardware_raw_iso_ceiling() {
        use std::time::Instant;
        use device::Device;
        use iso::IsoStream;
        use protocol::AdcConfig;

        let dev = Device::open(false).expect("open device");
        let cap0 = dev.get_capability().expect("get_capability");
        if cap0.state == DeviceState::Running {
            let _ = dev.pipe_stop();
        }
        dev.set_adc_config(&AdcConfig { sample_rate: 64000, channel_mask: 0x001F, samplebits: 12 })
            .expect("set_adc_config");
        let cap = dev.get_capability().expect("get_capability");
        let packet_size = (cap.packet_size as usize).max(1);
        dev.pipe_start().expect("pipe_start");

        let mut stream = unsafe {
            IsoStream::start(dev.raw_context(), dev.raw_handle(), dev.ep_in(),
                             packet_size, PACKETS_PER_TRANSFER, NUM_TRANSFERS)
        }
        .expect("iso start");

        let ch = cap.current_channel_mask.count_ones().max(1);
        let dev_bps = (cap.current_sample_rate * ch * 2) as f64;

        // Bucket throughput per 250 ms to expose any startup ramp, and measure
        // steady-state rate (excluding a warm-up window) vs. the device clock.
        const RUN: f64 = 6.0;
        const BUCKET: f64 = 0.25;
        const WARMUP: f64 = 1.5;
        let nbuckets = (RUN / BUCKET) as usize;
        let mut buckets = vec![0u64; nbuckets];
        let mut raw = Vec::with_capacity(1 << 20);
        let mut total: u64 = 0;
        let mut errors: u64 = 0;
        let mut first_byte_s = -1.0f64;

        let started = Instant::now();
        loop {
            let t = started.elapsed().as_secs_f64();
            if t >= RUN {
                break;
            }
            raw.clear();
            errors += stream.drain(&mut raw) as u64;
            let n = raw.len() as u64;
            if n > 0 {
                if first_byte_s < 0.0 {
                    first_byte_s = t;
                }
                total += n;
                let b = ((t / BUCKET) as usize).min(nbuckets - 1);
                buckets[b] += n;
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        drop(stream);
        let _ = dev.pipe_stop();

        eprintln!("--- raw iso ramp ({elapsed:.1}s, device ~{:.1} KB/s) ---", dev_bps / 1024.0);
        eprintln!("first bytes at: {first_byte_s:.3}s after stream start");
        eprintln!("per-250ms KB/s (capture%):");
        for (i, &b) in buckets.iter().enumerate() {
            let t0 = i as f64 * BUCKET;
            let kbps = b as f64 / 1024.0 / BUCKET;
            eprintln!("  t={t0:>4.2}s  {kbps:>6.1} KB/s  ({:>3.0}%)", b as f64 / BUCKET / dev_bps * 100.0);
        }
        // Steady-state: sum buckets after the warm-up window.
        let warm_b = (WARMUP / BUCKET) as usize;
        let steady_bytes: u64 = buckets[warm_b..].iter().sum();
        let steady_s = (nbuckets - warm_b) as f64 * BUCKET;
        let steady_pct = steady_bytes as f64 / steady_s / dev_bps * 100.0;
        eprintln!("overall: {:.0}% | steady-state (after {WARMUP}s): {steady_pct:.0}% | errored-packets {errors}",
                  total as f64 / elapsed / dev_bps * 100.0);
    }

    // Validate the GET_DEVICE_TIME clock contract end to end: while actively
    // draining (so the ADC keeps free-running and the device counter advances),
    // read the device sample counter at two instants ~1.5 s apart and confirm
    // its rate against the host monotonic clock tracks the device sample rate.
    // Run:
    //   cargo test --release -p sigflow-plugin-usb-adc hardware_device_time \
    //       -- --ignored --nocapture
    #[test]
    #[ignore = "requires the physical USB ADC device (new GET_DEVICE_TIME firmware)"]
    fn hardware_device_time() {
        use device::Device;
        use iso::IsoStream;
        use protocol::AdcConfig;
        use std::time::Instant;

        let dev = Device::open(false).expect("open device");
        if dev.get_capability().expect("get_capability").state == DeviceState::Running {
            let _ = dev.pipe_stop();
        }
        dev.set_adc_config(&AdcConfig { sample_rate: 64000, channel_mask: 0x001F, samplebits: 12 })
            .expect("set_adc_config");
        let cap = dev.get_capability().expect("get_capability");
        let expected_hz = cap.current_sample_rate;
        let packet_size = (cap.packet_size as usize).max(1);
        dev.pipe_start().expect("pipe_start");
        let mut stream = unsafe {
            IsoStream::start(dev.raw_context(), dev.raw_handle(), dev.ep_in(),
                             packet_size, PACKETS_PER_TRANSFER, NUM_TRANSFERS)
        }
        .expect("iso start");

        // Drain continuously; capture a device-time observation just after the
        // startup ramp and again ~1.5 s later.
        let mut raw = Vec::with_capacity(1 << 16);
        let started = Instant::now();
        let mut first: Option<protocol::DeviceTimeObs> = None;
        let mut last: Option<protocol::DeviceTimeObs> = None;
        while started.elapsed().as_secs_f64() < 3.0 {
            raw.clear();
            let _ = stream.drain(&mut raw);
            let t = started.elapsed().as_secs_f64();
            if first.is_none() && t >= 1.0 {
                first = Some(dev.get_device_time().expect("get_device_time #1"));
            } else if first.is_some() && last.is_none() && t >= 2.5 {
                last = Some(dev.get_device_time().expect("get_device_time #2"));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let _ = dev.pipe_stop();

        let a = first.expect("first device-time obs");
        let b = last.expect("second device-time obs");
        let d_samples = b.sample_count.saturating_sub(a.sample_count) as f64;
        let d_host_s = (b.host_mono_mid_ns - a.host_mono_mid_ns) as f64 / 1e9;
        let implied_hz = d_samples / d_host_s;
        eprintln!("--- device time ---");
        eprintln!("obs1: count={} mid={}ns rtt={}us", a.sample_count, a.host_mono_mid_ns, a.rtt_ns / 1000);
        eprintln!("obs2: count={} mid={}ns rtt={}us", b.sample_count, b.host_mono_mid_ns, b.rtt_ns / 1000);
        eprintln!("Δsamples={d_samples:.0} over Δhost={d_host_s:.3}s → device clock ~{implied_hz:.1} Hz/ch (nominal {expected_hz})");

        assert!(b.sample_count > a.sample_count, "device counter must advance");
        assert!(d_host_s > 1.0, "expected ~1.5 s between observations, got {d_host_s:.3}s");
        let ratio = implied_hz / expected_hz as f64;
        assert!((ratio - 1.0).abs() < 0.02,
                "device clock {implied_hz:.1} Hz/ch deviates >2% from nominal {expected_hz}");
    }

    // Richer manual validation: stream for ~2 s, mimicking the shell's source
    // tick, and report observed throughput / voltage / discontinuities. Run:
    //   cargo test --release -p sigflow-plugin-usb-adc hardware_stream_report \
    //       -- --ignored --nocapture
    //
    // The device free-runs at 64000 Hz/ch on this firmware (SET_ADC_CONFIG's
    // rate field is not honored in ISO mode — channel_mask/samplebits are), so
    // we configure to match and assert the effective throughput tracks it.
    #[test]
    #[ignore = "requires the physical USB ADC device"]
    fn hardware_stream_report() {
        use std::time::Instant;

        const EXPECTED_HZ: u32 = 64000; // device free-runs here regardless of config
        const CONFIGURED_HZ: u32 = 16000; // deliberately != device, to prove t0 ignores it

        let mut p = UsbAdc::new(&manifest());
        p.set_param("sample_rate", &ParamValue::U32(CONFIGURED_HZ));
        p.set_param("channel_mask", &ParamValue::U32(0x001F));
        p.set_param("reset_on_open", &ParamValue::Bool(false));
        match p.start() {
            ProcessOutcome::Ok => {}
            other => panic!("start failed (device must be attached): {other:?}"),
        }
        let channels = p.channels().max(1) as usize;
        eprintln!(
            "configured: channels={channels} sample_rate={}Hz samplebits={} vref={}V",
            p.sample_rate, p.samplebits, p.vref
        );
        // Read back what the device says it is actually doing.
        if let Some(cap) = p.debug_capability() {
            eprintln!(
                "device reports: cur_sr={}Hz cur_ch_mask={:#06x} ({} ch) \
                 cur_bits={} state={:?} packet_size={}B block_size={}B \
                 sr_range={}..{}",
                cap.current_sample_rate,
                cap.current_channel_mask,
                cap.current_channel_mask.count_ones(),
                cap.current_samplebits,
                cap.state,
                cap.packet_size,
                cap.block_data_size,
                cap.sample_rate_min,
                cap.sample_rate_max,
            );
        }

        let mut buf = vec![0u8; 1 << 18];
        let mut total_samples_per_ch: u64 = 0;
        let mut frames_with_data: u64 = 0;
        let mut discontinuities: u64 = 0;
        let mut vmin = f32::INFINITY;
        let mut vmax = f32::NEG_INFINITY;
        let mut vsum = 0.0f64;
        let mut vcount: u64 = 0;
        // Track the t0/sample_index anchor of the first and last data frames so
        // we can recover the rate the header *implies* and confirm it tracks the
        // device clock, not the (deliberately wrong) configured rate.
        let mut first_anchor: Option<(u64, i64)> = None;
        let mut last_anchor: (u64, i64) = (0, 0);

        // Warm up: the device does not begin streaming until ~0.5 s after
        // pipe_start and ramps to full rate by ~0.75 s, so drain (discarding
        // stats) past the ramp before measuring steady-state throughput.
        {
            let warm = Instant::now();
            let mut wbuf = vec![0u8; 1 << 18];
            while warm.elapsed().as_secs_f64() < 1.5 {
                std::thread::sleep(std::time::Duration::from_millis(1));
                let mut outs = [FrameOut::new(&mut wbuf)];
                let _ = p.process(&[], &mut outs);
            }
        }

        // Drive at the shell's source-tick cadence (~1 ms) for ~2 s.
        let mut ticks: u64 = 0;
        let started = Instant::now();
        while started.elapsed().as_secs_f64() < 2.0 {
            ticks += 1;
            std::thread::sleep(std::time::Duration::from_millis(1));
            let (h, written) = {
                let mut outs = [FrameOut::new(&mut buf)];
                match p.process(&[], &mut outs) {
                    ProcessOutcome::Ok => {}
                    other => panic!("process faulted mid-stream: {other:?}"),
                }
                (outs[0].header, outs[0].written())
            };
            if h.n_samples > 0 {
                frames_with_data += 1;
                total_samples_per_ch += h.n_samples as u64;
                if h.is_discontinuity() {
                    discontinuities += 1;
                }
                if first_anchor.is_none() {
                    first_anchor = Some((h.sample_index, h.t0_ns));
                }
                last_anchor = (h.sample_index, h.t0_ns);
                // First-channel voltage stats from the written f32 block (now
                // sitting at the start of `buf`).
                let n_floats = written / 4;
                let floats: &[f32] =
                    unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const f32, n_floats) };
                for frame in floats.chunks_exact(channels) {
                    let v = frame[0];
                    vmin = vmin.min(v);
                    vmax = vmax.max(v);
                    vsum += v as f64;
                    vcount += 1;
                }
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        let submit_failures = p.debug_submit_failures();

        p.stop();
        p.shutdown();

        let effective_hz = total_samples_per_ch as f64 / elapsed;
        eprintln!("--- stream report (steady-state, {ticks} ticks, {elapsed:.3}s wall) ---");
        eprintln!("frames with data: {frames_with_data}");
        eprintln!("samples/channel:  {total_samples_per_ch}");
        eprintln!("effective rate:   ~{effective_hz:.0} Hz/ch (device clock ~{EXPECTED_HZ})");
        eprintln!("discontinuities:  {discontinuities} (real ISO transfer errors)");
        eprintln!("submit failures:  {submit_failures} (resubmit errors → pool shrink)");
        if vcount > 0 {
            eprintln!(
                "ch0 voltage:      min={vmin:.4}V max={vmax:.4}V mean={:.4}V (n={vcount})",
                vsum / vcount as f64
            );
        } else {
            eprintln!("ch0 voltage:      no samples captured");
        }

        // Header t0 must be derived from the device's actual rate, not the
        // configured one: recover the rate implied by the t0/sample_index anchors
        // and confirm it tracks the device clock (≈64000), NOT the configured
        // 16000. This is what makes downstream absolute timing correct.
        if let Some((si0, t0)) = first_anchor {
            let (si1, t1) = last_anchor;
            let dsi = si1.saturating_sub(si0);
            let dt_ns = (t1 - t0) as f64;
            if dt_ns > 0.0 {
                let implied_hz = dsi as f64 / (dt_ns / 1e9);
                eprintln!(
                    "t0-implied rate:  ~{implied_hz:.0} Hz/ch (device ~{EXPECTED_HZ}, configured {CONFIGURED_HZ})"
                );
                let r = implied_hz / EXPECTED_HZ as f64;
                assert!(
                    (0.98..=1.02).contains(&r),
                    "t0 implies {implied_hz:.0} Hz/ch, expected device {EXPECTED_HZ} \
                     (t0 derived from configured rate instead of cur_sr?)"
                );
            }
        }

        assert!(total_samples_per_ch > 0, "no samples received from device");
        // Data must be contiguous (no corrupted/errored ISO transfers).
        assert_eq!(discontinuities, 0, "unexpected ISO transfer errors");
        // After warm-up the plugin captures the full device clock (byte-exact in
        // the raw-ISO ceiling test); the process() path keeps up at the 1 ms
        // source tick. Allow modest slack for measurement-window edges.
        let ratio = effective_hz / EXPECTED_HZ as f64;
        eprintln!("capture ratio:    {ratio:.2}x of device clock");
        assert!(
            ratio > 0.9,
            "steady-state rate {effective_hz:.0} Hz/ch is only {ratio:.2}x the device {EXPECTED_HZ}"
        );
    }
}
