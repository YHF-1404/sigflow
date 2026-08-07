//! Replay source plugin: the dual of the storage sink.
//!
//! Given a list of catalog record ULIDs (the query engine stays in the CLI —
//! this node only consumes the resulting list), it loads each record's
//! `waveform.npy`, strips the trigger-marker channels declared in the
//! manifest, and replays the signal frames out its output port.
//!
//! Re-anchoring (the ratified default): the original `t0_ns` is the
//! capture machine's CLOCK_MONOTONIC and is meaningless on another host, so
//! each emitted frame's `t0_ns` is rebuilt on THIS host's monotonic clock,
//! with frame spacing reconstructed from the record's `fs_hz`. The original
//! `t0_ns`/`fs_hz`/`sample_index`/ULID ride along in a per-record annotation
//! (`replay=true`), so a downstream storage sink that re-records preserves the
//! provenance. Records are emitted back to back, each opening with
//! `FLAG_DISCONTINUITY` so a sink re-segments one record per original capture.
//!
//! Pacing is real time: a frame's samples are released no faster than
//! `elapsed_since_record_start × fs_hz`, so VOFA reproduces the waveform at
//! its original speed and `t0_ns` stays honest. A record with `fs_hz == 0`
//! (single-frame / `fs_source = unknown`) drains as fast as the buffer allows.
//!
//! Contract lives in `manifest.toml`.

use std::fs;

use sigflow_catalog::manifest::ChannelDesc;
use sigflow_catalog::{ChannelRole, DataRoot, RecordManifest};
use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

const F32: usize = 4;

/// Schema id stamped on the per-record re-anchor annotation. Arbitrary but
/// stable — picked so it does not collide with the touch-merge annotation or
/// the test ids in this tree. The blob is JSON (see [`ReplaySource::process`]).
const REPLAY_ANNOTATION_SCHEMA_ID: u64 = 0x7265_706c_6179_5f74; // "replay_t"

/// One record loaded into memory, trigger channels already stripped.
struct LoadedRecord {
    ulid: String,
    /// `n_samples × out_channels` interleaved f32 (signal columns only).
    signal: Vec<f32>,
    out_channels: usize,
    n_samples: usize,
    fs_hz: f64,
    orig_t0_ns: i64,
    orig_sample_index: u64,
    gap_samples: u64,
}

pub struct ReplaySource {
    // --- Parameters (cached on set_param; records load in start()) ---
    data_root: String,
    ulid_list: String,
    re_anchor: bool,
    batch_size: u32,
    loop_replay: bool,

    // --- Playback state (built in start()) ---
    records: Vec<LoadedRecord>,
    cur: usize,         // index into `records`
    rec_emitted: usize, // rows emitted from the current record
    out_seq: u64,       // monotonic output frame counter
    out_sample_index: u64, // monotonic output sample index (from 0 across all records)
    rec_t0_ns: i64,     // local monotonic anchor of the current record's sample 0
    warned_ann: bool,   // annotation-overflow log throttle
}

impl ReplaySource {
    fn current(&self) -> Option<&LoadedRecord> {
        self.records.get(self.cur)
    }
}

