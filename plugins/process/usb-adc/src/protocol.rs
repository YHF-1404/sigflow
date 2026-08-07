//! USB ADC device protocol: identifiers, vendor requests, and the on-wire
//! capability / config structures.
//!
//! Mirrors the firmware `usb_iso_capability.h` and the Python reference
//! `adc_host/device.py`. All multi-byte fields are little-endian and the
//! structs are packed (no alignment padding), matching the firmware's
//! `__attribute__((packed))` layout.

// ── Device identity ─────────────────────────────────────────────────────────
pub const VID: u16 = 0x1FF7;
pub const PID_ISO: u16 = 0x0F61;
/// Bulk-transport firmware variant (Phase 2): reliable delivery + per-block
/// in-band sample counts. Same vendor-request set as ISO (0x01..0x13).
pub const PID_BULK: u16 = 0x0F60;
pub const EP_IN: u8 = 0x81;

/// Which data transport the connected firmware speaks (determined by PID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Isochronous IN: raw byte stream, lossy (silent drops), zero-copy.
    Iso,
    /// Bulk IN: framed blocks `[16B header | samples]`, reliable, in-band
    /// device sample counts (the data axis IS the device axis).
    Bulk,
}

// ── Vendor request codes ────────────────────────────────────────────────────
pub const REQ_GET_CAPABILITY: u8 = 0x01;
pub const REQ_SET_ADC_CONFIG: u8 = 0x02;
pub const REQ_PIPE_START: u8 = 0x10;
pub const REQ_PIPE_STOP: u8 = 0x11;
/// Read the device's free-running per-channel sample counter (clock recovery).
pub const REQ_GET_DEVICE_TIME: u8 = 0x12;
/// Drain the device's pen-trigger event FIFO (IO-trigger capture).
pub const REQ_GET_TRIGGER_EVENTS: u8 = 0x13;

// ── bmRequestType (vendor | device) ─────────────────────────────────────────
/// device→host, vendor, recipient device.
pub const BM_IN: u8 = 0xC1;
/// host→device, vendor, recipient device.
pub const BM_OUT: u8 = 0x41;

/// Wire size of the capability structure (`<HIIH4sBIHBBHH`).
pub const CAP_SIZE: usize = 29;
/// Wire size of the ADC config structure (`<IHBx`).
pub const CFG_SIZE: usize = 8;
/// Wire size of the GET_DEVICE_TIME response (`u64 sample_count`, LE).
pub const DEV_TIME_SIZE: usize = 8;
/// Max trigger events per GET_TRIGGER_EVENTS (firmware `USB_ISO_TRIG_MAX_EVENTS`).
pub const TRIG_MAX_EVENTS: usize = 16;
/// Wire size of one trigger event: `u64 sample_count + u8 pen + 7 pad` = 16 B.
pub const TRIG_EVENT_SIZE: usize = 16;
/// Wire size of the GET_TRIGGER_EVENTS response:
/// `u32 count + u32 dropped + TRIG_MAX_EVENTS × TRIG_EVENT_SIZE` = 264 B.
pub const TRIG_EVENTS_SIZE: usize = 8 + TRIG_MAX_EVENTS * TRIG_EVENT_SIZE;

// ── Bulk block framing (Phase 2) ────────────────────────────────────────────

/// Bulk block header magic: bytes 'S','D','C','B' (LE u32).
pub const BLOCK_MAGIC: u32 = 0x4243_4453;
/// Device restarted the pipe / re-anchored the sample axis: re-anchor t0.
pub const BLOCK_FLAG_NEW_SESSION: u16 = 1 << 0;
/// Wire size of the bulk block header.
pub const BLOCK_HEADER_SIZE: usize = 24;
/// Every block is exactly this long (header + payload + zero pad). Fixed-size
/// max-packet blocks never short-terminate a transfer, so the host can drain
/// many blocks per large URB and parse on 512-byte strides; payload length is
/// explicit in `n_frames`.
pub const BLOCK_TOTAL: usize = 512;

/// Parsed bulk block header. `first_sample_count` is the device's absolute
/// per-channel index of the block's first frame — the same axis
/// GET_DEVICE_TIME and trigger events use, so the host stream can carry
/// device counts directly and trigger alignment needs no mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    pub seq: u16,
    pub flags: u16,
    pub first_sample_count: u64,
    /// Valid frames in this block's payload (rest is zero padding).
    pub n_frames: u16,
}

