//! Per-frame metadata and the `process()` data/return types.
//!
//! Every data frame flowing through the graph carries a [`FrameHeader`] so
//! downstream nodes can independently reconstruct absolute sample timestamps
//! (self-anchored frames). A drop shows up as a jump in `sample_index`, which
//! is both detectable and non-corrupting because each frame re-anchors via its
//! own `sample_index` + `t0_ns`.

use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

/// `flags` bit set when a gap precedes this frame (samples were lost or were
/// forward-filled). `gap_samples` then says how many.
pub const FLAG_DISCONTINUITY: u32 = 1 << 0;

/// `flags` bit for trigger/event marker frames whose `sample_index` is on the
/// *same per-channel sample axis as the paired signal stream* (rather than a
/// marker sequence number). A consumer aligning the event to the stream can
/// then use `sample_index` directly and skip the time→index conversion —
/// avoiding the long-lever error of mapping `t0_ns` back through its own
/// anchor + inferred rate (which grows with anchor age and jumps when the
/// source's recovered clock rate updates).
pub const FLAG_TRIG_INDEXED: u32 = 1 << 1;

/// `flags` bit set when this frame carries a structured **annotation** trailing
/// the sample payload. The payload buffer is then laid out as
/// `[sample bytes … | annotation blob]`, where the blob occupies the last
/// [`FrameHeader::annotation_len`] bytes and is itself
/// `[annotation_schema_id: u64 LE | annotation bytes]` (see [`parse_annotation`]).
///
/// The annotation travels *with* the data frame but is opaque to nodes that do
/// not consume it: samples stay at offset 0 so sample-only consumers read them
/// unchanged, pure pass-through nodes copy the whole buffer verbatim, and sinks
/// persist the blob. This is how a node attaches metadata to data (e.g. a touch
/// coordinate paired to a vibration capture) without corrupting the sample
/// stream. Use [`Frame::samples`]/[`Frame::annotation`] to read and
/// [`FrameOut::set_annotation`] to attach.
pub const FLAG_ANNOTATED: u32 = 1 << 2;

/// Parse an annotation blob (the trailing region of an annotated frame's
/// payload) into its `(schema_id, bytes)`. Returns `None` if the blob is too
/// short to hold the 8-byte schema id.
pub fn parse_annotation(blob: &[u8]) -> Option<(u64, &[u8])> {
    if blob.len() < 8 {
        return None;
    }
    let schema_id = u64::from_le_bytes(blob[..8].try_into().unwrap());
    Some((schema_id, &blob[8..]))
}

/// Capture-window annotation — part of the **capture convention**.
///
/// A windowed capture source (trigger-capture and friends) annotates the
/// **first chunk** of every capture with the window's self-describing
/// metadata, so downstream reassemblers complete windows without a
/// duplicated `window_samples` config and trigger consumers read the event
/// row from the source of truth instead of re-detecting it:
///
///   `[CAPTURE_WINDOW_ANNOTATION_SCHEMA | {"window_rows":N,"trig_row":K,"fs_hz":F}]`
///
/// `window_rows` = per-channel rows of the whole capture (after any partial
/// front truncation); `trig_row` = the trigger event's row within the
/// emitted window (shifts accordingly on partial captures); `fs_hz`
/// (optional) = the stream's declared sample rate on the window's t0 axis —
/// the capture node's authoritative value (its upstream declaration, else
/// its own dense-stream inference), self-consistent with the t0s it stamps.
/// Generic stream nodes must pass the annotation through opaquely
/// (first-chunk annotation rides on the reassembled whole-window frame).
pub const CAPTURE_WINDOW_ANNOTATION_SCHEMA: u64 = 0x7366_6361_7077_6E31; // "sfcapwn1"

/// Stream-rate annotation — in-band sample-rate declaration by a stream's
/// **source** (the clock authority; e.g. sdc-usb's frozen clock map).
/// Attached to every data frame of the declared stream:
///
///   `[RATE_ANNOTATION_SCHEMA | {"fs_hz":F}]`
///
/// `fs_hz` = samples per second **on the stream's own t0 axis** (whatever
/// clock the source stamps t0 with). Consumers prefer this over header
/// inference over any prior parameter — the source alone knows which clock
/// to trust (see the fs resolution order in window_filtfilt/knock_dispcomp).
pub const RATE_ANNOTATION_SCHEMA: u64 = 0x7366_7261_7465_3031; // "sfrate01"

/// 解析 JSON 体里 `key` 后面的数值（整数或浮点；容忍字段顺序/空白——
/// 类型层不引 JSON 依赖的最小解析）。
fn json_number_after(s: &str, key: &str) -> Option<f64> {
    let at = s.find(key)? + key.len();
    s[at..]
        .trim_start_matches([':', ' ', '\t'])
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
        .collect::<String>()
        .parse()
        .ok()
}

