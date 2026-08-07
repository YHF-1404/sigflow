//! `record.json` v1 — the per-record manifest, the unit of truth for one
//! capture (a record = one trigger event = npy waveform + future artifacts).
//!
//! Contract highlights (all user-ratified design decisions):
//! - The manifest's *filename* carries the state (`record.json.recording` →
//!   `record.json`); the `state` field inside is redundant. On mismatch the
//!   filename wins.
//! - Unknown fields are preserved on round-trip (`#[serde(flatten)]` maps) —
//!   an old tool must never silently drop fields written by a newer one.
//! - `format_version` is `major.minor`: minor additions are read by skipping
//!   unknowns; an unknown major is refused outright.
//! - Fields like `state`, `clock`, `fs_source` and channel `role` are kept as
//!   raw strings (with typed accessors) so that values written by newer tools
//!   survive a read-modify-write by older ones.
//! - `provenance.session_id` is the *pointer-file snapshot at capture time* —
//!   evidence, not truth. Effective attribution is the four-layer resolved
//!   view (annotations can override it).
//! - `intrinsic.t0_ns` is the window start (the only basis for replay
//!   reconstruction); `provenance.trigger_t0_ns` is the trigger event time.
//!   With `trigger_pos < 1` they differ by `p·W/fs` — they are NOT aliases.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::CatalogError;

/// Version written by this crate.
pub const FORMAT_VERSION: &str = "1.0";
/// Major version this crate can read.
pub const FORMAT_MAJOR: u32 = 1;

/// On-disk manifest filenames.
pub const RECORD_MANIFEST: &str = "record.json";
pub const SESSION_MANIFEST: &str = "session.json";
/// Suffix marking an in-progress manifest: `record.json.recording`.
pub const RECORDING_SUFFIX: &str = ".recording";
/// Suffix for atomic-write temporaries; residue is deleted by reindex.
pub const TMP_SUFFIX: &str = ".tmp";

// Raw string values for `state` (kept as strings for forward compatibility).
pub const STATE_RECORDING: &str = "recording";
pub const STATE_COMPLETE: &str = "complete";
pub const STATE_SALVAGED: &str = "salvaged";

// `intrinsic.clock` values.
pub const CLOCK_MONOTONIC: &str = "monotonic";
// `intrinsic.fs_source` values.
pub const FS_SOURCE_ESTIMATED: &str = "estimated";
/// No fs estimate was available (e.g. a single-frame capture) — `fs_hz` is
/// 0.0 and must not be presented as an estimate.
pub const FS_SOURCE_UNKNOWN: &str = "unknown";
// Channel roles.
pub const ROLE_SIGNAL: &str = "signal";
pub const ROLE_TRIGGER: &str = "trigger";

/// Typed view of the raw `state` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordState {
    Recording,
    Complete,
    Salvaged,
    /// A state written by a newer tool; readers must not destroy it.
    Unknown,
}

/// Typed view of a channel's raw `role` string (replay strips Trigger).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRole {
    Signal,
    Trigger,
    Unknown,
}

/// Typed view of the raw `intrinsic.clock` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockBase {
    Monotonic,
    Unknown,
}

