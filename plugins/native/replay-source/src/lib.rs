//! Replay source plugin: the dual of the storage sink.
//!
//! Given a list of catalog record ULIDs (the query engine stays in the CLI —
//! this node only consumes the resulting list), it loads each record's
//! `waveform.npy`, strips the trigger-marker channels declared in the
//! manifest, and replays the signal frames out its output port.
//!
//! Transport: `mode` decides who supplies the beat — the operator (`step`),
//! the wall clock (`play`, at the record's own `fs_hz` × `speed_x`), or nobody
//! (`flood`: every tick emits as much as the frame buffer allows, for offline
//! processing). The read head moves freely (step/jog/seek/reset), but the
//! EMITTED stream only ever moves forward: `seq`/`sample_index` stay monotonic
//! and every jump opens a new capture (`FLAG_DISCONTINUITY` + a fresh origin
//! annotation naming the source row). Downstream sinks re-segment on that
//! boundary instead of being told time went backwards.
//!
//! Re-anchoring (the ratified default): the original `t0_ns` is the
//! capture machine's CLOCK_MONOTONIC and is meaningless on another host, so
//! each emitted frame's `t0_ns` is rebuilt on THIS host's monotonic clock,
//! with frame spacing reconstructed from the record's `fs_hz` (× `speed_x`).
//! The original `t0_ns`/`fs_hz`/`sample_index`/ULID ride along in a per-record
//! annotation (`replay=true`), so a downstream storage sink that re-records
//! preserves the provenance.
//!
//! Loading: record MANIFESTS load eagerly at `start()` (a typo'd ULID, a
//! 0-row record or an all-trigger record refuses loudly and up front, and the
//! row totals the read head needs are known before the first frame); WAVEFORMS
//! load lazily with a small cache, so a 4000-record list does not have to fit
//! in memory at once.
//!
//! Contract lives in `manifest.toml`.

use std::fs;
use std::path::PathBuf;

use sigflow_catalog::manifest::ChannelDesc;
use sigflow_catalog::{ChannelRole, DataRoot, RecordManifest};
use sigflow_plugin_sdk::{
    mono_ns, Frame, FrameOut, ParamValue, Plugin, PluginManifest, ProcessOutcome,
};

const F32: usize = 4;

/// Waveforms kept in memory: the current record plus one neighbour, so
/// stepping across a record boundary (forwards or backwards) does not re-read
/// the file every press.
const CACHE_RECORDS: usize = 2;

/// Position updates are telemetry, not data: one per jump, otherwise at most
/// this often (a 1 kHz position stream would swamp the widget tap for nothing).
const POSITION_MIN_INTERVAL_NS: i64 = 100_000_000; // 10 Hz

/// Schema id stamped on the per-record re-anchor annotation. Arbitrary but
/// stable — picked so it does not collide with the touch-merge annotation or
/// the test ids in this tree. The blob is JSON (see [`ReplaySource::process`]).
const REPLAY_ANNOTATION_SCHEMA_ID: u64 = 0x7265_706c_6179_5f74; // "replay_t"

/// Schema id for the read-head telemetry blob on the `position` port.
const POSITION_ANNOTATION_SCHEMA_ID: u64 = 0x7265_706c_6179_5f70; // "replay_p"

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Step,
    Play,
    Flood,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        match s {
            "step" => Some(Mode::Step),
            "play" => Some(Mode::Play),
            "flood" => Some(Mode::Flood),
            _ => None,
        }
    }
}

/// One record's identity and geometry — everything the read head needs before
/// the waveform itself is touched.
struct RecordMeta {
    ulid: String,
    dir: PathBuf,
    artifact: String,
    /// Signal column indices; empty means "the manifest declared no channels,
    /// take every column" (resolved when the waveform is read).
    sig_cols: Vec<usize>,
    /// 0 = unknown until the waveform is read (manifest declared no channels).
    out_channels: usize,
    n_samples: usize,
    fs_hz: f64,
    orig_t0_ns: i64,
    orig_sample_index: u64,
    gap_samples: u64,
    /// First global row of this record (prefix sum over the list).
    start_row: usize,
}

pub struct ReplaySource {
    // --- Parameters (cached on set_param; records load in start()) ---
    data_root: String,
    ulid_list: String,
    re_anchor: bool,
    batch_size: u32,
    loop_replay: bool,
    mode: Mode,
    playing: bool,
    speed_x: f64,
    step_rows: u32,
    jog_s: f64,