/// Encode the capture-window annotation body (JSON). `fs_hz` omitted when
/// the capture node has no rate to declare yet.
pub fn encode_capture_window_annotation(window_rows: u64, trig_row: u64, fs_hz: Option<f64>) -> Vec<u8> {
    match fs_hz {
        Some(fs) => format!(
            "{{\"window_rows\":{window_rows},\"trig_row\":{trig_row},\"fs_hz\":{fs}}}"
        )
        .into_bytes(),
        None => format!("{{\"window_rows\":{window_rows},\"trig_row\":{trig_row}}}").into_bytes(),
    }
}

/// Decode a capture-window annotation body → (window_rows, trig_row, fs_hz).
/// `fs_hz` is `None` for bodies from older encoders (field optional).
pub fn decode_capture_window_annotation(bytes: &[u8]) -> Option<(u64, u64, Option<f64>)> {
    let s = std::str::from_utf8(bytes).ok()?;
    let rows = json_number_after(s, "\"window_rows\"")? as u64;
    let trig = json_number_after(s, "\"trig_row\"")? as u64;
    let fs = json_number_after(s, "\"fs_hz\"").filter(|f| f.is_finite() && *f > 0.0);
    Some((rows, trig, fs))
}

/// Encode the stream-rate annotation body (JSON).
pub fn encode_rate_annotation(fs_hz: f64) -> Vec<u8> {
    format!("{{\"fs_hz\":{fs_hz}}}").into_bytes()
}

/// Decode a stream-rate annotation body → fs_hz.
pub fn decode_rate_annotation(bytes: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(bytes).ok()?;
    json_number_after(s, "\"fs_hz\"").filter(|f| f.is_finite() && *f > 0.0)
}

/// Self-anchoring per-frame metadata.
///
/// `#[repr(C)]` with fields ordered largest-first; the layout is 56 bytes
/// (five `u64`/`i64` = 40, three `u32` = 12, plus 4 bytes of `repr(C)` tail
/// padding to the 8-byte alignment). It is carried verbatim in the iceoryx2
/// user-header slot (via a wrapper in the dataplane crate) and serialized into
/// JSON control messages for the process runtime.
///
/// Two kinds of field live here: **source-authored** time/sequencing
/// (`seq`/`sample_index`/`t0_ns`/`gap_samples`/`n_samples`/`flags`) and one
/// **shell-measured** transport field (`tx_egress_mono_ns`). The shell
/// overwrites the latter on every hop just before the frame leaves a node;
/// downstream diffs it against its own ingress clock to get per-hop latency.
/// They share one struct for now (pragmatic); splitting source vs transport
/// metadata into a separate user-header section is a future refactor.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct FrameHeader {
    /// Monotonic frame counter since stream start.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub seq: u64,
    /// Absolute index of this frame's first sample since stream start.
    /// A jump relative to `prev.sample_index + prev.n_samples` means samples
    /// were lost — the key to drop detection.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub sample_index: u64,
    /// Monotonic-clock timestamp (ns) of this frame's first sample.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub t0_ns: i64,
    /// `CLOCK_MONOTONIC` ns when the frame last left a node (its egress).
    /// Shell-stamped per hop, overwritten each node. A downstream node computes
    /// per-hop (transport) latency as `ingress_now − tx_egress_mono_ns`. 0 means
    /// not yet stamped (e.g. a freshly built header before publish).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub tx_egress_mono_ns: i64,
    /// When `FLAG_DISCONTINUITY` is set: number of samples missing before this
    /// frame (whether dropped or forward-filled).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub gap_samples: u64,
    /// Samples per channel in this frame.
    pub n_samples: u32,
    /// Bit flags; bit0 = [`FLAG_DISCONTINUITY`], bit1 = [`FLAG_TRIG_INDEXED`],
    /// bit2 = [`FLAG_ANNOTATED`].
    pub flags: u32,
    /// When [`FLAG_ANNOTATED`] is set: byte length of the annotation blob that
    /// trails the sample payload (the blob is the last `annotation_len` bytes of
    /// the payload buffer). 0 when the frame carries no annotation.
    pub annotation_len: u32,
}

impl FrameHeader {
    /// All-zero header (seq 0, sample 0, t0 0, no gap, no samples).
    pub const ZERO: FrameHeader = FrameHeader {
        seq: 0,
        sample_index: 0,
        t0_ns: 0,
        tx_egress_mono_ns: 0,
        gap_samples: 0,
        n_samples: 0,
        flags: 0,
        annotation_len: 0,
    };

    /// Whether the discontinuity flag is set.
    pub fn is_discontinuity(&self) -> bool {
        self.flags & FLAG_DISCONTINUITY != 0
    }