/// Decode a NumPy `<f4` (little-endian f32) array into `(rows, cols, data)`.
/// Replay only handles the format the storage sink writes (`npy/f32-le/C`); an
/// unexpected dtype/shape is a loud error, not a silent misread.
fn decode_npy(bytes: &[u8]) -> Result<(usize, usize, Vec<f32>), String> {
    if bytes.len() < 10 || &bytes[0..6] != b"\x93NUMPY" {
        return Err("not a .npy file (bad magic)".into());
    }
    let (hlen, hstart) = match bytes[6] {
        1 => (u16::from_le_bytes([bytes[8], bytes[9]]) as usize, 10),
        2 | 3 => {
            if bytes.len() < 12 {
                return Err("truncated npy v2/3 header".into());
            }
            (u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize, 12)
        }
        v => return Err(format!("unsupported npy major version {v}")),
    };
    let header = bytes
        .get(hstart..hstart + hlen)
        .ok_or("npy header longer than file")?;
    let header = String::from_utf8_lossy(header);
    if !header.contains("<f4") {
        return Err(format!("npy descr is not '<f4' (LE f32): {header}"));
    }
    // Replay assumes C order (row-major interleaved); a Fortran-order array
    // would be silently transposed (channels↔samples). Refuse it loudly.
    if header.contains("'fortran_order': True") {
        return Err(format!("npy is Fortran-order (only C-order supported): {header}"));
    }
    let shp = header.split("'shape':").nth(1).ok_or("npy header missing shape")?;
    let inside = shp
        .split('(')
        .nth(1)
        .and_then(|s| s.split(')').next())
        .ok_or("npy shape unparseable")?;
    let dims: Vec<usize> = inside
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|x| x.parse::<usize>().map_err(|_| format!("npy shape dim '{x}'")))
        .collect::<Result<_, _>>()?;
    let (rows, cols) = match dims.as_slice() {
        [r, c] => (*r, *c),
        [r] => (*r, 1),
        _ => return Err(format!("npy shape rank {} unsupported (need 1-D or 2-D)", dims.len())),
    };
    let payload = &bytes[hstart + hlen..];
    let need = rows.checked_mul(cols).and_then(|n| n.checked_mul(F32)).ok_or("npy shape overflow")?;
    if payload.len() < need {
        return Err(format!("npy payload {} bytes < shape ({rows},{cols})×4 = {need}", payload.len()));
    }
    let data = payload[..need]
        .chunks_exact(F32)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok((rows, cols, data))
}

/// The signal column indices (everything not declared `role == trigger`). An
/// empty/absent channel list means "all columns are signal" (lenient, like a
/// raw source). Future/unknown roles are KEPT — only explicit triggers strip.
fn signal_columns(channels: &[ChannelDesc], cols: usize) -> Vec<usize> {
    if channels.is_empty() {
        return (0..cols).collect();
    }
    channels
        .iter()
        .filter(|c| c.role() != ChannelRole::Trigger)
        .map(|c| c.idx as usize)
        .filter(|&i| i < cols)
        .collect()
}