    // --- Playlist (built in start()) ---
    records: Vec<RecordMeta>,
    total_rows: usize,
    /// (record index, signal rows interleaved) — at most [`CACHE_RECORDS`].
    cache: Vec<(usize, Vec<f32>)>,

    // --- Read head / playback state ---
    cur: usize, // index into `records`; == len() means "past the end"
    row: usize, // next row to emit within `records[cur]`
    pending: usize, // rows the operator queued (step mode)
    jump: bool, // next frame opens a new capture (seek/reset/wrap)
    anchor_ns: i64, // local monotonic anchor for play-mode pacing…
    anchor_row: usize, // …and the global row it corresponds to
    out_seq: u64,
    out_sample_index: u64,
    pos_seq: u64,
    pos_last_ns: i64,
    pos_force: bool,
    warned_ann: bool, // annotation-overflow log throttle
    fault: Option<String>, // lazy-load failure, reported on the next process()
}

impl ReplaySource {
    fn current(&self) -> Option<&RecordMeta> {
        self.records.get(self.cur)
    }

    fn global_row(&self) -> usize {
        match self.records.get(self.cur) {
            Some(m) => m.start_row + self.row,
            None => self.total_rows,
        }
    }

    /// Restart play-mode pacing from here and now (after a seek, a pause, a
    /// speed change or a record change — the record's fs is part of the rate).
    fn reanchor(&mut self) {
        self.anchor_ns = mono_ns();
        self.anchor_row = self.global_row();
    }

    /// Move the read head to a global row. The emitted stream does not move
    /// back with it: the next frame just opens a new capture.
    fn seek_global(&mut self, pos: usize) {
        let pos = pos.min(self.total_rows);
        match self.records.iter().rposition(|m| m.start_row <= pos) {
            Some(i) if pos < self.total_rows => {
                self.cur = i;
                self.row = pos - self.records[i].start_row;
            }
            _ => {
                self.cur = self.records.len();
                self.row = 0;
            }
        }
        self.jump = true;
        self.pos_force = true;
        self.reanchor();
        // Not running free (step mode, or paused): queue one step so the face
        // shows where the head landed instead of keeping the stale frame.
        if self.mode == Mode::Step || !self.playing {
            self.pending = self.pending.max(self.step_rows as usize);
        }
    }

    fn seek_relative(&mut self, delta: i64) {
        let here = self.global_row() as i64;
        self.seek_global((here + delta).max(0) as usize);
    }

    /// Rows corresponding to `jog_s` at the current record's rate (a record
    /// with no declared rate jogs by one batch instead).
    fn jog_rows(&self) -> i64 {
        match self.current() {
            Some(m) if m.fs_hz > 0.0 => (self.jog_s * m.fs_hz).round().max(1.0) as i64,
            _ => self.batch_size as i64,
        }
    }

    fn ensure_loaded(&mut self, idx: usize) -> Result<(), String> {
        if self.cache.iter().any(|(i, _)| *i == idx) {
            return Ok(());
        }
        let (signal, oc) = load_waveform(&self.records[idx])?;
        self.records[idx].out_channels = oc;
        if self.cache.len() >= CACHE_RECORDS {
            self.cache.remove(0);
        }
        self.cache.push((idx, signal));
        Ok(())
    }

    fn waveform(&self, idx: usize) -> Option<&[f32]> {
        self.cache.iter().find(|(i, _)| *i == idx).map(|(_, v)| v.as_slice())
    }