    /// Set or clear the discontinuity flag and record the gap size.
    pub fn set_discontinuity(&mut self, gap_samples: u64) {
        self.flags |= FLAG_DISCONTINUITY;
        self.gap_samples = gap_samples;
    }

    /// Whether `sample_index` is on the paired signal stream's sample axis
    /// (a stream-indexed trigger marker).
    pub fn is_trig_indexed(&self) -> bool {
        self.flags & FLAG_TRIG_INDEXED != 0
    }

    /// Mark this header as a stream-indexed trigger marker.
    pub fn set_trig_indexed(&mut self) {
        self.flags |= FLAG_TRIG_INDEXED;
    }

    /// Whether this frame carries a trailing annotation blob.
    pub fn is_annotated(&self) -> bool {
        self.flags & FLAG_ANNOTATED != 0
    }

    /// Split a payload buffer into `(samples, annotation_blob)` using this
    /// header's `annotation_len`. The annotation blob is the trailing
    /// `annotation_len` bytes; `None` when the frame is not annotated. If the
    /// recorded `annotation_len` exceeds the buffer (corrupt/truncated), the
    /// whole buffer is returned as samples with no annotation.
    pub fn split_payload<'a>(&self, payload: &'a [u8]) -> (&'a [u8], Option<&'a [u8]>) {
        let n = self.annotation_len as usize;
        if !self.is_annotated() || n == 0 || n > payload.len() {
            return (payload, None);
        }
        let cut = payload.len() - n;
        (&payload[..cut], Some(&payload[cut..]))
    }
}

impl Default for FrameHeader {
    fn default() -> Self {
        Self::ZERO
    }
}

/// Result of a `process()` (or `start()`) call.
///
/// Plugins report `Ok` / `Fault` via their normal control response; `Dead` is
/// constructed shell-side when the IPC itself breaks (EOF / crash / timeout)
/// and a plugin can no longer answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ProcessOutcome {
    /// Healthy, even if no output was produced this call.
    Ok,
    /// Plugin-reported logical fault (e.g. device FAULT, disconnect, error
    /// rate over threshold). Recoverable via restart.
    Fault { reason: String },
    /// IPC broke — the plugin is gone or unresponsive. Shell-constructed.
    Dead { reason: String },
}

/// One input frame handed to `process()`: metadata plus borrowed payload.
pub struct Frame<'a> {
    pub header: FrameHeader,
    pub data: &'a [u8],
    /// Total latency (ns) of this frame at this node's ingress: `now − t0_ns`,
    /// i.e. how old the data is when the plugin receives it. Shell-stamped (not
    /// on the wire); plugins that align/buffer by time (e.g. trigger-capture)
    /// read it to size their buffers. 0 if the shell did not stamp it.
    pub latency_ns: i64,
}

impl<'a> Frame<'a> {
    /// Build a frame with no measured latency (`latency_ns = 0`). Callers that
    /// have an ingress latency set the field directly afterward.
    pub fn new(header: FrameHeader, data: &'a [u8]) -> Self {
        Frame {
            header,
            data,
            latency_ns: 0,
        }
    }

    /// The sample bytes only — the payload minus any annotation trailer.
    /// Identical to `data` for non-annotated frames. Sample-consuming nodes
    /// should read this rather than `data` so an attached annotation never
    /// leaks into the sample stream.
    pub fn samples(&self) -> &[u8] {
        self.header.split_payload(self.data).0
    }

    /// The raw annotation blob (`[schema_id: u64 LE | bytes]`) if this frame is
    /// annotated, else `None`. Pair with [`parse_annotation`] to decode.
    pub fn annotation(&self) -> Option<&[u8]> {
        self.header.split_payload(self.data).1
    }
}

/// One output slot for `process()`.
///
/// The plugin fills [`FrameOut::header`], writes samples into the buffer
/// (via [`FrameOut::write`] or [`FrameOut::buffer_mut`] + [`FrameOut::set_written`]),
/// and the shell publishes exactly `written()` bytes with that header.
pub struct FrameOut<'a> {
    pub header: FrameHeader,
    buf: &'a mut [u8],
    written: usize,
}

impl<'a> FrameOut<'a> {
    /// Wrap a writable buffer. Header starts at [`FrameHeader::ZERO`], written 0.
    pub fn new(buf: &'a mut [u8]) -> Self {
        FrameOut {
            header: FrameHeader::ZERO,
            buf,
            written: 0,
        }
    }

    /// Total capacity of the output buffer.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Mutable access to the raw buffer for in-place writes. Pair with
    /// [`FrameOut::set_written`].
    pub fn buffer_mut(&mut self) -> &mut [u8] {
        self.buf
    }

    /// Bytes the plugin has declared valid for publishing.
    pub fn written(&self) -> usize {
        self.written
    }