impl BlockHeader {
    /// Parse one fixed-size block; returns the header and the *valid* payload
    /// slice (`n_frames × frame_size` bytes, padding excluded).
    pub fn parse(raw: &[u8], frame_size: usize) -> Result<(BlockHeader, &[u8]), String> {
        if raw.len() < BLOCK_TOTAL {
            return Err(format!("bulk block too short: {} B", raw.len()));
        }
        let magic = u32::from_le_bytes(raw[0..4].try_into().unwrap());
        if magic != BLOCK_MAGIC {
            return Err(format!("bulk block bad magic: {magic:#010x}"));
        }
        let h = BlockHeader {
            seq: u16::from_le_bytes(raw[4..6].try_into().unwrap()),
            flags: u16::from_le_bytes(raw[6..8].try_into().unwrap()),
            first_sample_count: u64::from_le_bytes(raw[8..16].try_into().unwrap()),
            n_frames: u16::from_le_bytes(raw[16..18].try_into().unwrap()),
        };
        let payload_len = h.n_frames as usize * frame_size;
        if BLOCK_HEADER_SIZE + payload_len > BLOCK_TOTAL {
            return Err(format!("bulk block n_frames {} overruns block", h.n_frames));
        }
        Ok((h, &raw[BLOCK_HEADER_SIZE..BLOCK_HEADER_SIZE + payload_len]))
    }
}

/// One pen-trigger event: the device's per-channel sample index latched (in the
/// acquisition clock domain) at the trigger edge, plus which pen fired. The
/// `sample_count` shares the same monotonic axis as the streamed data, so it can
/// be mapped to a frame position without any host↔device clock conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerEvent {
    pub sample_count: u64,
    /// Logical pen: 0 = PB4, 1 = PB5.
    pub pen: u8,
}

/// Parsed GET_TRIGGER_EVENTS response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerEvents {
    /// Events lost to device-FIFO overflow since the last drain (should be 0).
    pub dropped: u32,
    pub events: Vec<TriggerEvent>,
}

impl TriggerEvents {
    /// Parse the response (little-endian, packed): `count, dropped, events[]`.
    pub fn parse(raw: &[u8]) -> Result<Self, String> {
        if raw.len() < 8 {
            return Err(format!("trigger events too short: {} < 8 bytes", raw.len()));
        }
        let count = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        let dropped = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        // Clamp to what the wire format and buffer can actually hold.
        let count = count.min(TRIG_MAX_EVENTS).min((raw.len() - 8) / TRIG_EVENT_SIZE);
        let mut events = Vec::with_capacity(count);
        for i in 0..count {
            let off = 8 + i * TRIG_EVENT_SIZE;
            let sample_count = u64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
            events.push(TriggerEvent {
                sample_count,
                pen: raw[off + 8],
            });
        }
        Ok(TriggerEvents { dropped, events })
    }
}

/// One device-clock observation: the device's free-running per-channel sample
/// counter read at a host-monotonic instant bracketed by the control transfer.
/// `host_mono_mid_ns` is the round-trip midpoint (best estimate of when the
/// device sampled `sample_count`); `rtt_ns` is the round-trip span, a quality
/// metric for filtering jittery observations before feeding the clock PLL.
#[derive(Debug, Clone, Copy)]
pub struct DeviceTimeObs {
    pub sample_count: u64,
    pub host_mono_mid_ns: i64,
    pub rtt_ns: i64,
}

/// Firmware state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Idle,
    Stop,
    Running,
    Fault,
    Unknown(u8),
}

impl DeviceState {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => DeviceState::Idle,
            1 => DeviceState::Stop,
            2 => DeviceState::Running,
            3 => DeviceState::Fault,
            other => DeviceState::Unknown(other),
        }
    }
}

/// Parsed device capability (Bulk and ISO share this layout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub version: u16,
    pub sample_rate_min: u32,
    pub sample_rate_max: u32,
    pub channel_mask_avail: u16,
    pub samplebits_list: Vec<u8>,
    pub current_sample_rate: u32,
    pub current_channel_mask: u16,
    pub current_samplebits: u8,
    pub state: DeviceState,
    pub packet_size: u16,
    pub block_data_size: u16,
}