    /// Read-head telemetry: ONE channel carrying the global row, so a plain
    /// numeric_display (whose tap stats are aggregates over all channels)
    /// reads the position exactly. The rest — which record, how long it is,
    /// how far through the list — rides in the annotation.
    fn emit_position(&mut self, out: &mut FrameOut) {
        let now = mono_ns();
        if !self.pos_force && now.saturating_sub(self.pos_last_ns) < POSITION_MIN_INTERVAL_NS {
            return;
        }
        let (idx, row, rows, ulid) = match self.current() {
            Some(m) => (self.cur, self.row, m.n_samples, m.ulid.clone()),
            None => (self.records.len(), 0, 0, String::new()),
        };
        let global = self.global_row();
        let ann = serde_json::json!({
            "record_index": idx,
            "records_total": self.records.len(),
            "ulid": ulid,
            "row_in_record": row,
            "rows_in_record": rows,
            "global_row": global,
            "rows_total": self.total_rows,
        })
        .to_string();
        if out.capacity() < F32 {
            return;
        }
        let buf = out.buffer_mut();
        buf[..F32].copy_from_slice(&(global as f32).to_le_bytes());
        out.set_written(F32);
        out.set_annotation(POSITION_ANNOTATION_SCHEMA_ID, ann.as_bytes());
        out.header.seq = self.pos_seq;
        out.header.sample_index = self.pos_seq;
        out.header.n_samples = 1;
        out.header.t0_ns = now;
        self.pos_seq += 1;
        self.pos_last_ns = now;
        self.pos_force = false;
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

/// Read one record's manifest: identity, geometry, rate. No waveform yet.
fn load_meta(root: &DataRoot, ulid: &str, start_row: usize) -> Result<RecordMeta, String> {
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
    // A 0-row record has nothing to replay; keeping it would stall the head
    // (n==0 every tick). Refuse loudly up front, naming the ULID.
    let n_samples = m.intrinsic.n_samples as usize;
    if n_samples == 0 {
        return Err(format!("{ulid}: record has 0 sample rows — nothing to replay"));
    }
    let declared = m.intrinsic.channels.len();
    let sig_cols = signal_columns(&m.intrinsic.channels, declared);
    if declared > 0 && sig_cols.is_empty() {
        return Err(format!("{ulid}: no signal channels (all trigger?) — nothing to replay"));
    }
    Ok(RecordMeta {
        ulid: ulid.to_string(),
        dir,
        artifact: art.path.clone(),
        out_channels: sig_cols.len(), // 0 = manifest declared none → resolved on load
        sig_cols,
        n_samples,
        fs_hz: m.intrinsic.fs_hz,
        orig_t0_ns: m.intrinsic.t0_ns,
        orig_sample_index: m.intrinsic.sample_index,
        gap_samples: m.intrinsic.gap_samples,
        start_row,
    })
}

/// Read one record's waveform, trigger columns stripped. Returns the
/// interleaved signal rows and the resolved channel count.
fn load_waveform(meta: &RecordMeta) -> Result<(Vec<f32>, usize), String> {
    let ulid = &meta.ulid;
    let npy = fs::read(meta.dir.join(&meta.artifact))
        .map_err(|e| format!("{ulid}: read {}: {e}", meta.artifact))?;
    let (rows, cols, data) = decode_npy(&npy).map_err(|e| format!("{ulid}: {e}"))?;
    // The manifest's row count is what the read head was built from; a
    // disagreeing array would make every seek land somewhere else.
    if rows != meta.n_samples {
        return Err(format!(
            "{ulid}: waveform has {rows} rows but the manifest says {} — record is inconsistent",
            meta.n_samples
        ));
    }
    let sig_cols: Vec<usize> =
        if meta.sig_cols.is_empty() { (0..cols).collect() } else { meta.sig_cols.clone() };
    if sig_cols.is_empty() {
        return Err(format!("{ulid}: no signal channels — nothing to replay"));
    }
    if let Some(&max) = sig_cols.iter().max() {
        if max >= cols {
            return Err(format!(
                "{ulid}: manifest declares channel {max} but the waveform has {cols} columns"
            ));
        }
    }
    let oc = sig_cols.len();
    let mut signal = Vec::with_capacity(rows * oc);
    for r in 0..rows {
        let base = r * cols;
        for &c in &sig_cols {
            signal.push(data[base + c]);
        }
    }
    Ok((signal, oc))
}

impl Plugin for ReplaySource {
    fn new(_manifest: &PluginManifest) -> Self {
        ReplaySource {
            data_root: String::new(),
            ulid_list: String::new(),
            re_anchor: true,
            batch_size: 4096,
            loop_replay: false,
            mode: Mode::Play,
            playing: true,
            speed_x: 1.0,
            step_rows: 1,
            jog_s: 1.0,
            records: Vec::new(),
            total_rows: 0,
            cache: Vec::new(),
            cur: 0,
            row: 0,
            pending: 0,
            jump: false,
            anchor_ns: 0,
            anchor_row: 0,
            out_seq: 0,
            out_sample_index: 0,
            pos_seq: 0,
            pos_last_ns: 0,
            pos_force: true,
            warned_ann: false,
            fault: None,
        }
    }

    fn set_param(&mut self, id: &str, value: &ParamValue) {
        match (id, value) {
            ("data_root", ParamValue::String(s)) => self.data_root = s.clone(),
            ("ulid_list", ParamValue::String(s)) => self.ulid_list = s.clone(),
            ("re_anchor", ParamValue::Bool(v)) => self.re_anchor = *v,
            ("batch_size", ParamValue::U32(v)) => self.batch_size = (*v).clamp(64, 65536),
            ("loop_replay", ParamValue::Bool(v)) => self.loop_replay = *v,
            ("mode", ParamValue::Enum(s)) | ("mode", ParamValue::String(s)) => {
                if let Some(m) = Mode::parse(s) {
                    if m != self.mode {
                        self.mode = m;
                        self.pending = 0;
                        self.reanchor(); // a mode change restarts the beat
                    }
                }
            }
            ("playing", ParamValue::Bool(v)) => {
                if *v != self.playing {
                    self.playing = *v;
                    self.reanchor(); // resume from here, not from the pause hole
                }
            }
            ("speed_x", ParamValue::F64(v)) => {
                let v = v.clamp(0.05, 100.0);
                if v != self.speed_x {
                    self.speed_x = v;
                    self.reanchor(); // the old anchor was in old-speed units
                }
            }
            ("step_rows", ParamValue::U32(v)) => self.step_rows = (*v).clamp(1, 65536),
            ("jog_s", ParamValue::F64(v)) => self.jog_s = v.clamp(0.01, 3600.0),
            // Imperative: writing it seeks (see manifest). Before start() the
            // playlist is empty and seek_global clamps to 0 — harmless.
            ("seek_row", ParamValue::U32(v)) => self.seek_global(*v as usize),
            _ => {}
        }
    }

    fn invoke_action(&mut self, id: &str) {
        match id {
            "reset" => self.seek_global(0),
            "step_next" => self.pending += self.step_rows as usize,
            // Back up by one step and queue it: the head lands on the previous
            // frame AND emits it (a stepper that moves silently is useless).
            "step_prev" => {
                let back = self.step_rows as i64;
                self.seek_relative(-back);
                self.pending = self.pending.max(self.step_rows as usize);
            }
            "jog_fwd" => {
                let d = self.jog_rows();
                self.seek_relative(d);
            }
            "jog_back" => {
                let d = self.jog_rows();
                self.seek_relative(-d);
            }
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
        // Manifests eagerly: a bad/missing record (a typo'd ULID) must refuse
        // loudly and up front, and the read head needs every record's length
        // before the first seek. Waveforms stay on disk until touched.
        let mut records = Vec::with_capacity(ulids.len());
        let mut start_row = 0usize;
        for u in &ulids {
            match load_meta(&root, u, start_row) {
                Ok(r) => {
                    start_row += r.n_samples;
                    records.push(r);
                }
                Err(e) => {
                    return ProcessOutcome::Fault { reason: format!("replay: cannot load record {e}") }
                }
            }
        }
        // A list whose records disagree on channel count would change the
        // stream's shape mid-flight; downstream nodes size themselves once.
        let widths: Vec<usize> =
            records.iter().map(|r| r.out_channels).filter(|&c| c > 0).collect();
        if let Some(&first) = widths.first() {
            if let Some(bad) = records.iter().find(|r| r.out_channels > 0 && r.out_channels != first)
            {
                return ProcessOutcome::Fault {
                    reason: format!(
                        "replay: mixed channel counts in the list ({first} vs {} in {}) — \
                         replay one geometry at a time",
                        bad.out_channels, bad.ulid
                    ),
                };
            }
        }
        self.total_rows = start_row;
        self.records = records;
        self.cache.clear();
        self.cur = 0;
        self.row = 0;
        self.pending = 0;
        self.jump = false;
        self.out_seq = 0;
        self.out_sample_index = 0;
        self.pos_seq = 0;
        self.pos_last_ns = 0;
        self.pos_force = true;
        self.warned_ann = false;
        self.fault = None;
        self.reanchor();
        ProcessOutcome::Ok
    }

    fn process(&mut self, _inputs: &[Frame], outputs: &mut [FrameOut]) -> ProcessOutcome {
        if let Some(reason) = self.fault.take() {
            return ProcessOutcome::Fault { reason };
        }
        if self.records.is_empty() || outputs.is_empty() {
            return ProcessOutcome::Ok;
        }

        // --- This tick's row budget: who supplies the beat ------------------
        let fs = self.current().map(|m| m.fs_hz).unwrap_or(0.0);
        let budget = match self.mode {
            Mode::Step => self.pending,
            Mode::Play if !self.playing => 0,
            // fs == 0 (single-frame / unknown rate) has no beat to keep: drain
            // it as fast as the buffer allows, like flood.
            Mode::Play if fs <= 0.0 => usize::MAX,
            Mode::Play => {
                let elapsed = mono_ns().saturating_sub(self.anchor_ns).max(0) as f64 / 1e9;
                let due = self.anchor_row as f64 + elapsed * fs * self.speed_x;
                (due.max(0.0) as usize).saturating_sub(self.global_row())
            }
            Mode::Flood => usize::MAX,
        };

        // --- Past the end: wrap or idle -------------------------------------
        if self.cur >= self.records.len() {
            if self.loop_replay && budget > 0 && self.mode != Mode::Step {
                self.seek_global(0); // next tick emits from the top
            } else if self.loop_replay && self.pending > 0 {
                self.seek_global(0);
            }
            if let Some(pos) = outputs.get_mut(1) {
                self.emit_position(pos);
            }
            return ProcessOutcome::Ok;
        }
        if budget == 0 {
            if let Some(pos) = outputs.get_mut(1) {
                self.emit_position(pos);
            }
            return ProcessOutcome::Ok;
        }

        // Waveform on demand (a read error faults, like a bad ULID does).
        let cur = self.cur;
        if let Err(e) = self.ensure_loaded(cur) {
            return ProcessOutcome::Fault { reason: format!("replay: {e}") };
        }

        let (oc, n_samples, meta_fs, orig_t0, orig_index, gap, ulid) = {
            let m = &self.records[cur];
            (m.out_channels, m.n_samples, m.fs_hz, m.orig_t0_ns, m.orig_sample_index, m.gap_samples, m.ulid.clone())
        };
        let row_bytes = oc * F32;
        if row_bytes == 0 {
            return ProcessOutcome::Ok;
        }
        let remaining = n_samples - self.row;
        let is_open = self.jump || self.row == 0;

        // Build the origin annotation up front (segment-opening frames only)
        // and reserve room for it so a full frame still leaves space.
        let ann = if is_open {
            Some(
                serde_json::json!({
                    "replay": true,
                    "original_ulid": ulid,
                    "original_t0_ns": orig_t0,
                    "original_fs_hz": meta_fs,
                    "original_sample_index": orig_index + self.row as u64,
                    "record_index": cur,
                    "row": self.row,
                })
                .to_string(),
            )
        } else {
            None
        };

        let n = {
            let out = &mut outputs[0];
            let reserve = ann.as_ref().map_or(0, |s| 8 + s.len());
            let mut cap_samples = out.capacity().saturating_sub(reserve) / row_bytes;
            // Degenerate tiny buffer: drop the reserve (the annotation will be
            // skipped below) rather than wedge emitting zero forever.
            if cap_samples == 0 && out.capacity() / row_bytes > 0 {
                cap_samples = out.capacity() / row_bytes;
            }
            budget.min(self.batch_size as usize).min(cap_samples).min(remaining)
        };
        if n == 0 {
            if let Some(pos) = outputs.get_mut(1) {
                self.emit_position(pos);
            }
            return ProcessOutcome::Ok;
        }

        // Header: monotonic seq/sample_index; t0 rebuilt on the local clock
        // (re-anchored) or kept absolute (strict replay).
        let t0 = if !self.re_anchor {
            let off = if meta_fs > 0.0 { (self.row as f64 / meta_fs * 1e9) as i64 } else { 0 };
            // Saturating: a corrupt/absurd on-disk t0_ns must not panic (debug)
            // or silently wrap (release) when the frame offset is added.
            orig_t0.saturating_add(off)
        } else if self.mode == Mode::Play && meta_fs > 0.0 {
            // Paced: keep the frames on the release grid so the arrival rate
            // downstream reads as fs × speed_x, not as tick jitter.
            let rows_since = self.global_row().saturating_sub(self.anchor_row) as f64;
            let off = (rows_since / (meta_fs * self.speed_x) * 1e9) as i64;
            self.anchor_ns.saturating_add(off)
        } else {
            mono_ns() // step/flood: the frame's samples arrive now
        };

        {
            let out = &mut outputs[0];
            let start = self.row * oc;
            let Some(wf) = self.waveform(cur) else {
                return ProcessOutcome::Ok; // unreachable: ensure_loaded above
            };
            let slice = &wf[start..start + n * oc];
            let buf = out.buffer_mut();
            for (i, &v) in slice.iter().enumerate() {
                buf[i * F32..i * F32 + F32].copy_from_slice(&v.to_le_bytes());
            }
            out.set_written(n * row_bytes);
            out.header.seq = self.out_seq;
            out.header.sample_index = self.out_sample_index;
            out.header.n_samples = n as u32;
            out.header.t0_ns = t0;

            // A segment-opening frame marks the boundary so a downstream sink
            // re-segments, and carries the origin annotation. A jump's gap is
            // 0: the rows before it were not "lost", the head moved.
            if is_open {
                out.header.set_discontinuity(if self.jump { 0 } else { gap });
                if let Some(a) = &ann {
                    if !out.set_annotation(REPLAY_ANNOTATION_SCHEMA_ID, a.as_bytes())
                        && !self.warned_ann
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
        }

        self.out_seq += 1;
        self.out_sample_index += n as u64;
        self.row += n;
        self.jump = false;
        self.pending = self.pending.saturating_sub(n);
        if self.row >= n_samples {
            // Next record: anchor its pacing fresh (its fs may differ) so its
            // first frame is due immediately.
            self.cur += 1;
            self.row = 0;
            self.reanchor();
            self.pos_force = true;
        }
        if let Some(pos) = outputs.get_mut(1) {
            self.emit_position(pos);
        }
        ProcessOutcome::Ok
    }

    fn stop(&mut self) {
        self.records.clear();
        self.cache.clear();
        self.total_rows = 0;
        self.cur = 0;
        self.row = 0;
        self.pending = 0;
    }
}

sigflow_plugin_sdk::export_plugin!(ReplaySource);

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

    /// A tick with the position port attached; returns (global_row, details).
    fn tick_pos(p: &mut ReplaySource) -> Option<(f32, serde_json::Value)> {
        let mut sig = vec![0u8; 64 * 1024];
        let mut pos = vec![0u8; 4 * 1024];
        let (h, w) = {
            let (a, b) = (FrameOut::new(&mut sig), FrameOut::new(&mut pos));
            let mut outs = [a, b];
            p.process(&[], &mut outs);
            (outs[1].header, outs[1].written())
        };
        if w == 0 {
            return None;
        }
        let f = Frame::new(h, &pos[..w]);
        let (_, blob) = parse_annotation(f.annotation().unwrap()).unwrap();
        Some((f32s(f.samples())[0], serde_json::from_slice(blob).unwrap()))
    }

    fn f32s(bytes: &[u8]) -> Vec<f32> {
        bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    fn set_mode(p: &mut ReplaySource, m: &str) {
        p.set_param("mode", &ParamValue::Enum(m.into()));
    }

    #[test]
    fn manifest_is_a_source_with_expected_contract() {
        let m = manifest();
        assert_eq!(m.name, "sigflow.io.replay_source");
        // Two producer ports, no consumer → the shell treats it as a source.
        assert_eq!(m.ports.len(), 2);
        assert_eq!(m.ports[0].id, "signal_out");
        assert_eq!(m.ports[1].id, "position");
        let ids: Vec<&str> = m.parameters.iter().map(|p| p.id.as_str()).collect();
        for id in [
            "data_root", "ulid_list", "re_anchor", "batch_size", "loop_replay", "mode", "playing",
            "speed_x", "step_rows", "jog_s", "seek_row",
        ] {
            assert!(ids.contains(&id), "missing param {id}");
        }
        let acts: Vec<&str> = m.actions.iter().map(|a| a.id.as_str()).collect();
        for id in ["reset", "step_next", "step_prev", "jog_fwd", "jog_back"] {
            assert!(acts.contains(&id), "missing action {id}");
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
        assert_eq!(p.total_rows, 3);
        // Waveforms are lazy: nothing read until the first frame is due.
        assert!(p.cache.is_empty(), "waveform must not load at start()");
        p.ensure_loaded(0).unwrap();
        // Row r signal cols are r*10+0, r*10+1 (col 2 = trigger dropped).
        assert_eq!(p.waveform(0).unwrap(), &[0.0, 1.0, 10.0, 11.0, 20.0, 21.0]);

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
        // Exhausted: the wrap tick repositions, the next one replays record a
        // again (discontinuity reopened), index keeps climbing.
        let _ = tick(&mut p);
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
    fn mixed_channel_counts_are_refused_at_start() {
        // The stream's shape is fixed once downstream; a list that changes it
        // mid-flight must refuse up front, naming the offender.
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 2, 2, 0, 0.0);
        let b = fab(&root, &mut gen, 2, 3, 0, 0.0);
        let mut p = replay_for(&root, &format!("{a} {b}"));
        match p.start() {
            ProcessOutcome::Fault { reason } => {
                assert!(reason.contains("mixed channel counts") && reason.contains(&b), "{reason}")
            }
            o => panic!("expected Fault, got {o:?}"),
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

    #[test]
    fn speed_multiplies_the_release_rate() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 100_000, 1, 0, 2000.0);
        let mut p = replay_for(&root, &a);
        p.set_param("speed_x", &ParamValue::F64(10.0));
        p.set_param("batch_size", &ParamValue::U32(65536));
        p.start();
        let _ = tick(&mut p);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let (h, _) = tick(&mut p);
        // 10× of 2000 Hz over ~100 ms ≈ 2000 rows (vs ~200 at 1×). Wide band:
        // this asserts the multiplier applies, not the scheduler's precision.
        assert!(h.n_samples > 700, "10× speed should release far more: {}", h.n_samples);
    }

    #[test]
    fn paused_play_emits_nothing_and_resumes_from_the_same_row() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 10_000, 1, 0, 2000.0);
        let mut p = replay_for(&root, &a);
        p.start();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let (h0, _) = tick(&mut p);
        let after_first = h0.n_samples as usize;
        p.set_param("playing", &ParamValue::Bool(false));
        std::thread::sleep(std::time::Duration::from_millis(80));
        let (h1, _) = tick(&mut p);
        assert_eq!(h1, sigflow_plugin_sdk::FrameHeader::ZERO, "paused emits nothing");
        assert_eq!(p.row, after_first, "the read head stays put while paused");
        // Resume: the pause hole is NOT owed — pacing re-anchors on resume.
        p.set_param("playing", &ParamValue::Bool(true));
        let (h2, _) = tick(&mut p);
        assert!(
            (h2.n_samples as usize) < 200,
            "resume must not dump the paused interval: {}",
            h2.n_samples
        );
    }

    #[test]
    fn step_mode_emits_only_on_step_and_walks_both_ways() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 5, 1, 0, 100.0); // rows 0,10,20,30,40
        let mut p = replay_for(&root, &a);
        set_mode(&mut p, "step");
        p.start();
        // Idle until a button is pressed.
        assert_eq!(tick(&mut p).0, sigflow_plugin_sdk::FrameHeader::ZERO);

        p.invoke_action("step_next");
        let (h1, pay1) = tick(&mut p);
        assert_eq!(h1.n_samples, 1);
        assert_eq!(f32s(Frame::new(h1, &pay1).samples()), vec![0.0]);
        p.invoke_action("step_next");
        let (h2, pay2) = tick(&mut p);
        assert_eq!(f32s(Frame::new(h2, &pay2).samples()), vec![10.0]);
        assert_eq!(h2.sample_index, 1, "output index keeps climbing");

        // Back one frame: the head moves back, the STREAM does not — the frame
        // opens a new capture and its annotation names the source row.
        p.invoke_action("step_prev");
        let (h3, pay3) = tick(&mut p);
        assert_eq!(f32s(Frame::new(h3, &pay3).samples()), vec![10.0], "re-emits the previous row");
        assert!(h3.is_discontinuity(), "a jump opens a new capture");
        assert!(h3.sample_index > h2.sample_index, "stream time never goes back");
        let f = Frame::new(h3, &pay3);
        let (_, blob) = parse_annotation(f.annotation().unwrap()).unwrap();
        let j: serde_json::Value = serde_json::from_slice(blob).unwrap();
        assert_eq!(j["row"], 1, "annotation names the source row");
        assert_eq!(j["original_sample_index"], 8, "= stored sample_index (7) + row");
    }

    #[test]
    fn reset_returns_to_the_first_frame() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 4, 1, 0, 100.0);
        let b = fab(&root, &mut gen, 4, 1, 0, 100.0);
        let mut p = replay_for(&root, &format!("{a} {b}"));
        set_mode(&mut p, "step");
        p.set_param("step_rows", &ParamValue::U32(2));
        p.start();
        for _ in 0..3 {
            p.invoke_action("step_next");
            let _ = tick(&mut p);
        }
        assert!(p.global_row() > 0);
        // Reset repositions AND queues a step, so the face is not left stale.
        p.invoke_action("reset");
        assert_eq!(p.global_row(), 0);
        let (h, pay) = tick(&mut p);
        assert!(h.is_discontinuity());
        assert_eq!(f32s(Frame::new(h, &pay).samples()), vec![0.0, 10.0]);
    }