    /// Record how many bytes of the buffer are valid (clamped to capacity).
    pub fn set_written(&mut self, n: usize) {
        self.written = n.min(self.buf.len());
    }

    /// Copy `bytes` into the buffer (clamped to capacity) and set written len.
    /// Returns the number of bytes copied.
    pub fn write(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(self.buf.len());
        self.buf[..n].copy_from_slice(&bytes[..n]);
        self.written = n;
        n
    }

    /// Append a structured annotation after the sample bytes already written.
    ///
    /// Call this *after* the sample payload is in place (via [`write`] or
    /// [`set_written`]): it writes `[schema_id: u64 LE | bytes]` starting at the
    /// current `written` offset, extends `written` to cover the trailer, and
    /// sets [`FLAG_ANNOTATED`] + `annotation_len` on the header so the shell
    /// publishes the combined buffer and downstream can split it back out.
    ///
    /// Returns `false` (writing nothing) if the trailer would overflow the
    /// buffer's capacity.
    pub fn set_annotation(&mut self, schema_id: u64, bytes: &[u8]) -> bool {
        let trailer = 8 + bytes.len();
        if self.written + trailer > self.buf.len() {
            return false;
        }
        let off = self.written;
        self.buf[off..off + 8].copy_from_slice(&schema_id.to_le_bytes());
        self.buf[off + 8..off + 8 + bytes.len()].copy_from_slice(bytes);
        self.written += trailer;
        self.header.flags |= FLAG_ANNOTATED;
        self.header.annotation_len = trailer as u32;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_is_56_bytes() {
        // repr(C): 5x u64/i64 (40) + 3x u32 (12) + 4 tail pad = 56.
        assert_eq!(std::mem::size_of::<FrameHeader>(), 56);
    }

    #[test]
    fn non_annotated_frame_samples_equal_data() {
        let h = FrameHeader::ZERO;
        let data = [1u8, 2, 3, 4];
        let f = Frame::new(h, &data);
        assert!(!h.is_annotated());
        assert_eq!(f.samples(), &data);
        assert_eq!(f.annotation(), None);
    }

    #[test]
    fn set_annotation_roundtrips_through_split() {
        let mut buf = [0u8; 64];
        let samples = [10u8, 11, 12, 13, 14];
        let ann = [0xAAu8, 0xBB, 0xCC];
        let schema_id = 0x4243_4453_5544_4f54; // arbitrary

        let (header, written) = {
            let mut out = FrameOut::new(&mut buf);
            out.write(&samples);
            assert!(out.set_annotation(schema_id, &ann));
            (out.header, out.written())
        };

        assert!(header.is_annotated());
        assert_eq!(header.annotation_len as usize, 8 + ann.len());
        assert_eq!(written, samples.len() + 8 + ann.len());

        // Reconstruct the published payload and split it back out.
        let payload = &buf[..written];
        let f = Frame::new(header, payload);
        assert_eq!(f.samples(), &samples);
        let blob = f.annotation().expect("annotated");
        let (got_id, got_bytes) = parse_annotation(blob).expect("parses");
        assert_eq!(got_id, schema_id);
        assert_eq!(got_bytes, &ann);
    }

    #[test]
    fn set_annotation_empty_payload_and_empty_bytes() {
        let mut buf = [0u8; 16];
        let (header, written) = {
            let mut out = FrameOut::new(&mut buf);
            // no samples written
            assert!(out.set_annotation(7, &[]));
            (out.header, out.written())
        };
        assert_eq!(written, 8);
        let f = Frame::new(header, &buf[..written]);
        assert_eq!(f.samples(), b"");
        let (id, bytes) = parse_annotation(f.annotation().unwrap()).unwrap();
        assert_eq!(id, 7);
        assert_eq!(bytes, b"");
    }

    #[test]
    fn set_annotation_rejects_overflow_without_writing() {
        let mut buf = [0u8; 10];
        let mut out = FrameOut::new(&mut buf);
        out.write(&[1, 2, 3, 4, 5, 6]); // 6 used, 4 left, trailer needs >=8
        assert!(!out.set_annotation(1, &[]));
        assert!(!out.header.is_annotated());
        assert_eq!(out.written(), 6);
    }

    #[test]
    fn split_payload_tolerates_corrupt_len() {
        let mut h = FrameHeader::ZERO;
        h.flags |= FLAG_ANNOTATED;
        h.annotation_len = 99; // larger than buffer
        let data = [1u8, 2, 3];
        let (s, a) = h.split_payload(&data);
        assert_eq!(s, &data);
        assert_eq!(a, None);
    }

    #[test]
    fn parse_annotation_rejects_short_blob() {
        assert_eq!(parse_annotation(&[1, 2, 3]), None);
        assert_eq!(parse_annotation(&[]), None);
    }
}