impl Capability {
    /// Parse the 29-byte capability structure (little-endian, packed).
    pub fn parse(raw: &[u8]) -> Result<Self, String> {
        if raw.len() < CAP_SIZE {
            return Err(format!(
                "capability too short: expected {CAP_SIZE} bytes, got {}",
                raw.len()
            ));
        }
        let u16_at = |o: usize| u16::from_le_bytes([raw[o], raw[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);

        let samplebits_count = raw[16] as usize;
        let samplebits_list: Vec<u8> = raw[12..16]
            .iter()
            .take(samplebits_count.min(4))
            .copied()
            .filter(|&b| b > 0)
            .collect();

        Ok(Capability {
            version: u16_at(0),
            sample_rate_min: u32_at(2),
            sample_rate_max: u32_at(6),
            channel_mask_avail: u16_at(10),
            samplebits_list,
            current_sample_rate: u32_at(17),
            current_channel_mask: u16_at(21),
            current_samplebits: raw[23],
            state: DeviceState::from_u8(raw[24]),
            packet_size: u16_at(25),
            block_data_size: u16_at(27),
        })
    }

    /// Number of active channels (popcount of the current channel mask).
    #[allow(dead_code)]
    pub fn num_channels(&self) -> u32 {
        self.current_channel_mask.count_ones()
    }
}

/// ADC configuration sent to the firmware (`<IHBx` = 8 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdcConfig {
    pub sample_rate: u32,
    pub channel_mask: u16,
    pub samplebits: u8,
}

impl AdcConfig {
    /// Encode to the 8-byte wire layout (last byte reserved/zero).
    pub fn encode(&self) -> [u8; CFG_SIZE] {
        let mut buf = [0u8; CFG_SIZE];
        buf[0..4].copy_from_slice(&self.sample_rate.to_le_bytes());
        buf[4..6].copy_from_slice(&self.channel_mask.to_le_bytes());
        buf[6] = self.samplebits;
        // buf[7] reserved = 0
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 29-byte capability buffer for tests.
    fn cap_bytes() -> Vec<u8> {
        let mut b = vec![0u8; CAP_SIZE];
        b[0..2].copy_from_slice(&1u16.to_le_bytes()); // version
        b[2..6].copy_from_slice(&1000u32.to_le_bytes()); // sr_min
        b[6..10].copy_from_slice(&192000u32.to_le_bytes()); // sr_max
        b[10..12].copy_from_slice(&0x00FFu16.to_le_bytes()); // ch_mask_avail (8 ch)
        b[12..16].copy_from_slice(&[8, 12, 16, 0]); // samplebits_list raw
        b[16] = 3; // samplebits_count
        b[17..21].copy_from_slice(&16000u32.to_le_bytes()); // cur_sr
        b[21..23].copy_from_slice(&0x001Fu16.to_le_bytes()); // cur_ch_mask (5 ch)
        b[23] = 12; // cur_samplebits
        b[24] = 1; // state = STOP
        b[25..27].copy_from_slice(&240u16.to_le_bytes()); // pkt_size
        b[27..29].copy_from_slice(&2400u16.to_le_bytes()); // block_size
        b
    }

    #[test]
    fn capability_parses_all_fields() {
        let cap = Capability::parse(&cap_bytes()).unwrap();
        assert_eq!(cap.version, 1);
        assert_eq!(cap.sample_rate_min, 1000);
        assert_eq!(cap.sample_rate_max, 192000);
        assert_eq!(cap.channel_mask_avail, 0x00FF);
        assert_eq!(cap.samplebits_list, vec![8, 12, 16]);
        assert_eq!(cap.current_sample_rate, 16000);
        assert_eq!(cap.current_channel_mask, 0x001F);
        assert_eq!(cap.num_channels(), 5);
        assert_eq!(cap.current_samplebits, 12);
        assert_eq!(cap.state, DeviceState::Stop);
        assert_eq!(cap.packet_size, 240);
        assert_eq!(cap.block_data_size, 2400);
    }

    #[test]
    fn capability_rejects_short_buffer() {
        assert!(Capability::parse(&[0u8; 10]).is_err());
    }

    #[test]
    fn config_encodes_little_endian_packed() {
        let cfg = AdcConfig {
            sample_rate: 16000,
            channel_mask: 0x001F,
            samplebits: 12,
        };
        let enc = cfg.encode();
        assert_eq!(enc.len(), 8);
        assert_eq!(&enc[0..4], &16000u32.to_le_bytes());
        assert_eq!(&enc[4..6], &0x001Fu16.to_le_bytes());
        assert_eq!(enc[6], 12);
        assert_eq!(enc[7], 0);
    }

    #[test]
    fn trigger_events_parse() {
        let mut b = vec![0u8; TRIG_EVENTS_SIZE];
        b[0..4].copy_from_slice(&2u32.to_le_bytes()); // count
        b[4..8].copy_from_slice(&0u32.to_le_bytes()); // dropped
        // event 0: pen 0 @ sample 123456
        b[8..16].copy_from_slice(&123456u64.to_le_bytes());
        b[16] = 0;
        // event 1: pen 1 @ sample 9_000_000_000 (>u32, proves u64)
        b[24..32].copy_from_slice(&9_000_000_000u64.to_le_bytes());
        b[32] = 1;
        let ev = TriggerEvents::parse(&b).unwrap();
        assert_eq!(ev.dropped, 0);
        assert_eq!(ev.events.len(), 2);
        assert_eq!(ev.events[0], TriggerEvent { sample_count: 123456, pen: 0 });
        assert_eq!(ev.events[1], TriggerEvent { sample_count: 9_000_000_000, pen: 1 });
    }

    #[test]
    fn trigger_events_clamps_bogus_count() {
        let mut b = vec![0u8; TRIG_EVENTS_SIZE];
        b[0..4].copy_from_slice(&9999u32.to_le_bytes()); // absurd count
        let ev = TriggerEvents::parse(&b).unwrap();
        assert_eq!(ev.events.len(), TRIG_MAX_EVENTS, "count clamped to capacity");
    }

    #[test]
    fn trigger_events_rejects_short() {
        assert!(TriggerEvents::parse(&[0u8; 4]).is_err());
    }

    #[test]
    fn block_header_parse_roundtrip() {
        let mut b = vec![0u8; BLOCK_TOTAL];
        b[0..4].copy_from_slice(&BLOCK_MAGIC.to_le_bytes());
        b[4..6].copy_from_slice(&7u16.to_le_bytes());
        b[6..8].copy_from_slice(&BLOCK_FLAG_NEW_SESSION.to_le_bytes());
        b[8..16].copy_from_slice(&9_000_000_123u64.to_le_bytes());
        b[16..18].copy_from_slice(&48u16.to_le_bytes()); // n_frames
        let (h, payload) = BlockHeader::parse(&b, 10).unwrap();
        assert_eq!(h.seq, 7);
        assert_eq!(h.flags & BLOCK_FLAG_NEW_SESSION, BLOCK_FLAG_NEW_SESSION);
        assert_eq!(h.first_sample_count, 9_000_000_123);
        assert_eq!(h.n_frames, 48);
        assert_eq!(payload.len(), 480); // padding excluded
    }

    #[test]
    fn block_header_rejects_bad_magic_short_and_overrun() {
        assert!(BlockHeader::parse(&[0u8; 64], 10).is_err()); // short
        let mut b = vec![0u8; BLOCK_TOTAL];
        assert!(BlockHeader::parse(&b, 10).is_err()); // magic=0
        b[0..4].copy_from_slice(&BLOCK_MAGIC.to_le_bytes());
        b[16..18].copy_from_slice(&49u16.to_le_bytes()); // 49×10+24 > 512
        assert!(BlockHeader::parse(&b, 10).is_err()); // n_frames overrun
    }

    #[test]
    fn device_state_mapping() {
        assert_eq!(DeviceState::from_u8(0), DeviceState::Idle);
        assert_eq!(DeviceState::from_u8(2), DeviceState::Running);
        assert_eq!(DeviceState::from_u8(3), DeviceState::Fault);
        assert_eq!(DeviceState::from_u8(9), DeviceState::Unknown(9));
    }
}
