//! Storage plugin: a sink node that records the incoming signal into the
//! catalog data root.
//!
//! Built for the trigger-capture use case: each capture window arrives as a
//! contiguous burst whose first frame carries `FLAG_DISCONTINUITY`. The plugin
//! buffers a capture in memory and, when the next capture begins (or on stop,
//! or when `max_samples` is exceeded), commits it as one record:
//!
//! ```text
//! <data_root>/records/<shard>/<ulid>/
//! ├── record.json    — manifest of truth (provenance, intrinsic, hashes)
//! └── waveform.npy   — NumPy v1.0, shape (n_samples, channels), LE f32
//! ```
//!
//! The commit goes through `sigflow_catalog::commit_complete_record`, which
//! binds the fsync order (artifact durable → manifest tmp+rename) and the
//! completeness gate together — this segment-buffered sink intentionally
//! writes no provisional manifest (a crash mid-capture loses only what was
//! in memory and leaves zero on-disk residue).
//!
//! The record ULID is generated when the capture's first frame arrives, so
//! its time bits approximate the trigger time. `fs` (per-channel sample
//! rate) is not in the frame header; it is estimated from consecutive
//! frames' `Δsample_index / Δt0_ns` and stamped as `fs_source = estimated`.
//! `provenance.session_id` is the session-pointer snapshot at flush time:
//! the plugin reads `_session/current.json` once per record commit (µs-scale,
//! never cached, so `data session begin/end` take effect on the very next
//! capture). Every pointer-read failure degrades to `session_id = null` —
//! repairable via the annotation layer — and is logged once per distinct
//! reason. Contract lives in `manifest.toml`.

use std::fs;