/// Split a raw param string into ULIDs (whitespace/comma/newline separated).
fn parse_ulids(raw: &str) -> Vec<String> {
    raw.split(|c: char| c.is_whitespace() || c == ',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Load one record by ULID and strip its trigger channels.
fn load_record(root: &DataRoot, ulid: &str) -> Result<LoadedRecord, String> {
    let mpath = root.record_manifest(ulid).map_err(|e| format!("{ulid}: {e}"))?;
    let mbytes = fs::read(&mpath).map_err(|e| format!("{ulid}: read manifest: {e}"))?;
    let m = RecordManifest::from_slice(&mbytes).map_err(|e| format!("{ulid}: manifest: {e}"))?;
    let dir = root.record_dir(ulid).map_err(|e| format!("{ulid}: {e}"))?;

    let art = m
        .artifacts
        .iter()
        .find(|a| a.role == "waveform")
        .or_else(|| m.artifacts.first())
        .ok_or_else(|| format!("{ulid}: record has no artifact to replay"))?;
    // v1 contract: a flat filename. Anything else is a corrupt/forged manifest
    // and would be a directory-traversal read.
    if art.path.is_empty() || art.path.contains('/') || art.path.contains('\\') || art.path == ".." {
        return Err(format!("{ulid}: artifact path '{}' is not a flat filename", art.path));
    }
    let npy = fs::read(dir.join(&art.path)).map_err(|e| format!("{ulid}: read {}: {e}", art.path))?;
    let (rows, cols, data) = decode_npy(&npy).map_err(|e| format!("{ulid}: {e}"))?;
    // A 0-row record has nothing to replay; loading it would otherwise stall
    // process() (n==0 every tick) and make every later record unreachable.
    // Refuse loudly up front (the eager-load contract), naming the ULID.
    if rows == 0 {
        return Err(format!("{ulid}: record has 0 sample rows — nothing to replay"));
    }

    let sig_cols = signal_columns(&m.intrinsic.channels, cols);
    if sig_cols.is_empty() {
        return Err(format!("{ulid}: no signal channels (all trigger?) — nothing to replay"));
    }
    let oc = sig_cols.len();
    let mut signal = Vec::with_capacity(rows * oc);
    for r in 0..rows {
        let base = r * cols;
        for &c in &sig_cols {
            signal.push(data[base + c]);
        }
    }
    Ok(LoadedRecord {
        ulid: ulid.to_string(),
        signal,
        out_channels: oc,
        n_samples: rows,
        fs_hz: m.intrinsic.fs_hz,
        orig_t0_ns: m.intrinsic.t0_ns,
        orig_sample_index: m.intrinsic.sample_index,
        gap_samples: m.intrinsic.gap_samples,
    })
}

impl Plugin for ReplaySource {
    fn new(_manifest: &PluginManifest) -> Self {
        ReplaySource {
            data_root: String::new(),
            ulid_list: String::new(),
            re_anchor: true,
            batch_size: 4096,
            loop_replay: false,
            records: Vec::new(),
            cur: 0,
            rec_emitted: 0,
            out_seq: 0,
            out_sample_index: 0,
            rec_t0_ns: 0,
            warned_ann: false,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("data_root", ParamValue::String(s)) => self.data_root = s.clone(),
            ("ulid_list", ParamValue::String(s)) => self.ulid_list = s.clone(),
            ("re_anchor", ParamValue::Bool(v)) => self.re_anchor = *v,
            ("batch_size", ParamValue::U32(v)) => self.batch_size = (*v).clamp(64, 65536),
            ("loop_replay", ParamValue::Bool(v)) => self.loop_replay = *v,
            _ => {}
        }
    }

    fn start(&mut self) -> ProcessOutcome {
        let root =
            DataRoot::new(if self.data_root.is_empty() { "captures" } else { &self.data_root });
        let ulids = parse_ulids(&self.ulid_list);
        if ulids.is_empty() {
            return ProcessOutcome::Fault {
                reason: "replay: no ULIDs to replay (set the 'ulid_list' param — e.g. \
                         `sigflow-cli data ls … --json | jq -r '.[].id'`)"
                    .into(),
            };
        }
        // Eager load: a finite, deliberate operation should refuse loudly and
        // up front on a bad/missing record (a typo'd ULID), not skip silently.
        let mut records = Vec::with_capacity(ulids.len());
        for u in &ulids {
            match load_record(&root, u) {
                Ok(r) => records.push(r),
                Err(e) => {
                    return ProcessOutcome::Fault { reason: format!("replay: cannot load record {e}") }
                }
            }
        }
        self.records = records;
        self.cur = 0;
        self.rec_emitted = 0;
        self.out_seq = 0;
        self.out_sample_index = 0;
        self.rec_t0_ns = mono_ns();
        self.warned_ann = false;
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        let Some(out) = outputs.first_mut() else {
            return ProcessOutcome::Ok;
        };
        // Exhausted: loop (re-anchoring a fresh stream) or go idle. Emitting
        // nothing leaves the ZERO header, which the shell skips (no empty
        // frame published) — so an idle source never floods downstream.
        if self.cur >= self.records.len() {
            if self.loop_replay && !self.records.is_empty() {
                self.cur = 0;
                self.rec_emitted = 0;
                self.rec_t0_ns = mono_ns();
            } else {
                return ProcessOutcome::Ok;
            }
        }
        let Some(rec) = self.current() else {
            return ProcessOutcome::Ok;
        };
        // Hoist everything we need off `rec` so the immutable borrow ends at
        // the sample copy below — the rest of the call mutates `self`.
        let oc = rec.out_channels;
        let n_samples = rec.n_samples;
        let fs = rec.fs_hz;
        let orig_t0 = rec.orig_t0_ns;
        let gap = rec.gap_samples;
        let row_bytes = oc * F32;
        if row_bytes == 0 {
            return ProcessOutcome::Ok;
        }
        let remaining = n_samples - self.rec_emitted;
        if remaining == 0 {
            // Defensive: load rejects 0-row records, but a 0-remaining record
            // must never wedge the queue — step over it this tick so later
            // records stay reachable (and a lone one reaches idle/loop).
            self.cur += 1;
            self.rec_emitted = 0;
            self.rec_t0_ns = mono_ns();
            return ProcessOutcome::Ok;
        }
        let is_first = self.rec_emitted == 0;

        // Build the per-record re-anchor annotation up front (first frame only)
        // and reserve room for it so a full frame still leaves space for it.
        let ann = if is_first {
            Some(
                serde_json::json!({
                    "replay": true,
                    "original_ulid": rec.ulid,
                    "original_t0_ns": orig_t0,
                    "original_fs_hz": fs,
                    "original_sample_index": rec.orig_sample_index,
                })
                .to_string(),
            )
        } else {
            None
        };
        let reserve = ann.as_ref().map_or(0, |s| 8 + s.len());
        let mut cap_samples = out.capacity().saturating_sub(reserve) / row_bytes;
        // Degenerate tiny buffer: drop the reserve (the annotation will be
        // skipped below) rather than wedge emitting zero forever.
        if cap_samples == 0 && out.capacity() / row_bytes > 0 {
            cap_samples = out.capacity() / row_bytes;
        }

        // Real-time pacing: release no more than elapsed×fs samples from this
        // record. fs == 0 (unknown rate) drains as fast as the buffer allows.
        let due = if fs > 0.0 {
            let elapsed = (mono_ns() - self.rec_t0_ns).max(0) as f64 / 1e9;
            (elapsed * fs).max(0.0) as u64
        } else {
            n_samples as u64
        };
        let due = (due as usize).saturating_sub(self.rec_emitted);
        let n = due.min(self.batch_size as usize).min(cap_samples).min(remaining);
        if n == 0 {
            return ProcessOutcome::Ok; // nothing due yet this tick
        }

        // Write the signal samples (interleaved f32 LE). Last use of `rec`.
        let start = self.rec_emitted * oc;
        let slice = &rec.signal[start..start + n * oc];
        let buf = out.buffer_mut();
        for (i, &v) in slice.iter().enumerate() {
            buf[i * F32..i * F32 + F32].copy_from_slice(&v.to_le_bytes());
        }
        out.set_written(n * row_bytes);

        // Header: monotonic seq/sample_index; t0 rebuilt on the local clock
        // (re-anchored) or kept absolute (strict). Offset is this frame's first
        // sample within the record.
        let off_ns = if fs > 0.0 { (self.rec_emitted as f64 / fs * 1e9) as i64 } else { 0 };
        let anchor = if self.re_anchor { self.rec_t0_ns } else { orig_t0 };
        out.header.seq = self.out_seq;
        out.header.sample_index = self.out_sample_index;
        out.header.n_samples = n as u32;
        // Saturating: a corrupt/absurd on-disk t0_ns (strict replay) must not
        // panic (debug) or silently wrap (release) when the frame offset is
        // added — keep the node alive on any input.
        out.header.t0_ns = anchor.saturating_add(off_ns);

        // First frame of each record opens a new capture: mark the boundary so
        // a downstream sink re-segments one record per original capture, and
        // attach the origin annotation (carrying the original t0/fs/ulid).
        if is_first {
            out.header.set_discontinuity(gap);
            if let Some(a) = &ann {
                if !out.set_annotation(REPLAY_ANNOTATION_SCHEMA_ID, a.as_bytes()) && !self.warned_ann
                {
                    eprintln!(
                        "replay: output buffer too small for the origin annotation \
                         ({} bytes) — emitting frames without it",
                        8 + a.len()
                    );
                    self.warned_ann = true;
                }
            }
        }

        self.out_seq += 1;
        self.out_sample_index += n as u64;
        self.rec_emitted += n;
        if self.rec_emitted >= n_samples {
            // Advance to the next record; anchor its real-time clock fresh so
            // its first frame is due immediately and re-anchors cleanly.
            self.cur += 1;
            self.rec_emitted = 0;
            self.rec_t0_ns = mono_ns();
        }
        ProcessOutcome::Ok
    }

    fn stop(&mut self) {
        // Replay holds only loaded sample buffers; nothing to flush. Keep them
        // so a restart (start()) reloads from params cleanly.
        self.records.clear();
        self.cur = 0;
        self.rec_emitted = 0;
    }
}

sigflow_plugin_sdk::export_plugin!(ReplaySource);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sigflow_catalog::manifest::{
        Artifact, CLOCK_MONOTONIC, FORMAT_VERSION, FS_SOURCE_ESTIMATED, FS_SOURCE_UNKNOWN,
        Intrinsic, Provenance, RecordManifest, ROLE_SIGNAL, ROLE_TRIGGER, STATE_COMPLETE,
    };
    use sigflow_catalog::{commit_complete_record, ArtifactPayload, DataRoot, UlidGen};
    use sigflow_plugin_sdk::{parse_annotation, FrameOut};

    fn manifest() -> PluginManifest {
        toml::from_str(include_str!("../manifest.toml")).expect("manifest parses")
    }

    /// Build a NumPy v1.0 `<f4` C-order header for `(rows, cols)`.
    fn npy(rows: usize, cols: usize, vals: &[f32]) -> Vec<u8> {
        let dict =
            format!("{{'descr': '<f4', 'fortran_order': False, 'shape': ({rows}, {cols}), }}");
        let base = 10 + dict.len() + 1;
        let pad = (64 - (base % 64)) % 64;
        let hlen = dict.len() + 1 + pad;
        let mut out = Vec::new();
        out.extend_from_slice(b"\x93NUMPY");
        out.push(1);
        out.push(0);
        out.extend_from_slice(&(hlen as u16).to_le_bytes());
        out.extend_from_slice(dict.as_bytes());
        out.extend(std::iter::repeat_n(b' ', pad));
        out.push(b'\n');
        for v in vals {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Commit a record with `signal_cols` signal columns + `trig_cols` trailing
    /// trigger columns, `rows` rows. Cell (r,c) = r*10 + c. fs_hz=0 → as-fast.
    fn fab(
        root: &DataRoot,
        gen: &mut UlidGen,
        rows: usize,
        signal_cols: usize,
        trig_cols: usize,
        fs_hz: f64,
    ) -> String {
        fab_t0(root, gen, rows, signal_cols, trig_cols, fs_hz, 123_456)
    }

    #[allow(clippy::too_many_arguments)]
    fn fab_t0(
        root: &DataRoot,
        gen: &mut UlidGen,
        rows: usize,
        signal_cols: usize,
        trig_cols: usize,
        fs_hz: f64,
        t0_ns: i64,
    ) -> String {
        let cols = signal_cols + trig_cols;
        let mut vals = Vec::with_capacity(rows * cols);
        for r in 0..rows {
            for c in 0..cols {
                vals.push((r * 10 + c) as f32);
            }
        }
        let bytes = npy(rows, cols, &vals);
        let channels: Vec<ChannelDesc> = (0..cols)
            .map(|idx| ChannelDesc {
                idx: idx as u32,
                role: if idx >= signal_cols { ROLE_TRIGGER } else { ROLE_SIGNAL }.to_string(),
                label: None,
                extra: Default::default(),
            })
            .collect();
        let id = gen.next_now();
        let mut m = RecordManifest {
            format_version: FORMAT_VERSION.into(),
            id: id.clone(),
            state: STATE_COMPLETE.into(),
            provenance: Provenance {
                session_id: None,
                graph: None,
                node_id: "rec0".into(),
                plugin: "sigflow.io.storage@0.1.0".into(),
                host: "test".into(),
                capture_seq: 0,
                stream_label: "pen0".into(),
                trigger_t0_ns: None,
                created_utc: "2026-06-13T00:00:00.000Z".into(),
                extra: Default::default(),
            },
            intrinsic: Intrinsic {
                t0_ns,
                clock: CLOCK_MONOTONIC.into(),
                fs_hz,
                fs_source: if fs_hz > 0.0 { FS_SOURCE_ESTIMATED } else { FS_SOURCE_UNKNOWN }.into(),
                channels,
                n_samples: rows as u64,
                sample_index: 7,
                gap_samples: 0,
                annotations: vec![],
                extra: Default::default(),
            },
            artifacts: Vec::<Artifact>::new(),
            extra: Default::default(),
        };
        commit_complete_record(
            root,
            &mut m,
            &[ArtifactPayload {
                role: "waveform",
                path: "waveform.npy",
                format: "npy/f32-le/C/(n,ch)",
                bytes: &bytes,
            }],
        )
        .unwrap();
        id
    }

    fn replay_for(root: &DataRoot, ulids: &str) -> ReplaySource {
        let mut p = ReplaySource::new(&manifest());
        p.set_param("data_root", &ParamValue::String(root.path().to_string_lossy().into()));
        p.set_param("ulid_list", &ParamValue::String(ulids.to_string()));
        p
    }

    /// One process() tick into a fresh 64 KiB buffer; returns (header, payload).
    fn tick(p: &mut ReplaySource) -> (sigflow_plugin_sdk::FrameHeader, Vec<u8>) {
        let mut buf = vec![0u8; 64 * 1024];
        let (h, w) = {
            let mut outs = [FrameOut::new(&mut buf)];
            p.process(&[], &mut outs);
            (outs[0].header, outs[0].written())
        };
        (h, buf[..w].to_vec())
    }

    fn f32s(bytes: &[u8]) -> Vec<f32> {
        bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    #[test]
    fn manifest_is_a_source_with_expected_contract() {
        let m = manifest();
        assert_eq!(m.name, "sigflow.io.replay_source");
        assert_eq!(m.ports.len(), 1);
        assert_eq!(m.ports[0].id, "signal_out");
        // No consumer port → the shell treats it as a source.
        let ids: Vec<&str> = m.parameters.iter().map(|p| p.id.as_str()).collect();
        for id in ["data_root", "ulid_list", "re_anchor", "batch_size", "loop_replay"] {
            assert!(ids.contains(&id), "missing param {id}");
        }
    }

    #[test]
    fn decode_npy_roundtrips_shape_and_values() {
        let bytes = npy(3, 2, &[0.0, 1.0, 10.0, 11.0, 20.0, 21.0]);
        let (r, c, d) = decode_npy(&bytes).unwrap();
        assert_eq!((r, c), (3, 2));
        assert_eq!(d, vec![0.0, 1.0, 10.0, 11.0, 20.0, 21.0]);
        assert!(decode_npy(b"not npy").is_err());
    }

    #[test]
    fn start_strips_trigger_channels_and_faults_on_bad_ulid() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        // 2 signal + 1 trigger column, fs=0 (as-fast).
        let a = fab(&root, &mut gen, 3, 2, 1, 0.0);

        let mut p = replay_for(&root, &a);
        assert!(matches!(p.start(), ProcessOutcome::Ok));
        assert_eq!(p.records.len(), 1);
        assert_eq!(p.records[0].out_channels, 2, "trigger column stripped");
        assert_eq!(p.records[0].n_samples, 3);
        // Row r signal cols are r*10+0, r*10+1 (col 2 = trigger dropped).
        assert_eq!(p.records[0].signal, vec![0.0, 1.0, 10.0, 11.0, 20.0, 21.0]);

        // A bogus ULID makes start() refuse, naming it.
        let mut bad = replay_for(&root, "01JXAB3C4D5E6F7G8H9JKMNPQR");
        match bad.start() {
            ProcessOutcome::Fault { reason } => assert!(reason.contains("cannot load record"), "{reason}"),
            o => panic!("expected Fault, got {o:?}"),
        }
        // Empty list also refuses.
        let mut empty = replay_for(&root, "   ");
        assert!(matches!(empty.start(), ProcessOutcome::Fault { .. }));
    }

    #[test]
    fn replays_signal_with_discontinuity_and_origin_annotation() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 3, 2, 1, 0.0); // fs=0 → drains in one tick

        let mut p = replay_for(&root, &a);
        p.start();
        let (h, payload) = tick(&mut p);
        // First frame: discontinuity + annotation; samples are signal-only.
        assert!(h.is_discontinuity(), "first frame opens a capture");
        assert!(h.is_annotated());
        assert_eq!(h.n_samples, 3);
        assert_eq!(h.sample_index, 0, "output index is monotonic from 0");
        let f = Frame::new(h, &payload);
        assert_eq!(f32s(f.samples()), vec![0.0, 1.0, 10.0, 11.0, 20.0, 21.0]);
        let (sid, blob) = parse_annotation(f.annotation().unwrap()).unwrap();
        assert_eq!(sid, REPLAY_ANNOTATION_SCHEMA_ID);
        let j: serde_json::Value = serde_json::from_slice(blob).unwrap();
        assert_eq!(j["replay"], true);
        assert_eq!(j["original_ulid"], a);
        assert_eq!(j["original_t0_ns"], 123_456);
        assert_eq!(j["original_sample_index"], 7);

        // Re-anchored: t0 is on THIS host's monotonic clock, not the stored
        // 123456 — it must be far larger (real mono_ns) and not the original.
        assert_ne!(h.t0_ns, 123_456, "t0 re-anchored to local clock");
        assert!(h.t0_ns > 0);

        // Record exhausted → idle (ZERO header, shell skips it).
        let (h2, p2) = tick(&mut p);
        assert_eq!(h2, sigflow_plugin_sdk::FrameHeader::ZERO);
        assert!(p2.is_empty());
    }

    #[test]
    fn two_records_advance_each_opening_with_discontinuity() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 2, 1, 0, 0.0);
        let b = fab(&root, &mut gen, 2, 1, 0, 0.0);

        let mut p = replay_for(&root, &format!("{a}\n{b}"));
        p.start();
        let (h1, pay1) = tick(&mut p); // record a
        assert!(h1.is_discontinuity() && h1.is_annotated());
        assert_eq!(f32s(Frame::new(h1, &pay1).samples()), vec![0.0, 10.0]);
        let (h2, pay2) = tick(&mut p); // record b
        assert!(h2.is_discontinuity(), "each record opens a new capture");
        assert!(h2.is_annotated());
        assert_eq!(f32s(Frame::new(h2, &pay2).samples()), vec![0.0, 10.0]);
        // Output sample_index is monotonic across records (a: 0..2, b: 2..4).
        assert_eq!(h1.sample_index, 0);
        assert_eq!(h2.sample_index, 2);
        // Origin annotation distinguishes the two records.
        let id_of = |h, pay: &[u8]| -> String {
            let f = Frame::new(h, pay);
            let (_, blob) = parse_annotation(f.annotation().unwrap()).unwrap();
            serde_json::from_slice::<serde_json::Value>(blob).unwrap()["original_ulid"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(id_of(h1, &pay1), a);
        assert_eq!(id_of(h2, &pay2), b);
        // Then idle.
        assert_eq!(tick(&mut p).0, sigflow_plugin_sdk::FrameHeader::ZERO);
    }

    #[test]
    fn loop_replay_wraps_instead_of_idling() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 2, 1, 0, 0.0);
        let mut p = replay_for(&root, &a);
        p.set_param("loop_replay", &ParamValue::Bool(true));
        p.start();
        let (h1, _) = tick(&mut p);
        assert!(h1.n_samples == 2);
        // Exhausted, but loop wraps: the next non-idle tick replays record a
        // again (discontinuity reopened), index keeps climbing.
        let (h2, pay2) = tick(&mut p);
        assert!(h2.is_discontinuity());
        assert_eq!(f32s(Frame::new(h2, &pay2).samples()), vec![0.0, 10.0]);
        assert_eq!(h2.sample_index, 2, "monotonic across the wrap");
    }

    #[test]
    fn no_re_anchor_keeps_original_absolute_t0() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 2, 1, 0, 0.0);
        let mut p = replay_for(&root, &a);
        p.set_param("re_anchor", &ParamValue::Bool(false));
        p.start();
        let (h, _) = tick(&mut p);
        // fs=0 → offset 0 → t0 is the stored absolute value verbatim.
        assert_eq!(h.t0_ns, 123_456, "strict replay keeps the original t0");
    }

    #[test]
    fn zero_row_record_is_refused_at_load() {
        // A degenerate (0-row) record must Fault loudly at load (naming the
        // ULID), never silently wedge the source queue.
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let z = fab_t0(&root, &mut gen, 0, 1, 0, 0.0, 123_456);
        let mut p = replay_for(&root, &z);
        match p.start() {
            ProcessOutcome::Fault { reason } => {
                assert!(reason.contains("0 sample rows") && reason.contains(&z), "{reason}")
            }
            o => panic!("expected Fault on a 0-row record, got {o:?}"),
        }
    }

    #[test]
    fn fortran_order_npy_is_refused() {
        // C-order is assumed; a Fortran-order array would be silently
        // transposed (channels↔samples) — refuse it instead.
        let dict = "{'descr': '<f4', 'fortran_order': True, 'shape': (2, 1), }";
        let base = 10 + dict.len() + 1;
        let pad = (64 - (base % 64)) % 64;
        let hlen = dict.len() + 1 + pad;
        let mut b = Vec::new();
        b.extend_from_slice(b"\x93NUMPY");
        b.push(1);
        b.push(0);
        b.extend_from_slice(&(hlen as u16).to_le_bytes());
        b.extend_from_slice(dict.as_bytes());
        b.extend(std::iter::repeat_n(b' ', pad));
        b.push(b'\n');
        b.extend_from_slice(&[0u8; 8]); // 2 f32
        let err = decode_npy(&b).unwrap_err();
        assert!(err.contains("Fortran-order"), "{err}");
    }

    #[test]
    fn strict_replay_near_i64_max_t0_does_not_overflow() {
        // Strict replay (no re-anchor) of a record whose stored t0 is i64::MAX:
        // emitting a later frame adds a positive offset to the anchor, which
        // would panic (debug) / wrap (release) without saturating_add.
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab_t0(&root, &mut gen, 1000, 1, 0, 2000.0, i64::MAX);
        let mut p = replay_for(&root, &a);
        p.set_param("re_anchor", &ParamValue::Bool(false));
        p.set_param("batch_size", &ParamValue::U32(64)); // many frames, never drained at once
        p.start();
        std::thread::sleep(std::time::Duration::from_millis(60));
        let (h0, _) = tick(&mut p); // first frame: offset 0 → t0 == i64::MAX
        assert_eq!(h0.t0_ns, i64::MAX);
        std::thread::sleep(std::time::Duration::from_millis(60));
        let (h1, _) = tick(&mut p); // later frame: offset > 0 → saturates, no panic
        assert!(h1.n_samples > 0, "second frame must emit (exercise the add)");
        assert_eq!(h1.t0_ns, i64::MAX, "saturating_add clamps instead of overflowing");
    }

    #[test]
    fn real_time_pacing_releases_at_the_record_rate() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        // 1000 samples at 2000 Hz = 0.5 s of data; one channel.
        let a = fab(&root, &mut gen, 1000, 1, 0, 2000.0);
        let mut p = replay_for(&root, &a);
        p.start();
        // Immediately: ~0 due (no elapsed time yet).
        let (h0, _) = tick(&mut p);
        assert!(h0.n_samples < 100, "real-time pacing must not dump the record at once: {}", h0.n_samples);
        // After ~120 ms, ~240 samples are due (2000 Hz × 0.12 s); allow slack.
        std::thread::sleep(std::time::Duration::from_millis(120));
        let (h1, _) = tick(&mut p);
        let total = h0.n_samples + h1.n_samples;
        assert!(total >= 100 && total <= 600, "paced count out of range: {total}");
    }
}