    #[test]
    fn seek_row_param_jumps_and_clamps() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 4, 1, 0, 100.0);
        let b = fab(&root, &mut gen, 4, 1, 0, 100.0);
        let mut p = replay_for(&root, &format!("{a} {b}"));
        set_mode(&mut p, "step");
        p.start();
        // Global row 5 = record b, row 1.
        p.set_param("seek_row", &ParamValue::U32(5));
        assert_eq!((p.cur, p.row), (1, 1));
        let (_, pay) = tick(&mut p); // the seek queued its own step
        assert_eq!(f32s(&pay)[0], 10.0);
        // Past the end clamps to the end (idle / loop point), never panics.
        p.set_param("seek_row", &ParamValue::U32(9999));
        assert_eq!(p.global_row(), 8);
    }

    #[test]
    fn jog_moves_by_seconds_of_recorded_time() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 1000, 1, 0, 100.0); // 10 s at 100 Hz
        let mut p = replay_for(&root, &a);
        p.set_param("jog_s", &ParamValue::F64(2.0));
        p.start();
        p.invoke_action("jog_fwd");
        assert_eq!(p.global_row(), 200, "2 s × 100 Hz");
        p.invoke_action("jog_back");
        assert_eq!(p.global_row(), 0);
        p.invoke_action("jog_back"); // clamps at the start, no underflow
        assert_eq!(p.global_row(), 0);
    }

    #[test]
    fn flood_mode_fills_frames_regardless_of_the_clock() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        // 10 000 rows at 100 Hz = 100 s of data — paced, that is 100 seconds;
        // flooded, it must come out in a handful of ticks.
        let a = fab(&root, &mut gen, 10_000, 1, 0, 100.0);
        let mut p = replay_for(&root, &a);
        set_mode(&mut p, "flood");
        p.set_param("batch_size", &ParamValue::U32(4096));
        p.start();
        let mut total = 0usize;
        let mut ticks = 0;
        loop {
            let (h, _) = tick(&mut p);
            if h == sigflow_plugin_sdk::FrameHeader::ZERO {
                break;
            }
            total += h.n_samples as usize;
            ticks += 1;
            assert!(ticks < 20, "flood should drain in a few big frames");
        }
        assert_eq!(total, 10_000, "every row must come out exactly once");
        assert!(ticks <= 4, "expected ~3 frames of 4096, got {ticks}");
    }

    #[test]
    fn position_port_reports_the_read_head() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab(&root, &mut gen, 4, 1, 0, 100.0);
        let b = fab(&root, &mut gen, 6, 1, 0, 100.0);
        let mut p = replay_for(&root, &format!("{a} {b}"));
        set_mode(&mut p, "step");
        p.set_param("step_rows", &ParamValue::U32(2));
        p.start();
        p.invoke_action("step_next");
        let (global, d) = tick_pos(&mut p).expect("position reported");
        assert_eq!(global, 2.0, "one 2-row step in");
        assert_eq!(d["record_index"], 0);
        assert_eq!(d["row_in_record"], 2);
        assert_eq!(d["rows_in_record"], 4);
        assert_eq!(d["rows_total"], 10);
        assert_eq!(d["records_total"], 2);
        // A jump forces an update even inside the 10 Hz throttle window.
        p.set_param("seek_row", &ParamValue::U32(7));
        let (global, d) = tick_pos(&mut p).expect("a jump reports immediately");
        assert_eq!(d["record_index"], 1, "record b");
        // The seek queued its own step (2 rows), so the head reports 7 + 2.
        assert_eq!(global, 9.0, "global row after the queued step");
    }
}