/// Typed view of the raw `intrinsic.fs_source` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsSource {
    Estimated,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordManifest {
    pub format_version: String,
    /// ULID; also the record directory name. Generated at the capture's
    /// first frame, so the ULID time bits approximate the trigger time.
    pub id: String,
    /// Redundant copy of the filename-carried state (`recording` /
    /// `complete` / `salvaged`). Filename wins on mismatch.
    pub state: String,
    pub provenance: Provenance,
    pub intrinsic: Intrinsic,
    pub artifacts: Vec<Artifact>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Who produced this record, where, and within which declared experiment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Provenance {
    /// Session pointer snapshot at capture time; `null` is a safe fallback
    /// (attribution is repairable via annotations, a wrong value is not).
    pub session_id: Option<String>,
    /// Graph name, when the sink knows it (used to detect pointer mismatch).
    pub graph: Option<String>,
    pub node_id: String,
    /// Plugin identity, e.g. `sigflow.io.storage@0.4.0`.
    pub plugin: String,
    pub host: String,
    /// In-process capture counter. Resets on restart: evidence, not identity.
    pub capture_seq: u64,
    /// Stream label (successor of the legacy `prefix` filename component).
    pub stream_label: String,
    /// Trigger event time (ns, `intrinsic.clock` time base), when known.
    /// Differs from `intrinsic.t0_ns` (window start) by `trigger_pos·W/fs`.
    pub trigger_t0_ns: Option<i64>,
    /// RFC 3339 UTC wall-clock anchor (t0_ns is monotonic-only).
    pub created_utc: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Machine-extracted facts about the captured data. Immutable once the
/// record is complete; re-derivable from the data in principle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Intrinsic {
    /// Absolute time of the first sample row (window start) — the only basis
    /// replay uses to reconstruct frame spacing.
    pub t0_ns: i64,
    /// Time base of `t0_ns` / `trigger_t0_ns`, e.g. `monotonic`.
    pub clock: String,
    /// Per-channel sample rate. May be 0.0 when no estimate was available.
    pub fs_hz: f64,
    /// Provenance of `fs_hz`, e.g. `estimated` (Δsample_index/Δt0 — not
    /// authoritative).
    pub fs_source: String,
    /// One entry per interleaved column. Replay strips `role == "trigger"`
    /// columns; a bare channel count is not enough to rebuild frames.
    pub channels: Vec<ChannelDesc>,
    /// Total sample rows. Redundant with the npy shape — allows verification
    /// without opening the artifact.
    pub n_samples: u64,
    /// Absolute sample index of the first row in the source stream.
    pub sample_index: u64,
    /// Samples known missing before this window (discontinuity bookkeeping).
    pub gap_samples: u64,
    /// Capture-time frame annotations (e.g. the touch-merge JSON). An array
    /// from day one so multi-schema needs no shape change; today it holds at
    /// most one entry. Analysis-time labels do NOT go here — they live in
    /// `annotations/` (the mutable human layer).
    pub annotations: Vec<AnnotationEntry>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelDesc {
    /// Column index in the interleaved layout.
    pub idx: u32,
    /// `signal` | `trigger` (raw string; see role constants).
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ChannelDesc {
    pub fn role(&self) -> ChannelRole {
        match self.role.as_str() {
            ROLE_SIGNAL => ChannelRole::Signal,
            ROLE_TRIGGER => ChannelRole::Trigger,
            _ => ChannelRole::Unknown,
        }
    }
}

impl Intrinsic {
    pub fn clock(&self) -> ClockBase {
        match self.clock.as_str() {
            CLOCK_MONOTONIC => ClockBase::Monotonic,
            _ => ClockBase::Unknown,
        }
    }

    pub fn fs_source(&self) -> FsSource {
        match self.fs_source.as_str() {
            FS_SOURCE_ESTIMATED => FsSource::Estimated,
            _ => FsSource::Unknown,
        }
    }
}

/// A frame annotation captured with the data: `schema_id` is the in-band
/// `[schema_id u64 | bytes]` discriminator; `data` is the blob parsed as JSON
/// when it is valid UTF-8 JSON, else a lowercase-hex string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnnotationEntry {
    pub schema_id: u64,
    pub data: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Artifact {
    /// Semantic role, e.g. `waveform`.
    pub role: String,
    /// Path relative to the record directory (plain filename in v1).
    pub path: String,
    /// Byte-layout self-description, e.g. `npy/f32-le/C/(n,ch)`.
    pub format: String,
    pub size_bytes: u64,
    /// Lowercase-hex blake3 of the file. Optional only while `recording`
    /// (optional-until-complete); required on `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl AnnotationEntry {
    /// Build from an in-band annotation blob, mirroring the sink's existing
    /// behaviour: valid UTF-8 JSON is nested verbatim, anything else becomes
    /// a lowercase-hex string.
    pub fn from_blob(schema_id: u64, bytes: &[u8]) -> Self {
        let data = match std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
        {
            Some(v) => v,
            None => Value::String(hex(bytes)),
        };
        AnnotationEntry { schema_id, data, extra: Map::new() }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Parse `major.minor` out of a `format_version` string.
pub fn parse_format_version(v: &str) -> Option<(u32, u32)> {
    let (maj, min) = v.split_once('.')?;
    Some((maj.parse().ok()?, min.parse().ok()?))
}

/// Probe `format_version` alone and refuse unknown majors before any typed
/// parse — a newer major may have restructured fields, and that must not be
/// reported as JSON damage.
fn gate_version(bytes: &[u8]) -> Result<(), CatalogError> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        format_version: String,
    }
    let p: Probe = serde_json::from_slice(bytes)?;
    match parse_format_version(&p.format_version) {
        Some((maj, _)) if maj == FORMAT_MAJOR => Ok(()),
        _ => Err(CatalogError::Version { found: p.format_version }),
    }
}

impl RecordManifest {
    /// Typed view of the raw state string.
    pub fn state(&self) -> RecordState {
        match self.state.as_str() {
            STATE_RECORDING => RecordState::Recording,
            STATE_COMPLETE => RecordState::Complete,
            STATE_SALVAGED => RecordState::Salvaged,
            _ => RecordState::Unknown,
        }
    }

    /// Deserialize, refusing an unknown major version (read tolerance covers
    /// minor additions only — see module docs). The version gate runs on a
    /// minimal probe *before* the typed parse, so a newer major whose
    /// structure changed reports `Version` ("upgrade the tool"), never
    /// `Json` ("bit rot") — the two alerts demand different operator actions.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, CatalogError> {
        gate_version(bytes)?;
        Ok(serde_json::from_slice(bytes)?)
    }

    /// Serialize as pretty JSON with a trailing newline (the on-disk form).
    pub fn to_json_vec(&self) -> Result<Vec<u8>, CatalogError> {
        let mut v = serde_json::to_vec_pretty(self)?;
        v.push(b'\n');
        Ok(v)
    }

    /// Validate the invariants a *final* (complete/salvaged) manifest must
    /// satisfy before it is committed by the rename. Collects every problem.
    pub fn validate_complete(&self) -> Result<(), CatalogError> {
        let mut problems = Vec::new();
        if !crate::id::is_valid_ulid(&self.id) {
            problems.push(format!("id '{}' is not a ULID", self.id));
        }
        match self.state() {
            RecordState::Complete | RecordState::Salvaged => {}
            _ => problems.push(format!("state '{}' is not complete/salvaged", self.state)),
        }
        if self.intrinsic.clock.is_empty() {
            problems.push("intrinsic.clock is empty".into());
        }
        if self.intrinsic.channels.is_empty() {
            problems.push("intrinsic.channels is empty".into());
        }
        if self.provenance.created_utc.is_empty() {
            problems.push("provenance.created_utc is empty".into());
        }
        if self.artifacts.is_empty() {
            problems.push("complete record has no artifacts".into());
        }
        for a in &self.artifacts {
            if a.path.is_empty()
                || a.path.starts_with('/')
                || a.path.split('/').any(|c| c == "..")
            {
                problems.push(format!("artifact path '{}' is not a safe relative path", a.path));
            } else if a.path.contains('/') {
                // v1 artifacts sit flat in the record directory; the
                // scanner's undeclared-file detection relies on it.
                problems.push(format!("artifact path '{}' must be a flat filename in v1", a.path));
            }
            if a.format.is_empty() {
                problems.push(format!("artifact '{}' has empty format", a.path));
            }
            match &a.blake3 {
                Some(h) if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) => {}
                Some(h) => problems.push(format!("artifact '{}' blake3 '{h}' malformed", a.path)),
                None => problems.push(format!("artifact '{}' missing blake3", a.path)),
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(CatalogError::Invalid(problems.join("; ")))
        }
    }
}

/// The session manifest — a record without artifacts, living in the same
/// `records/<shard>/<ulid>/` layout as `session.json` so that sharding,
/// rsync and reindex are all reused. Written at `session begin` *before* the
/// pointer file is renamed into place (manifest-before-pointer discipline).
///
/// Minimal v1: identity + capture-time declared attributes. `session end`
/// deliberately writes no summary into it — the manifest is immutable once
/// written, and any end-time summary would be a convenience cache, not
/// truth (the boundary IS the pointer's removal / the next begin).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionManifest {
    pub format_version: String,
    /// ULID; also the session directory name.
    pub id: String,
    pub started_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<String>,
    pub host: String,
    /// Globally shared declared attributes (subject, device, …).
    #[serde(default)]
    pub attrs: Map<String, Value>,
    /// Per-sink declared attributes (e.g. `rec0 → {pen: "A"}`). Internal
    /// structure of resolution layer 4 (session-intrinsic): expanded per
    /// sink, then merged key-wise — the outside view stays four layers.
    #[serde(default)]
    pub sink_attrs: Map<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SessionManifest {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, CatalogError> {
        gate_version(bytes)?;
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn to_json_vec(&self) -> Result<Vec<u8>, CatalogError> {
        let mut v = serde_json::to_vec_pretty(self)?;
        v.push(b'\n');
        Ok(v)
    }
}

/// Test fixtures shared across modules (real npy bytes + a manifest whose
/// artifact entry actually matches them, so hash verification is exercised
/// end to end).
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// A valid NumPy v1.0 `<f4` C-order file of the given shape.
    pub fn npy_bytes(rows: usize, cols: usize) -> Vec<u8> {
        let dict =
            format!("{{'descr': '<f4', 'fortran_order': False, 'shape': ({rows}, {cols}), }}");
        let base = 10 + dict.len() + 1;
        let pad = (64 - (base % 64)) % 64;
        let hlen = dict.len() + 1 + pad;
        let mut out = Vec::with_capacity(10 + hlen + rows * cols * 4);
        out.extend_from_slice(b"\x93NUMPY");
        out.push(1);
        out.push(0);
        out.extend_from_slice(&(hlen as u16).to_le_bytes());
        out.extend_from_slice(dict.as_bytes());
        out.extend(std::iter::repeat_n(b' ', pad));
        out.push(b'\n');
        for i in 0..(rows * cols) {
            out.extend_from_slice(&(i as f32).to_le_bytes());
        }
        out
    }

    /// A `complete` manifest whose single artifact entry matches `bytes`.
    pub fn complete_manifest(path: &str, bytes: &[u8]) -> RecordManifest {
        RecordManifest {
            format_version: FORMAT_VERSION.to_string(),
            id: "01JXAB3C4D5E6F7G8H9JKMNPQR".to_string(),
            state: STATE_COMPLETE.to_string(),
            provenance: Provenance {
                session_id: None,
                graph: None,
                node_id: "rec0".into(),
                plugin: "sigflow.io.storage@0.4.0".into(),
                host: "test".into(),
                capture_seq: 0,
                stream_label: "pen0".into(),
                trigger_t0_ns: None,
                created_utc: "2026-06-10T08:30:12.345Z".into(),
                extra: Map::new(),
            },
            intrinsic: Intrinsic {
                t0_ns: 0,
                clock: CLOCK_MONOTONIC.into(),
                fs_hz: 4000.0,
                fs_source: FS_SOURCE_ESTIMATED.into(),
                channels: vec![ChannelDesc {
                    idx: 0,
                    role: ROLE_SIGNAL.into(),
                    label: None,
                    extra: Map::new(),
                }],
                n_samples: 0,
                sample_index: 0,
                gap_samples: 0,
                annotations: vec![],
                extra: Map::new(),
            },
            artifacts: vec![Artifact {
                role: "waveform".into(),
                path: path.to_string(),
                format: "npy/f32-le/C/(n,ch)".into(),
                size_bytes: bytes.len() as u64,
                blake3: Some(blake3::hash(bytes).to_hex().to_string()),
                extra: Map::new(),
            }],
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RecordManifest {
        RecordManifest {
            format_version: FORMAT_VERSION.to_string(),
            id: "01JXAB3C4D5E6F7G8H9JKMNPQR".to_string(),
            state: STATE_COMPLETE.to_string(),
            provenance: Provenance {
                session_id: None,
                graph: Some("pen-io-trigger-host".into()),
                node_id: "rec0".into(),
                plugin: "sigflow.io.storage@0.4.0".into(),
                host: "archlinux".into(),
                capture_seq: 17,
                stream_label: "pen0".into(),
                trigger_t0_ns: Some(1_234_567_890_123),
                created_utc: "2026-06-10T08:30:12.345Z".into(),
                extra: Map::new(),
            },
            intrinsic: Intrinsic {
                t0_ns: 1_234_567_000_000,
                clock: CLOCK_MONOTONIC.into(),
                fs_hz: 4000.0,
                fs_source: FS_SOURCE_ESTIMATED.into(),
                channels: vec![
                    ChannelDesc { idx: 0, role: ROLE_SIGNAL.into(), label: None, extra: Map::new() },
                    ChannelDesc { idx: 1, role: ROLE_TRIGGER.into(), label: Some("trig".into()), extra: Map::new() },
                ],
                n_samples: 2400,
                sample_index: 81920,
                gap_samples: 0,
                annotations: vec![AnnotationEntry::from_blob(
                    0xABCD,
                    br#"{"matched":true,"dt_ms":3.5}"#,
                )],
                extra: Map::new(),
            },
            artifacts: vec![Artifact {
                role: "waveform".into(),
                path: "waveform.npy".into(),
                format: "npy/f32-le/C/(n,ch)".into(),
                size_bytes: 57728,
                blake3: Some("a".repeat(64)),
                extra: Map::new(),
            }],
            extra: Map::new(),
        }
    }

    #[test]
    fn roundtrip_preserves_unknown_fields_at_every_level() {
        let mut v: Value = serde_json::from_slice(&sample().to_json_vec().unwrap()).unwrap();
        // Simulate a newer minor version adding fields at several levels.
        v["future_top"] = serde_json::json!({"a": 1});
        v["provenance"]["future_prov"] = serde_json::json!("x");
        v["intrinsic"]["future_intr"] = serde_json::json!([1, 2]);
        v["artifacts"][0]["future_art"] = serde_json::json!(true);
        v["intrinsic"]["channels"][0]["future_ch"] = serde_json::json!(7);
        v["intrinsic"]["annotations"][0]["future_ann"] = serde_json::json!("y");

        let bytes = serde_json::to_vec(&v).unwrap();
        let m = RecordManifest::from_slice(&bytes).unwrap();
        let back: Value = serde_json::from_slice(&m.to_json_vec().unwrap()).unwrap();
        assert_eq!(back["future_top"]["a"], 1);
        assert_eq!(back["provenance"]["future_prov"], "x");
        assert_eq!(back["intrinsic"]["future_intr"][1], 2);
        assert_eq!(back["artifacts"][0]["future_art"], true);
        assert_eq!(back["intrinsic"]["channels"][0]["future_ch"], 7);
        assert_eq!(back["intrinsic"]["annotations"][0]["future_ann"], "y");
    }

    #[test]
    fn version_gate_rejects_unknown_major_accepts_newer_minor() {
        let mut v: Value = serde_json::from_slice(&sample().to_json_vec().unwrap()).unwrap();
        v["format_version"] = "1.7".into();
        assert!(RecordManifest::from_slice(&serde_json::to_vec(&v).unwrap()).is_ok());
        v["format_version"] = "2.0".into();
        let err = RecordManifest::from_slice(&serde_json::to_vec(&v).unwrap()).unwrap_err();
        assert!(matches!(err, CatalogError::Version { .. }), "{err}");
        v["format_version"] = "garbage".into();
        assert!(matches!(
            RecordManifest::from_slice(&serde_json::to_vec(&v).unwrap()).unwrap_err(),
            CatalogError::Version { .. }
        ));
        // A major-2 manifest with a *restructured* body must still report
        // Version (upgrade the tool), not Json (bit rot).
        let v2 = br#"{"format_version":"2.0","records":[{"whatever":1}]}"#;
        assert!(matches!(
            RecordManifest::from_slice(v2).unwrap_err(),
            CatalogError::Version { .. }
        ));
        assert!(matches!(
            SessionManifest::from_slice(v2).unwrap_err(),
            CatalogError::Version { .. }
        ));
    }

    #[test]
    fn typed_accessors_map_known_and_unknown_values() {
        let mut m = sample();
        assert_eq!(m.intrinsic.channels[0].role(), ChannelRole::Signal);
        assert_eq!(m.intrinsic.channels[1].role(), ChannelRole::Trigger);
        assert_eq!(m.intrinsic.clock(), ClockBase::Monotonic);
        assert_eq!(m.intrinsic.fs_source(), FsSource::Estimated);
        m.intrinsic.channels[0].role = "aux".into();
        m.intrinsic.clock = "tai".into();
        m.intrinsic.fs_source = "device".into();
        assert_eq!(m.intrinsic.channels[0].role(), ChannelRole::Unknown);
        assert_eq!(m.intrinsic.clock(), ClockBase::Unknown);
        assert_eq!(m.intrinsic.fs_source(), FsSource::Unknown);
        // The raw strings survive a round-trip untouched.
        let back = RecordManifest::from_slice(&m.to_json_vec().unwrap()).unwrap();
        assert_eq!(back.intrinsic.clock, "tai");
        assert_eq!(back.intrinsic.channels[0].role, "aux");
    }

    #[test]
    fn validate_complete_rejects_subdir_paths_in_v1() {
        let mut m = sample();
        m.artifacts[0].path = "sub/waveform.npy".into();
        let msg = m.validate_complete().unwrap_err().to_string();
        assert!(msg.contains("flat filename"), "{msg}");
    }

    #[test]
    fn state_accessor_and_unknown_state_survival() {
        let mut m = sample();
        assert_eq!(m.state(), RecordState::Complete);
        m.state = "archived".to_string(); // written by a future tool
        assert_eq!(m.state(), RecordState::Unknown);
        let back = RecordManifest::from_slice(&m.to_json_vec().unwrap()).unwrap();
        assert_eq!(back.state, "archived"); // raw string preserved
    }

    #[test]
    fn validate_complete_collects_problems() {
        let mut m = sample();
        assert!(m.validate_complete().is_ok());

        m.artifacts[0].blake3 = None;
        m.artifacts[0].path = "../escape.npy".into();
        m.intrinsic.channels.clear();
        m.state = STATE_RECORDING.into();
        let msg = m.validate_complete().unwrap_err().to_string();
        for needle in ["missing blake3", "not a safe relative path", "channels is empty", "not complete"] {
            assert!(msg.contains(needle), "missing '{needle}' in: {msg}");
        }
    }

    #[test]
    fn annotation_blob_json_vs_binary() {
        let j = AnnotationEntry::from_blob(1, br#"{"x":1}"#);
        assert_eq!(j.data["x"], 1);
        let b = AnnotationEntry::from_blob(2, &[0xff, 0x00]);
        assert_eq!(b.data, Value::String("ff00".into()));
    }

    #[test]
    fn session_manifest_roundtrip() {
        let s = SessionManifest {
            format_version: FORMAT_VERSION.into(),
            id: "01JXAB3C4D5E6F7G8H9JKMNPQR".into(),
            started_utc: "2026-06-10T08:00:00.000Z".into(),
            graph: Some("pen-io-trigger-host".into()),
            host: "archlinux".into(),
            attrs: serde_json::json!({"subject": "s01"}).as_object().unwrap().clone(),
            sink_attrs: serde_json::json!({"rec0": {"pen": "A"}}).as_object().unwrap().clone(),
            extra: Map::new(),
        };
        let back = SessionManifest::from_slice(&s.to_json_vec().unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.sink_attrs["rec0"]["pen"], "A");
    }
}