use sigflow_catalog::manifest::{
    CLOCK_MONOTONIC, FORMAT_VERSION, FS_SOURCE_ESTIMATED, FS_SOURCE_UNKNOWN, ROLE_SIGNAL,
    ROLE_TRIGGER, STATE_COMPLETE,
};
use sigflow_catalog::{
    commit_complete_record, AnnotationEntry, ArtifactPayload, ChannelDesc, DataRoot, Intrinsic,
    Provenance, RecordManifest, UlidGen,
};
use sigflow_plugin_sdk::{
    parse_annotation, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

const F32: usize = 4;

pub struct Storage {
    // --- Parameters ---
    enabled: bool,
    data_root: String,
    stream_label: String,
    node_id: String,
    graph: String,      // graph name for the session pointer's mismatch check
    trig_channels: u32, // trailing marker channels (manifest channel roles)
    channels: u32,      // 0 = auto
    max_samples: u32,
    /// Push-order proofing: the shell pushes the whole ParamStore in HashMap
    /// (i.e. random) order on hot-load/restart. Once the canonical param has
    /// arrived non-empty, the deprecated aliases must no longer override it —
    /// otherwise an old persisted `output_dir` and the new `data_root` would
    /// race and the effective root would differ per restart.
    data_root_set: bool,
    stream_label_set: bool,

    // --- Current capture segment ---
    buf: Vec<u8>,        // raw interleaved f32 bytes accumulated for this segment
    seg_open: bool,      // a segment is currently buffering
    seg_channels: usize, // channels locked at the segment's first frame
    seg_t0_ns: i64,      // t0 of the segment's first sample
    seg_sample_index: u64,
    seg_gap: u64,
    seg_id: String,          // record ULID, generated at the segment's first frame
    seg_created_utc: String, // wall-clock anchor, captured with the ULID
    /// Frame annotation captured for this segment (the first one seen), as
    /// `(schema_id, bytes)`. Lands in the manifest's `intrinsic.annotations`
    /// (capture-time facts), NOT in the mutable analysis-side annotations/.
    seg_annotation: Option<(u64, Vec<u8>)>,

    // --- Sample-rate estimate (from consecutive frames) ---
    prev: Option<(u64, i64)>, // (sample_index, t0_ns) of the last frame seen
    fs: f64,
    /// The stream has shown strictly increasing `sample_index` — enables the
    /// in-segment continuity check (index-less sources always send 0 and
    /// must not be split on every frame).
    indexed: bool,

    // --- Misc ---
    ids: UlidGen,
    capture_seq: u64, // counts ATTEMPTED captures: an on-disk gap = loss evidence
    plugin_id: String, // "name@version" for provenance
    host: String,
    warned: bool,      // commit-failure log throttle (reset by the next success)
    warned_gap: bool,  // in-segment-hole log throttle
    warned_trig: bool, // trig_channels-misconfiguration log throttle
    /// Session-state log throttle: the message identity of the last warning
    /// ("recording without session" / a pointer-refusal reason). Logged only
    /// on change, cleared when a session is active again.
    session_note: Option<String>,
}

impl Storage {
    /// Build the NumPy v1.0 header for an `(rows, cols)` little-endian f32 array.
    /// The total header (10-byte preamble + dict + padding + '\n') is padded to a
    /// multiple of 64 bytes, per the .npy spec.
    fn npy_header(rows: usize, cols: usize) -> Vec<u8> {
        let dict = format!(
            "{{'descr': '<f4', 'fortran_order': False, 'shape': ({rows}, {cols}), }}"
        );
        // 10 = 6 (magic) + 2 (version) + 2 (header-len u16).
        let base = 10 + dict.len() + 1; // +1 for the trailing newline
        let pad = (64 - (base % 64)) % 64;
        let hlen = dict.len() + 1 + pad; // value stored in the u16 length field

        let mut out = Vec::with_capacity(10 + hlen);
        out.extend_from_slice(b"\x93NUMPY"); // magic
        out.push(1); // major version
        out.push(0); // minor version
        out.extend_from_slice(&(hlen as u16).to_le_bytes());
        out.extend_from_slice(dict.as_bytes());
        out.extend(std::iter::repeat_n(b' ', pad));
        out.push(b'\n');
        out
    }

    /// Channel descriptors for the manifest: the last `trig_channels` columns
    /// are trigger markers (appended upstream by trigger-capture), the rest
    /// are signal. Replay strips trigger roles to rebuild clean frames.
    fn channel_descs(&self, cols: usize) -> Vec<ChannelDesc> {
        let trig = (self.trig_channels as usize).min(cols);
        (0..cols)
            .map(|idx| {
                let is_trig = idx >= cols - trig;
                ChannelDesc {
                    idx: idx as u32,
                    role: if is_trig { ROLE_TRIGGER } else { ROLE_SIGNAL }.to_string(),
                    label: is_trig.then(|| "trig".to_string()),
                    extra: Default::default(),
                }
            })
            .collect()
    }

    /// Commit the buffered segment as one record (waveform.npy + record.json).
    fn write_segment(&mut self, rows: usize, cols: usize) -> Result<(), String> {
        if self.trig_channels > 0 && self.trig_channels as usize >= cols && !self.warned_trig {
            eprintln!(
                "storage: trig_channels={} >= channel count {cols} — this record has no \
                 signal channels (misconfigured trig_channels?)",
                self.trig_channels
            );
            self.warned_trig = true;
        }
        let mut npy = Self::npy_header(rows, cols);
        npy.extend_from_slice(&self.buf);

        // Per-flush pointer read (never cached): begin/end take effect on the
        // very next capture, and a stale in-memory value has no window. Every
        // failure path is session=null + a reason — never a wrong id.
        let session = sigflow_catalog::read_active_session(
            &DataRoot::new(&self.data_root),
            (!self.graph.is_empty()).then_some(self.graph.as_str()),
        );
        let note = match (&session.session_id, &session.degraded) {
            (Some(_), _) => None,
            (None, Some(reason)) => Some(format!("session=null — {reason}")),
            (None, None) => Some("recording without a session (session_id=null; \
                 `sigflow-cli data session begin` to attribute captures)"
                .to_string()),
        };
        if note != self.session_note {
            if let Some(n) = &note {
                eprintln!("storage: {n}");
            }
            self.session_note = note;
        }

        let mut manifest = RecordManifest {
            format_version: FORMAT_VERSION.to_string(),
            id: self.seg_id.clone(),
            state: STATE_COMPLETE.to_string(), // (re)set by commit
            provenance: Provenance {
                session_id: session.session_id,
                graph: (!self.graph.is_empty()).then(|| self.graph.clone()),
                node_id: self.node_id.clone(),
                plugin: self.plugin_id.clone(),
                host: self.host.clone(),
                capture_seq: self.capture_seq,
                stream_label: self.stream_label.clone(),
                // The frame header carries no independent trigger instant
                // (t0_ns is the window start); honest null beats a wrong copy.
                trigger_t0_ns: None,
                created_utc: self.seg_created_utc.clone(),
                extra: Default::default(),
            },
            intrinsic: Intrinsic {
                t0_ns: self.seg_t0_ns,
                clock: CLOCK_MONOTONIC.to_string(),
                fs_hz: if self.fs.is_finite() { self.fs } else { 0.0 },
                // A 0.0 rate is "no estimate yet" (e.g. single-frame capture)
                // and must not be presented as an estimate.
                fs_source: if self.fs.is_finite() && self.fs > 0.0 {
                    FS_SOURCE_ESTIMATED
                } else {
                    FS_SOURCE_UNKNOWN
                }
                .to_string(),
                channels: self.channel_descs(cols),
                n_samples: rows as u64,
                sample_index: self.seg_sample_index,
                gap_samples: self.seg_gap,
                annotations: self
                    .seg_annotation
                    .iter()
                    .map(|(schema, bytes)| AnnotationEntry::from_blob(*schema, bytes))
                    .collect(),
                extra: Default::default(),
            },
            artifacts: Vec::new(), // filled by commit from the payloads
            extra: Default::default(),
        };

        commit_complete_record(
            &DataRoot::new(&self.data_root),
            &mut manifest,
            &[ArtifactPayload {
                role: "waveform",
                path: "waveform.npy",
                format: "npy/f32-le/C/(n,ch)",
                bytes: &npy,
            }],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    /// Flush the current segment to disk (if any whole rows are buffered) and
    /// reset the segment state. Errors are logged once, never faulted.
    fn flush(&mut self) {
        let cols = self.seg_channels.max(1);
        let rows = (self.buf.len() / F32) / cols;
        if self.seg_open && rows > 0 {
            match self.write_segment(rows, cols) {
                // A success ends a failure burst: the next burst logs again.
                Ok(()) => self.warned = false,
                Err(e) => {
                    if !self.warned {
                        eprintln!("storage: failed to commit record to {}: {e}", self.data_root);
                        self.warned = true;
                    }
                }
            }
            // Every ATTEMPTED capture counts, success or not: a gap in the
            // on-disk capture_seq sequence is the audit evidence that
            // something was lost (capture_seq is evidence, not identity —
            // a dense sequence must mean nothing was dropped).
            self.capture_seq += 1;
        }
        self.seg_open = false;
        self.buf.clear();
    }
}

impl Plugin for Storage {
    fn new(manifest: &PluginManifest) -> Self {
        Storage {
            enabled: true,
            data_root: "captures".to_string(),
            stream_label: "capture".to_string(),
            node_id: String::new(),
            graph: String::new(),
            trig_channels: 0,
            channels: 0,
            max_samples: 1_048_576,
            data_root_set: false,
            stream_label_set: false,
            buf: Vec::new(),
            seg_open: false,
            seg_channels: 0,
            seg_t0_ns: 0,
            seg_sample_index: 0,
            seg_gap: 0,
            seg_id: String::new(),
            seg_created_utc: String::new(),
            seg_annotation: None,
            prev: None,
            fs: 0.0,
            indexed: false,
            ids: UlidGen::new(),
            capture_seq: 0,
            plugin_id: format!("{}@{}", manifest.name, manifest.version),
            host: sigflow_catalog::hostname(),
            warned: false,
            warned_gap: false,
            warned_trig: false,
            session_note: None,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("enabled", ParamValue::Bool(v)) => self.enabled = *v,
            ("data_root", ParamValue::String(s)) if !s.is_empty() => {
                self.data_root = s.clone();
                self.data_root_set = true;
            }
            ("stream_label", ParamValue::String(s)) if !s.is_empty() => {
                self.stream_label = s.clone();
                self.stream_label_set = true;
            }
            ("node_id", ParamValue::String(s)) => self.node_id = s.clone(),
            ("graph", ParamValue::String(s)) => self.graph = s.clone(),
            ("trig_channels", ParamValue::U32(v)) => self.trig_channels = *v,
            ("channels", ParamValue::U32(v)) => self.channels = *v,
            ("max_samples", ParamValue::U32(v)) => self.max_samples = *v,
            // Migration-window aliases. Two guards make them push-order-proof
            // (the shell replays the whole ParamStore in HashMap order):
            // empty values (the alias manifest defaults) are ignored, and a
            // canonical param that has already arrived always wins.
            ("output_dir", ParamValue::String(s)) if !s.is_empty() && !self.data_root_set => {
                eprintln!("storage: param 'output_dir' is deprecated, use 'data_root'");
                self.data_root = s.clone();
            }
            ("prefix", ParamValue::String(s)) if !s.is_empty() && !self.stream_label_set => {
                eprintln!("storage: param 'prefix' is deprecated, use 'stream_label'");
                self.stream_label = s.clone();
            }
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        // Fresh recording run: ensure the data root exists and reset state.
        if let Err(e) = fs::create_dir_all(&self.data_root) {
            return ProcessOutcome::Fault {
                reason: format!("storage: cannot create data root '{}': {e}", self.data_root),
            };
        }
        self.buf.clear();
        self.seg_open = false;
        self.capture_seq = 0;
        self.prev = None;
        self.fs = 0.0;
        self.indexed = false;
        self.warned = false;
        self.warned_gap = false;
        self.warned_trig = false;
        ProcessOutcome::Ok
    }

    fn process(&mut self, inputs: &[Frame], _outputs: &mut [FrameOut]) -> ProcessOutcome {
        if !self.enabled {
            return ProcessOutcome::Ok;
        }
        let Some(input) = inputs.first() else {
            return ProcessOutcome::Ok;
        };
        let h = &input.header;
        // Samples only — an attached annotation trailer must never land in the
        // .npy. The annotation (if any) is captured separately below.
        let data = input.samples();
        if data.is_empty() {
            return ProcessOutcome::Ok; // header-only / marker frame: nothing to store
        }
        let annotation = input
            .annotation()
            .and_then(parse_annotation)
            .map(|(id, b)| (id, b.to_vec()));

        // Update the running fs estimate from consecutive frames.
        if let Some((pi, pt)) = self.prev {
            let dsi = h.sample_index as i64 - pi as i64;
            if dsi > 0 {
                // The stream carries real (increasing) sample indices — the
                // in-segment continuity check below becomes meaningful.
                self.indexed = true;
                let dt = h.t0_ns - pt;
                if dt > 0 {
                    self.fs = dsi as f64 / (dt as f64 / 1e9);
                }
            }
        }
        self.prev = Some((h.sample_index, h.t0_ns));

        // Channels for this frame.
        let frame_channels = if self.channels > 0 {
            self.channels as usize
        } else if h.n_samples > 0 {
            (data.len() / F32) / (h.n_samples as usize)
        } else {
            return ProcessOutcome::Ok; // can't infer geometry; skip
        };
        let frame_channels = frame_channels.max(1);

        // A discontinuity begins a new capture: flush the previous one first.
        if h.is_discontinuity() && self.seg_open {
            self.flush();
        }

        // In-segment continuity check (indexed streams only): record.json
        // asserts a gapless window, so a hole the shell's latest-wins drain
        // tore into the stream must SPLIT the record, never get silently
        // spliced — the manifest must not vouch for a discontinuity the data
        // actually has. The hole size carries into the next record's
        // gap_samples (whose contract meaning is "samples missing before
        // this window"). A backwards jump (e.g. a lost discontinuity frame)
        // also splits, with hole 0.
        let mut hole = 0u64;
        if self.seg_open && self.indexed {
            let buffered_rows = (self.buf.len() / (self.seg_channels.max(1) * F32)) as u64;
            let expected = self.seg_sample_index + buffered_rows;
            if buffered_rows > 0 && h.sample_index != expected {
                hole = h.sample_index.saturating_sub(expected);
                if !self.warned_gap {
                    eprintln!(
                        "storage: in-segment hole (expected sample_index {expected}, got {}) — \
                         splitting the record; further holes are not logged",
                        h.sample_index
                    );
                    self.warned_gap = true;
                }
                self.flush();
            }
        }

        // Open a new segment if none is buffering. The record's identity is
        // born here: ULID time bits ≈ first-frame arrival ≈ trigger time.
        if !self.seg_open {
            self.seg_open = true;
            self.buf.clear();
            self.seg_channels = frame_channels;
            self.seg_t0_ns = h.t0_ns;
            self.seg_sample_index = h.sample_index;
            self.seg_gap = h.gap_samples.saturating_add(hole);
            self.seg_id = self.ids.next_now();
            self.seg_created_utc = sigflow_catalog::utc_now_rfc3339();
            self.seg_annotation = None;
        }
        // First annotation seen within the segment wins (all frames of one
        // capture carry the same touch coordinate).
        if self.seg_annotation.is_none() {
            self.seg_annotation = annotation;
        }

        // Append whole rows of this frame (locked to the segment's channel count).
        let row = self.seg_channels * F32;
        if row > 0 {
            let usable = (data.len() / row) * row;
            self.buf.extend_from_slice(&data[..usable]);
        }

        // OOM guard for continuous (non-segmented) streams.
        let max_bytes = self.max_samples as usize * self.seg_channels * F32;
        if max_bytes > 0 && self.buf.len() >= max_bytes {
            self.flush();
        }
        ProcessOutcome::Ok
    }

    fn stop(&mut self) {
        // Persist the final in-flight capture so it is not lost on stop.
        self.flush();
    }

    fn shutdown(&mut self) {
        self.flush();
    }
}

sigflow_plugin_sdk::export_plugin!(Storage);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sigflow_catalog::adopt::{check_record_dir, Verdict};
    use sigflow_plugin_sdk::FrameHeader;
    use std::path::{Path, PathBuf};

    /// A signal frame: `rows` samples × `channels` f32 where value(r,c) = base + r.
    fn frame_bytes(rows: usize, channels: usize, base: f32) -> Vec<u8> {
        let mut v = Vec::with_capacity(rows * channels * F32);
        for r in 0..rows {
            for _c in 0..channels {
                v.extend_from_slice(&(base + r as f32).to_le_bytes());
            }
        }
        v
    }

    fn header(seq: u64, sample_index: u64, t0_ns: i64, n_samples: u32, disc: bool) -> FrameHeader {
        let mut h = FrameHeader::default();
        h.seq = seq;
        h.sample_index = sample_index;
        h.t0_ns = t0_ns;
        h.n_samples = n_samples;
        if disc {
            h.set_discontinuity(0);
        }
        h
    }

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    fn new_storage(dir: &Path, label: &str) -> Storage {
        let mut s = Storage::new(&manifest());
        s.set_param("data_root", &ParamValue::String(dir.to_string_lossy().to_string()));
        s.set_param("stream_label", &ParamValue::String(label.to_string()));
        s.set_param("node_id", &ParamValue::String("rec0".to_string()));
        s.start();
        s
    }

    /// All committed record directories under `<root>/records/<shard>/<ulid>/`,
    /// sorted by ULID (= time order).
    fn record_dirs(root: &Path) -> Vec<PathBuf> {
        let mut v = Vec::new();
        let records = root.join("records");
        if !records.exists() {
            return v;
        }
        for shard in std::fs::read_dir(records).unwrap() {
            for rec in std::fs::read_dir(shard.unwrap().path()).unwrap() {
                let p = rec.unwrap().path();
                // Session manifests share the records tree; only record dirs.
                if p.join("record.json").exists() {
                    v.push(p);
                }
            }
        }
        v.sort();
        v
    }

    fn read_manifest(dir: &Path) -> RecordManifest {
        RecordManifest::from_slice(&std::fs::read(dir.join("record.json")).unwrap()).unwrap()
    }

    fn process_one(s: &mut Storage, h: FrameHeader, data: &[u8]) -> ProcessOutcome {
        let f = Frame::new(h, data);
        s.process(std::slice::from_ref(&f), &mut [])
    }

    /// Parse the shape `(rows, cols)` out of a .npy header.
    fn npy_shape(path: &Path) -> (usize, usize) {
        let bytes = std::fs::read(path).unwrap();
        let hlen = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
        let header = String::from_utf8_lossy(&bytes[10..10 + hlen]);
        let s = header.split("'shape':").nth(1).unwrap();
        let inside = s.split('(').nth(1).unwrap().split(')').next().unwrap();
        let mut it = inside.split(',').map(|x| x.trim()).filter(|x| !x.is_empty());
        let rows: usize = it.next().unwrap().parse().unwrap();
        let cols: usize = it.next().unwrap().parse().unwrap();
        (rows, cols)
    }

    /// Read the f32 payload of a .npy (after its header).
    fn npy_f32(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap();
        let hlen = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
        bytes[10 + hlen..]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn manifest_shape() {
        let m = manifest();
        assert_eq!(m.name, "sigflow.io.storage");
        assert_eq!(m.ports.len(), 1);
        assert_eq!(m.ports[0].id, "signal");
        let param_ids: Vec<&str> = m.parameters.iter().map(|p| p.id.as_str()).collect();
        for id in [
            "enabled",
            "data_root",
            "stream_label",
            "node_id",
            "graph",
            "trig_channels",
            "channels",
            "max_samples",
            // migration-window aliases
            "output_dir",
            "prefix",
        ] {
            assert!(param_ids.contains(&id), "missing param {id}");
        }
    }

    #[test]
    fn session_pointer_is_read_per_flush_and_stamped() {
        use sigflow_catalog::manifest::{SessionManifest, FORMAT_VERSION};

        let dir = tempfile::tempdir().unwrap();
        let root = sigflow_catalog::DataRoot::new(dir.path());
        let session = SessionManifest {
            format_version: FORMAT_VERSION.to_string(),
            id: "01JXAB3C4D5E6F7G8H9JKMNPQR".into(),
            started_utc: sigflow_catalog::utc_now_rfc3339(),
            graph: Some("vofa-dual-pen".into()),
            host: "test".into(),
            attrs: serde_json::from_str(r#"{"subject":"alice"}"#).unwrap(),
            sink_attrs: Default::default(),
            extra: Default::default(),
        };

        let mut s = new_storage(dir.path(), "pen0");
        s.set_param("graph", &ParamValue::String("vofa-dual-pen".into()));

        // The pointer is read at FLUSH time, and a capture flushes when the
        // NEXT capture's first frame arrives (deferred flush). So capture 0
        // (buffered before begin, flushed after) gets the session — the
        // user-ratified boundary effect: a tap just before `begin` may
        // attribute forward, auditable post-hoc via created_utc vs the
        // session's started_utc, repairable via annotations. Captures 1 and
        // 2 flush after `end` → null. No restart needed for any of it.
        process_one(&mut s, header(0, 0, 0, 2, true), &frame_bytes(2, 1, 0.0));
        sigflow_catalog::begin_session(&root, &session).unwrap();
        process_one(&mut s, header(1, 10, 1000, 2, true), &frame_bytes(2, 1, 0.0));
        sigflow_catalog::end_session(&root).unwrap();
        process_one(&mut s, header(2, 20, 2000, 2, true), &frame_bytes(2, 1, 0.0));
        s.stop();

        let ids: Vec<Option<String>> = record_dirs(dir.path())
            .iter()
            .map(|r| read_manifest(r).provenance.session_id)
            .collect();
        assert_eq!(
            ids,
            [Some("01JXAB3C4D5E6F7G8H9JKMNPQR".to_string()), None, None],
            "the pointer snapshot is taken at flush time, never cached"
        );
        // The sink's own graph lands in provenance regardless of session.
        for r in &record_dirs(dir.path()) {
            assert_eq!(read_manifest(r).provenance.graph.as_deref(), Some("vofa-dual-pen"));
        }
    }

    #[test]
    fn graph_mismatch_degrades_to_null_not_wrong_value() {
        use sigflow_catalog::manifest::{SessionManifest, FORMAT_VERSION};

        let dir = tempfile::tempdir().unwrap();
        let root = sigflow_catalog::DataRoot::new(dir.path());
        let session = SessionManifest {
            format_version: FORMAT_VERSION.to_string(),
            id: "01JXAB3C4D5E6F7G8H9JKMNPQR".into(),
            started_utc: sigflow_catalog::utc_now_rfc3339(),
            graph: Some("other-graph".into()),
            host: "test".into(),
            attrs: serde_json::from_str(r#"{"subject":"alice"}"#).unwrap(),
            sink_attrs: Default::default(),
            extra: Default::default(),
        };
        sigflow_catalog::begin_session(&root, &session).unwrap();

        let mut s = new_storage(dir.path(), "pen0");
        s.set_param("graph", &ParamValue::String("vofa-dual-pen".into()));
        process_one(&mut s, header(0, 0, 0, 2, true), &frame_bytes(2, 1, 0.0));
        s.stop();

        let m = read_manifest(&record_dirs(dir.path())[0]);
        assert_eq!(m.provenance.session_id, None, "mismatched session must not be stamped");
    }

    #[test]
    fn annotated_capture_persists_annotation_and_excludes_trailer_from_npy() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "pen0");
        let ch = 2;
        let samples = frame_bytes(4, ch, 0.0); // 4 rows × 2 ch
        let json = br#"{"matched":true,"dt_ms":3.5,"points":[{"x":640,"y":480}]}"#;

        // Build an annotated frame: payload = [samples | schema_id | json].
        let mut buf = vec![0u8; samples.len() + 8 + json.len() + 16];
        let (h, w) = {
            let mut out = sigflow_plugin_sdk::FrameOut::new(&mut buf);
            out.write(&samples);
            assert!(out.set_annotation(0xABCD, json));
            out.header.n_samples = 4;
            out.header.set_discontinuity(0);
            (out.header, out.written())
        };
        let payload = buf[..w].to_vec();
        process_one(&mut s, h, &payload);
        s.stop();

        let recs = record_dirs(dir.path());
        assert_eq!(recs.len(), 1);
        // The annotation trailer must NOT inflate the .npy: shape is exactly (4, 2).
        assert_eq!(npy_shape(&recs[0].join("waveform.npy")), (4, 2));
        // The annotation lands in intrinsic.annotations with its schema id.
        let m = read_manifest(&recs[0]);
        assert_eq!(m.intrinsic.annotations.len(), 1);
        assert_eq!(m.intrinsic.annotations[0].schema_id, 0xABCD);
        assert_eq!(m.intrinsic.annotations[0].data["matched"], true);
        assert_eq!(m.intrinsic.annotations[0].data["points"][0]["x"], 640);
        // The catalog scanner is the oracle: the record is Complete.
        let out = check_record_dir(&recs[0]).unwrap();
        assert_eq!(out.adoption.verdict, Verdict::Complete);
    }

    #[test]
    fn npy_header_is_64_byte_aligned_and_well_formed() {
        for (rows, cols) in [(1, 1), (2048, 6), (100, 3), (7, 9)] {
            let h = Storage::npy_header(rows, cols);
            assert_eq!(&h[0..6], b"\x93NUMPY");
            assert_eq!(h[6], 1); // version major
            assert_eq!(h.len() % 64, 0, "header not 64-aligned for ({rows},{cols})");
            assert_eq!(*h.last().unwrap(), b'\n');
            let s = String::from_utf8_lossy(&h[10..]);
            assert!(s.contains(&format!("'shape': ({rows}, {cols})")), "shape missing: {s}");
            assert!(s.contains("'descr': '<f4'"));
            assert!(s.contains("'fortran_order': False"));
        }
    }

    #[test]
    fn two_captures_two_records_with_correct_shape() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "pen0");
        let ch = 6;

        // Capture 1: discontinuity then a continuation chunk (same window).
        let d = process_one(&mut s, header(0, 20, 1000, 4, true), &frame_bytes(4, ch, 0.0));
        assert!(matches!(d, ProcessOutcome::Ok));
        process_one(&mut s, header(1, 24, 1041, 4, false), &frame_bytes(4, ch, 4.0));
        // Capture 2: a new discontinuity flushes capture 1.
        process_one(&mut s, header(2, 100, 2000, 3, true), &frame_bytes(3, ch, 0.0));
        // stop() flushes capture 2.
        s.stop();

        let recs = record_dirs(dir.path());
        assert_eq!(recs.len(), 2, "expected 2 records, got {recs:?}");

        // Capture 1 has 8 rows (4 + 4), capture 2 has 3 rows; both 6 channels.
        let shapes: Vec<(usize, usize)> =
            recs.iter().map(|p| npy_shape(&p.join("waveform.npy"))).collect();
        assert!(shapes.contains(&(8, 6)), "shapes: {shapes:?}");
        assert!(shapes.contains(&(3, 6)), "shapes: {shapes:?}");
        // Distinct ULIDs, both Complete, capture_seq increments.
        let m0 = read_manifest(&recs[0]);
        let m1 = read_manifest(&recs[1]);
        assert_ne!(m0.id, m1.id);
        assert_eq!([m0.provenance.capture_seq, m1.provenance.capture_seq], [0, 1]);
        for r in &recs {
            assert_eq!(check_record_dir(r).unwrap().adoption.verdict, Verdict::Complete);
        }
    }

    #[test]
    fn npy_payload_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "p");
        let ch = 2;
        process_one(&mut s, header(0, 0, 0, 3, true), &frame_bytes(3, ch, 10.0));
        s.stop();

        let recs = record_dirs(dir.path());
        assert_eq!(recs.len(), 1);
        let npy = recs[0].join("waveform.npy");
        assert_eq!(npy_shape(&npy), (3, 2));
        // row r, both channels == 10 + r
        assert_eq!(npy_f32(&npy), vec![10.0, 10.0, 11.0, 11.0, 12.0, 12.0]);
    }

    #[test]
    fn record_manifest_has_provenance_and_intrinsic_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "pen1");
        // Two frames to establish fs: dsi=4, dt=1_000_000 ns → 4000 Hz.
        process_one(&mut s, header(0, 0, 0, 4, true), &frame_bytes(4, 3, 0.0));
        process_one(&mut s, header(1, 4, 1_000_000, 4, false), &frame_bytes(4, 3, 4.0));
        s.stop();

        let recs = record_dirs(dir.path());
        assert_eq!(recs.len(), 1);
        let m = read_manifest(&recs[0]);
        assert!(sigflow_catalog::is_valid_ulid(&m.id), "{}", m.id);
        assert_eq!(m.state, "complete");
        assert_eq!(m.provenance.stream_label, "pen1");
        assert_eq!(m.provenance.node_id, "rec0");
        assert!(m.provenance.plugin.starts_with("sigflow.io.storage@"));
        assert!(!m.provenance.host.is_empty());
        assert!(m.provenance.session_id.is_none());
        assert!(!m.provenance.created_utc.is_empty());
        assert_eq!(m.intrinsic.t0_ns, 0);
        assert_eq!(m.intrinsic.clock, "monotonic");
        assert_eq!(m.intrinsic.fs_hz, 4000.0);
        assert_eq!(m.intrinsic.fs_source, "estimated");
        assert_eq!(m.intrinsic.channels.len(), 3);
        assert_eq!(m.intrinsic.n_samples, 8);
        assert_eq!(m.intrinsic.sample_index, 0);
    }

    #[test]
    fn trailing_trig_channels_get_trigger_role() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "pen0");
        s.set_param("trig_channels", &ParamValue::U32(1));
        process_one(&mut s, header(0, 0, 0, 4, true), &frame_bytes(4, 3, 0.0));
        s.stop();

        let recs = record_dirs(dir.path());
        let m = read_manifest(&recs[0]);
        let roles: Vec<&str> = m.intrinsic.channels.iter().map(|c| c.role.as_str()).collect();
        assert_eq!(roles, ["signal", "signal", "trigger"]);
        use sigflow_catalog::ChannelRole;
        assert_eq!(m.intrinsic.channels[2].role(), ChannelRole::Trigger);
    }

    #[test]
    fn legacy_output_dir_and_prefix_params_still_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Storage::new(&manifest());
        s.set_param("output_dir", &ParamValue::String(dir.path().to_string_lossy().to_string()));
        s.set_param("prefix", &ParamValue::String("pen9".to_string()));
        // The shell pushes every manifest default: the empty alias defaults
        // must not clobber what was just set.
        s.set_param("output_dir", &ParamValue::String(String::new()));
        s.set_param("prefix", &ParamValue::String(String::new()));
        s.start();
        process_one(&mut s, header(0, 0, 0, 2, true), &frame_bytes(2, 1, 0.0));
        s.stop();

        let recs = record_dirs(dir.path());
        assert_eq!(recs.len(), 1);
        assert_eq!(read_manifest(&recs[0]).provenance.stream_label, "pen9");
    }

    #[test]
    fn canonical_param_beats_alias_regardless_of_order() {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().to_string_lossy().to_string();
        // alias first, canonical later → canonical wins
        let mut s = Storage::new(&manifest());
        s.set_param("output_dir", &ParamValue::String("/old-root".into()));
        s.set_param("prefix", &ParamValue::String("old".into()));
        s.set_param("data_root", &ParamValue::String(canon.clone()));
        s.set_param("stream_label", &ParamValue::String("new".into()));
        assert_eq!(s.data_root, canon);
        assert_eq!(s.stream_label, "new");
        // canonical first, alias later (HashMap replay order is random!)
        // → alias must NOT override
        let mut s = Storage::new(&manifest());
        s.set_param("data_root", &ParamValue::String(canon.clone()));
        s.set_param("stream_label", &ParamValue::String("new".into()));
        s.set_param("output_dir", &ParamValue::String("/old-root".into()));
        s.set_param("prefix", &ParamValue::String("old".into()));
        assert_eq!(s.data_root, canon);
        assert_eq!(s.stream_label, "new");
    }

    #[test]
    fn rolling_split_on_max_samples_produces_two_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "cont");
        s.set_param("max_samples", &ParamValue::U32(4));
        // Continuous indexed stream, no discontinuities: 4 + 4 rows, 1 ch.
        process_one(&mut s, header(0, 0, 0, 4, true), &frame_bytes(4, 1, 0.0));
        process_one(&mut s, header(1, 4, 1_000_000, 4, false), &frame_bytes(4, 1, 4.0));
        s.stop();

        let recs = record_dirs(dir.path());
        assert_eq!(recs.len(), 2, "rolling split expected: {recs:?}");
        let m0 = read_manifest(&recs[0]);
        let m1 = read_manifest(&recs[1]);
        assert_ne!(m0.id, m1.id);
        // Second roll continues the stream: index picks up where roll 1 ended,
        // no gap, capture_seq increments.
        assert_eq!([m0.intrinsic.sample_index, m1.intrinsic.sample_index], [0, 4]);
        assert_eq!(m1.intrinsic.gap_samples, 0);
        assert_eq!([m0.provenance.capture_seq, m1.provenance.capture_seq], [0, 1]);
        for r in &recs {
            assert_eq!(check_record_dir(r).unwrap().adoption.verdict, Verdict::Complete);
        }
    }

    #[test]
    fn in_segment_hole_splits_record_and_carries_gap() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "pen0");
        // Window frame at index 20 (4 rows), then a continuation that should
        // start at 24 but arrives at 28 — the shell's latest-wins drain ate a
        // frame. The record must split, never silently splice.
        process_one(&mut s, header(0, 20, 1000, 4, true), &frame_bytes(4, 2, 0.0));
        process_one(&mut s, header(1, 28, 3000, 4, false), &frame_bytes(4, 2, 8.0));
        s.stop();

        let recs = record_dirs(dir.path());
        assert_eq!(recs.len(), 2, "hole must split the record: {recs:?}");
        let m0 = read_manifest(&recs[0]);
        let m1 = read_manifest(&recs[1]);
        assert_eq!(m0.intrinsic.n_samples, 4);
        assert_eq!(m0.intrinsic.sample_index, 20);
        assert_eq!(m1.intrinsic.sample_index, 28);
        // The observed hole (28 - 24 = 4) is recorded as the second record's
        // gap_samples — honest metadata instead of a fake-continuous npy.
        assert_eq!(m1.intrinsic.gap_samples, 4);
    }

    #[test]
    fn index_less_stream_is_never_split_by_the_continuity_check() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "raw");
        // A source that never sets sample_index (always 0): the continuity
        // check must stay disarmed or every frame would become a record.
        process_one(&mut s, header(0, 0, 0, 4, true), &frame_bytes(4, 1, 0.0));
        process_one(&mut s, header(1, 0, 1_000_000, 4, false), &frame_bytes(4, 1, 4.0));
        s.stop();

        let recs = record_dirs(dir.path());
        assert_eq!(recs.len(), 1);
        assert_eq!(read_manifest(&recs[0]).intrinsic.n_samples, 8);
    }

    #[test]
    fn single_frame_capture_has_fs_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "p");
        process_one(&mut s, header(0, 0, 0, 3, true), &frame_bytes(3, 2, 0.0));
        s.stop();

        let m = read_manifest(&record_dirs(dir.path())[0]);
        assert_eq!(m.intrinsic.fs_hz, 0.0);
        // 0.0 is "no estimate", not an estimate.
        assert_eq!(m.intrinsic.fs_source, "unknown");
        use sigflow_catalog::FsSource;
        assert_eq!(m.intrinsic.fs_source(), FsSource::Unknown);
    }

    #[test]
    fn failed_commit_leaves_capture_seq_gap_as_loss_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().to_string_lossy().to_string();
        // A regular file as data_root makes create_dir_all fail inside commit.
        let bad_file = dir.path().join("not-a-dir");
        std::fs::write(&bad_file, b"x").unwrap();

        let mut s = new_storage(dir.path(), "p");
        // A segment is flushed when the NEXT capture's first frame arrives,
        // so the bad root must be in place at the flush point, not while
        // the doomed capture is buffering.
        process_one(&mut s, header(0, 0, 0, 2, true), &frame_bytes(2, 1, 0.0));
        // Capture 0 flushes fine here (root still good); capture 1 buffers.
        process_one(&mut s, header(1, 10, 1000, 2, true), &frame_bytes(2, 1, 0.0));
        // Capture 1 flushes against the bad root → lost; capture 2 buffers.
        s.set_param("data_root", &ParamValue::String(bad_file.to_string_lossy().to_string()));
        process_one(&mut s, header(2, 20, 2000, 2, true), &frame_bytes(2, 1, 0.0));
        // Capture 2 flushes fine on stop.
        s.set_param("data_root", &ParamValue::String(good));
        s.stop();

        let recs = record_dirs(dir.path());
        assert_eq!(recs.len(), 2, "captures 0 and 2 on disk: {recs:?}");
        let seqs: Vec<u64> =
            recs.iter().map(|r| read_manifest(r).provenance.capture_seq).collect();
        // The lost capture left a hole in the sequence — audit evidence.
        assert_eq!(seqs, [0, 2]);
    }

    #[test]
    fn disabled_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = new_storage(dir.path(), "p");
        s.set_param("enabled", &ParamValue::Bool(false));
        process_one(&mut s, header(0, 0, 0, 4, true), &frame_bytes(4, 2, 0.0));
        s.stop();
        assert!(record_dirs(dir.path()).is_empty());
    }
}
