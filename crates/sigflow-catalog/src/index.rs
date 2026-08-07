//! The rebuildable SQLite index — `_index/catalog.sqlite`.
//!
//! Truth lives in the files; this is a CACHE. The write path is strictly
//! one-way (files → index): the sink never touches `_index/`, and `reindex`
//! can rebuild everything from the records tree at any time. Deleting the
//! database loses nothing.
//!
//! Reindex does three jobs in one walk:
//! 1. **Verify** record directories (via [`crate::adopt`]) — incremental by
//!    default: a previously indexed, byte-identical directory (manifest stat
//!    unchanged, no residue, no undeclared files) is trusted without
//!    re-hashing, because a complete record is immutable by contract.
//!    `full` re-verifies every hash.
//! 2. **Act** on the adoption verdicts (the ratified table): delete `.tmp`
//!    residue and stale `.recording` manifests, salvage usable recording
//!    residue into `state=salvaged` finals, delete provably empty
//!    directories, and alert (never touch) everything doubtful. All actions
//!    respect an age guard — a recently modified directory may be a live
//!    sink mid-write — and `dry_run` reports without acting.
//! 3. **Materialise attributes**: the four-layer resolved view goes into
//!    `record_attrs(record_id, key, value, value_num, source_layer)`. This
//!    pass always runs in full — annotation files live OUTSIDE the record
//!    directories, so directory-level change detection cannot see an edit;
//!    re-resolving every indexed record is just JSON reads and is cheap.
//!
//! Attribute value encoding: `value` holds the JSON encoding of the value
//! (strings quoted), `value_num` holds a numeric projection (NULL for
//! non-numbers) for range filters like `dt_ms<20`. Known limit: numeric
//! comparisons go through an f64 projection, so integers beyond 2^53
//! (e.g. a far-future `t0_ns`) compare approximately; exact-match those via
//! their string form if it ever matters.
//!
//! Concurrency: one reindex at a time — the whole run holds an EXCLUSIVE
//! SQLite transaction, so a second concurrent reindex fails fast with
//! SQLITE_BUSY instead of interleaving (two walkers would prune each
//! other's freshly indexed rows). Readers are unaffected (WAL). The disk
//! actions are idempotent and re-converge on the next run regardless.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;

use crate::adopt::{check_record_dir, ArtifactStatus, ScanOutcome, Verdict};
use crate::annot::{read_annotation_file, AnnotationFile};
use crate::atomic::write_atomic;
use crate::layout::DataRoot;
use crate::manifest::{
    Artifact, RecordManifest, RecordState, SessionManifest, RECORDING_SUFFIX, RECORD_MANIFEST,
    SESSION_MANIFEST, STATE_SALVAGED, TMP_SUFFIX,
};
use crate::resolve::{
    effective_session_id, resolve_record_attrs, resolve_session_attrs, ResolvedAttr,
};
use crate::{utc_now_rfc3339, CatalogError};

/// Bump on any schema change: an old database is dropped and rebuilt
/// (it is a cache — nothing is lost).
const SCHEMA_VERSION: i64 = 1;

pub const KIND_RECORD: &str = "record";
pub const KIND_SESSION: &str = "session";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ReindexOptions {
    /// Drop the index first and re-verify every artifact hash.
    pub full: bool,
    /// Report all actions without touching the index or the disk.
    pub dry_run: bool,
    /// Directories whose newest mtime is younger than this are exempt from
    /// every mutating action (a live sink may be mid-write). Verification
    /// and indexing still happen. Default 60 s (see [`ReindexOptions::new`]).
    pub min_age: Option<Duration>,
}

impl ReindexOptions {
    pub fn min_age(&self) -> Duration {
        self.min_age.unwrap_or(Duration::from_secs(60))
    }
}

/// One alert from reindex. Persisted in the `problems` table (rebuilt every
/// run) so they remain inspectable after the terminal scrolls away.
#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    /// What the alert is about: a directory path (relative to the data
    /// root), a ULID, or an annotation file path.
    pub target: String,
    pub category: String,
    pub detail: String,
}

// Problem categories.
pub const PROBLEM_CORRUPT: &str = "corrupt";
pub const PROBLEM_VERSION_REFUSED: &str = "version-refused";
pub const PROBLEM_ORPHAN: &str = "orphan";
pub const PROBLEM_SCAN_ERROR: &str = "scan-error";
pub const PROBLEM_NOT_A_ULID: &str = "not-a-ulid";
pub const PROBLEM_UNEXPECTED_ENTRY: &str = "unexpected-entry";
pub const PROBLEM_SALVAGE_REFUSED: &str = "salvage-refused";
pub const PROBLEM_SALVAGE_PARTIAL: &str = "salvage-partial";
pub const PROBLEM_ANNOTATION_UNREADABLE: &str = "annotation-unreadable";
pub const PROBLEM_DANGLING_ANNOTATION: &str = "dangling-annotation";
pub const PROBLEM_SESSION_MISSING: &str = "session-missing";
pub const PROBLEM_STATE_MISMATCH: &str = "state-mismatch";
pub const PROBLEM_SESSION_ID_INVALID: &str = "session-id-invalid";

#[derive(Debug, Default, Serialize)]
pub struct ReindexReport {
    pub scanned_dirs: u64,
    pub skipped_unchanged: u64,
    pub indexed_records: u64,
    pub indexed_sessions: u64,
    pub salvaged: u64,
    pub deleted_dirs: u64,
    pub tmp_removed: u64,
    pub stale_recordings_removed: u64,
    /// Index rows whose directory no longer exists on disk.
    pub pruned_rows: u64,
    /// Records whose attribute view was (re)materialised.
    pub attrs_records: u64,
    pub dry_run: bool,
    pub problems: Vec<Problem>,
}

/// One resolved attribute as stored/returned by the index.
#[derive(Debug, Clone, Serialize)]
pub struct AttrOut {
    pub value: Value,
    pub layer: String,
}

/// One queryable row (record or session) with its resolved attribute view.
#[derive(Debug, Serialize)]
pub struct RecordRow {
    pub id: String,
    pub kind: String,
    pub state: String,
    /// Directory relative to the data root.
    pub dir: String,
    pub created_utc: String,
    pub stream_label: Option<String>,
    pub node_id: Option<String>,
    /// Effective (four-layer resolved) session id.
    pub session_id: Option<String>,
    pub n_samples: Option<u64>,
    pub fs_hz: Option<f64>,
    pub attrs: BTreeMap<String, AttrOut>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Exists,
}

/// One `key<op>value` filter from the CLI (`stream_label=pen0`, `dt_ms<20`,
/// a bare `matched` means "key exists").
#[derive(Debug, Clone)]
pub struct Filter {
    pub key: String,
    pub op: FilterOp,
    /// JSON encoding of the comparison value (strings quoted) — matches the
    /// `record_attrs.value` column encoding.
    pub value_json: String,
    /// Numeric projection when the value is a number.
    pub value_num: Option<f64>,
}

impl Filter {
    /// Parse one CLI filter expression. Ordering comparisons require a
    /// numeric right-hand side. An unquoted value that parses as JSON keeps
    /// its JSON type (`true`, `3.5`, `null`); anything else is a string.
    pub fn parse(expr: &str) -> Result<Filter, CatalogError> {
        // Two-char ops first so `!=` does not parse as key `x!` op `=`.
        for (tok, op) in [
            ("!=", FilterOp::Ne),
            ("<=", FilterOp::Le),
            (">=", FilterOp::Ge),
            ("=", FilterOp::Eq),
            ("<", FilterOp::Lt),
            (">", FilterOp::Gt),
        ] {
            if let Some((k, v)) = expr.split_once(tok) {
                let key = k.trim();
                let raw = v.trim();
                if key.is_empty() {
                    return Err(CatalogError::Invalid(format!("filter '{expr}': empty key")));
                }
                let value: Value = serde_json::from_str(raw)
                    .unwrap_or_else(|_| Value::String(raw.to_string()));
                let value_num = value.as_f64();
                if matches!(op, FilterOp::Lt | FilterOp::Le | FilterOp::Gt | FilterOp::Ge)
                    && value_num.is_none()
                {
                    return Err(CatalogError::Invalid(format!(
                        "filter '{expr}': ordering comparison needs a numeric value"
                    )));
                }
                return Ok(Filter {
                    key: key.to_string(),
                    op,
                    value_json: value.to_string(),
                    value_num,
                });
            }
        }
        let key = expr.trim();
        if key.is_empty() {
            return Err(CatalogError::Invalid("empty filter expression".into()));
        }
        Ok(Filter {
            key: key.to_string(),
            op: FilterOp::Exists,
            value_json: String::new(),
            value_num: None,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct QueryOpts {
    /// Query sessions instead of records.
    pub sessions: bool,
    /// Include tombstoned entries (annotation key `deleted` set).
    pub include_deleted: bool,
    /// Keep only the most recent N (output stays in chronological order).
    pub limit: Option<u64>,
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

pub struct Index {
    conn: Connection,
    root: DataRoot,
    /// True when open() found an older schema and rebuilt the database —
    /// the caller should tell the user to run `reindex`.
    pub schema_was_reset: bool,
}

impl Index {
    /// Open (or create) `_index/catalog.sqlite` under the data root.
    ///
    /// A torn/garbage database file (power loss mid-write, partial copy) is
    /// deleted and recreated: the index is a rebuildable cache, so wedging
    /// every command until a human clears `_index/` would be strictly worse.
    pub fn open(root: &DataRoot) -> Result<Self, CatalogError> {
        let db = root.index_dir().join("catalog.sqlite");
        match Self::open_at(&db, root) {
            Err(CatalogError::Sqlite(e)) if is_not_a_database(&e) => {
                for suffix in ["", "-wal", "-shm"] {
                    let mut p = db.as_os_str().to_owned();
                    p.push(suffix);
                    let _ = fs::remove_file(std::path::PathBuf::from(p));
                }
                let mut ix = Self::open_at(&db, root)?;
                ix.schema_was_reset = true;
                Ok(ix)
            }
            other => other,
        }
    }

    fn open_at(db: &Path, root: &DataRoot) -> Result<Self, CatalogError> {
        fs::create_dir_all(root.index_dir())?;
        let conn = Connection::open(db)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let found: Option<i64> = conn
            .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::SqliteFailure(..) | rusqlite::Error::QueryReturnedNoRows => {
                    Ok(None)
                }
                other => Err(other),
            })?;
        let schema_was_reset = match found {
            Some(v) if v == SCHEMA_VERSION => false,
            Some(_) => {
                conn.execute_batch(
                    "DROP TABLE IF EXISTS record_attrs;
                     DROP TABLE IF EXISTS records;
                     DROP TABLE IF EXISTS problems;
                     DROP TABLE IF EXISTS meta;",
                )?;
                true
            }
            None => false,
        };

        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value);
             INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', {SCHEMA_VERSION});
             CREATE TABLE IF NOT EXISTS records(
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 state TEXT NOT NULL,
                 dir TEXT NOT NULL,
                 created_utc TEXT NOT NULL,
                 stream_label TEXT,
                 node_id TEXT,
                 session_id TEXT,
                 n_samples INTEGER,
                 fs_hz REAL,
                 manifest_json TEXT NOT NULL,
                 scan_mtime_ns INTEGER NOT NULL,
                 scan_size INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS record_attrs(
                 record_id TEXT NOT NULL REFERENCES records(id) ON DELETE CASCADE,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 value_num REAL,
                 source_layer TEXT NOT NULL,
                 PRIMARY KEY(record_id, key)
             );
             CREATE INDEX IF NOT EXISTS idx_attrs_kv ON record_attrs(key, value);
             CREATE INDEX IF NOT EXISTS idx_attrs_knum ON record_attrs(key, value_num);
             CREATE TABLE IF NOT EXISTS problems(
                 target TEXT NOT NULL,
                 category TEXT NOT NULL,
                 detail TEXT NOT NULL,
                 seen_utc TEXT NOT NULL
             );"
        ))?;

        Ok(Index { conn, root: root.clone(), schema_was_reset })
    }

    // -- reindex ------------------------------------------------------------

    pub fn reindex(&mut self, opts: &ReindexOptions) -> Result<ReindexReport, CatalogError> {
        let mut report = ReindexReport { dry_run: opts.dry_run, ..Default::default() };
        let root = self.root.clone();

        // One reindex at a time: the whole run holds one EXCLUSIVE
        // transaction, so a concurrent reindex fails fast (SQLITE_BUSY
        // after busy_timeout) instead of interleaving — two walkers would
        // prune each other's freshly indexed rows. WAL keeps readers
        // (`ls`/`show`) unaffected. The commit also makes the index update
        // atomic: a crashed reindex leaves the previous coherent state.
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Exclusive)?;

        if opts.full && !opts.dry_run {
            tx.execute_batch(
                "DELETE FROM record_attrs; DELETE FROM records; DELETE FROM problems;",
            )?;
        }

        let mut seen: Vec<String> = Vec::new();
        walk_records(&tx, &root, opts, &mut report, &mut seen)?;

        if !opts.dry_run {
            report.pruned_rows = prune_missing(&tx, &seen)?;
            materialize_attrs(&tx, &root, &mut report)?;
            detect_dangling_annotations(&tx, &root, &mut report)?;
            persist_problems(&tx, &report.problems)?;
        }
        tx.commit()?;
        Ok(report)
    }

    // -- query ----------------------------------------------------------------

    pub fn query(
        &self,
        filters: &[Filter],
        opts: &QueryOpts,
    ) -> Result<Vec<RecordRow>, CatalogError> {
        let mut sql = String::from(
            "SELECT id, kind, state, dir, created_utc, stream_label, node_id, session_id,
                    n_samples, fs_hz
             FROM records r WHERE r.kind = ?",
        );
        let mut params: Vec<SqlValue> = vec![SqlValue::Text(
            if opts.sessions { KIND_SESSION } else { KIND_RECORD }.into(),
        )];

        if !opts.include_deleted {
            // Tombstone semantics mirror AnnotationFile::is_deleted (present
            // and not false/null → hidden) — but ONLY from the record
            // annotation layer: deletion is a human annotation by contract,
            // so a capture-time blob field or a session attr named `deleted`
            // must not be able to forge invisibility.
            sql.push_str(
                " AND NOT EXISTS(SELECT 1 FROM record_attrs a WHERE a.record_id=r.id
                   AND a.key='deleted' AND a.source_layer='record_annotation'
                   AND a.value NOT IN ('false','null'))",
            );
        }

        for f in filters {
            match f.op {
                FilterOp::Exists => {
                    sql.push_str(
                        " AND EXISTS(SELECT 1 FROM record_attrs a
                           WHERE a.record_id=r.id AND a.key=?)",
                    );
                    params.push(SqlValue::Text(f.key.clone()));
                }
                FilterOp::Eq | FilterOp::Ne => {
                    let clause = if let Some(n) = f.value_num {
                        params.push(SqlValue::Text(f.key.clone()));
                        params.push(SqlValue::Real(n));
                        "EXISTS(SELECT 1 FROM record_attrs a WHERE a.record_id=r.id
                          AND a.key=? AND a.value_num=?)"
                    } else {
                        params.push(SqlValue::Text(f.key.clone()));
                        params.push(SqlValue::Text(f.value_json.clone()));
                        "EXISTS(SELECT 1 FROM record_attrs a WHERE a.record_id=r.id
                          AND a.key=? AND a.value=?)"
                    };
                    if f.op == FilterOp::Ne {
                        sql.push_str(" AND NOT ");
                    } else {
                        sql.push_str(" AND ");
                    }
                    sql.push_str(clause);
                }
                FilterOp::Lt | FilterOp::Le | FilterOp::Gt | FilterOp::Ge => {
                    let cmp = match f.op {
                        FilterOp::Lt => "<",
                        FilterOp::Le => "<=",
                        FilterOp::Gt => ">",
                        _ => ">=",
                    };
                    sql.push_str(&format!(
                        " AND EXISTS(SELECT 1 FROM record_attrs a WHERE a.record_id=r.id
                           AND a.key=? AND a.value_num {cmp} ?)"
                    ));
                    params.push(SqlValue::Text(f.key.clone()));
                    params.push(SqlValue::Real(f.value_num.expect("checked at parse")));
                }
            }
        }

        // Most-recent-N semantics with chronological output.
        if let Some(n) = opts.limit {
            sql.push_str(" ORDER BY r.id DESC LIMIT ?");
            params.push(SqlValue::Integer(n as i64));
        } else {
            sql.push_str(" ORDER BY r.id ASC");
        }

        let mut rows: Vec<RecordRow> = self
            .conn
            .prepare(&sql)?
            .query_map(rusqlite::params_from_iter(params), |r| {
                Ok(RecordRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    state: r.get(2)?,
                    dir: r.get(3)?,
                    created_utc: r.get(4)?,
                    stream_label: r.get(5)?,
                    node_id: r.get(6)?,
                    session_id: r.get(7)?,
                    n_samples: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                    fs_hz: r.get(9)?,
                    attrs: BTreeMap::new(),
                })
            })?
            .collect::<Result<_, _>>()?;
        if opts.limit.is_some() {
            rows.reverse();
        }

        let mut attr_stmt = self.conn.prepare(
            "SELECT key, value, source_layer FROM record_attrs WHERE record_id=?",
        )?;
        for row in &mut rows {
            let it = attr_stmt.query_map([&row.id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for a in it {
                let (k, v, layer) = a?;
                let value: Value = serde_json::from_str(&v).unwrap_or(Value::String(v));
                row.attrs.insert(k, AttrOut { value, layer });
            }
        }
        Ok(rows)
    }

    /// One row by ULID (any kind, tombstoned included — `show` must show).
    pub fn record(&self, ulid: &str) -> Result<Option<RecordRow>, CatalogError> {
        let mut rows = self.query(
            &[Filter {
                key: "id".into(),
                op: FilterOp::Eq,
                value_json: Value::String(ulid.to_string()).to_string(),
                value_num: None,
            }],
            &QueryOpts { include_deleted: true, ..Default::default() },
        )?;
        if rows.is_empty() {
            // Sessions carry `id` too, but live under kind=session.
            rows = self.query(
                &[Filter {
                    key: "id".into(),
                    op: FilterOp::Eq,
                    value_json: Value::String(ulid.to_string()).to_string(),
                    value_num: None,
                }],
                &QueryOpts { sessions: true, include_deleted: true, ..Default::default() },
            )?;
        }
        Ok(rows.into_iter().next())
    }

    /// Problems persisted by the last (non-dry-run) reindex.
    pub fn problems(&self) -> Result<Vec<Problem>, CatalogError> {
        let rows = self
            .conn
            .prepare("SELECT target, category, detail FROM problems ORDER BY rowid")?
            .query_map([], |r| {
                Ok(Problem { target: r.get(0)?, category: r.get(1)?, detail: r.get(2)? })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
}

/// Pass A: walk `records/<shard>/<ulid>/`, verify (or trust unchanged),
/// act on verdicts, upsert rows.
fn walk_records(
    conn: &Connection,
    root: &DataRoot,
    opts: &ReindexOptions,
    report: &mut ReindexReport,
    seen: &mut Vec<String>,
) -> Result<(), CatalogError> {
    let records_dir = root.records_dir();
    let shards = match fs::read_dir(&records_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for shard in shards {
        let shard = shard?;
        let shard_name = shard.file_name().to_string_lossy().into_owned();
        if !shard.file_type()?.is_dir() {
            report.problems.push(Problem {
                target: format!("records/{shard_name}"),
                category: PROBLEM_UNEXPECTED_ENTRY.into(),
                detail: "file directly under records/ (expected shard directories)".into(),
            });
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = format!("records/{shard_name}/{name}");
            if !entry.file_type()?.is_dir() {
                report.problems.push(Problem {
                    target: rel,
                    category: PROBLEM_UNEXPECTED_ENTRY.into(),
                    detail: "file directly under a shard (expected record directories)"
                        .into(),
                });
                continue;
            }
            if !crate::id::is_valid_ulid(&name) || !name.starts_with(&shard_name) {
                report.problems.push(Problem {
                    target: rel,
                    category: PROBLEM_NOT_A_ULID.into(),
                    detail: "directory name is not a ULID in its shard — left untouched"
                        .into(),
                });
                continue;
            }
            report.scanned_dirs += 1;
            scan_one(conn, opts, report, seen, &entry.path(), &name, &rel)?;
        }
    }
    Ok(())
}

fn scan_one(
    conn: &Connection,
    opts: &ReindexOptions,
    report: &mut ReindexReport,
    seen: &mut Vec<String>,
    dir: &Path,
    ulid: &str,
    rel: &str,
) -> Result<(), CatalogError> {
    // Fast path: trust a previously indexed, unchanged directory.
    if !opts.full {
        match unchanged(conn, dir, ulid) {
            Ok(true) => {
                report.skipped_unchanged += 1;
                seen.push(ulid.to_string());
                return Ok(());
            }
            Ok(false) => {}
            Err(e) => {
                // Fall through to the deep scan, which will classify it.
                let _ = e;
            }
        }
    }

    let out = match check_record_dir(dir) {
        Ok(o) => o,
        Err(e) => {
            // A read error is not proof of anything: alert, keep the
            // directory AND any existing index row (conservative).
            report.problems.push(Problem {
                target: rel.into(),
                category: PROBLEM_SCAN_ERROR.into(),
                detail: format!("scan failed: {e} — directory and index row left as-is"),
            });
            seen.push(ulid.to_string());
            return Ok(());
        }
    };

    // The age guard: a recently modified directory may be a live sink
    // mid-write — no mutating action may touch it, and no negative verdict
    // (Corrupt/Orphan) may be passed on it either: the artifacts-written/
    // manifest-not-yet-renamed window of a normal commit scans as Orphan,
    // which must defer to the next run rather than raise a false alarm and
    // de-index.
    let recent = dir_is_recent(dir, opts.min_age())?;
    let act = !opts.dry_run && !recent;

    // Always-safe cleanup actions from the adoption ruling.
    if out.adoption.delete_tmp && act {
        report.tmp_removed += remove_matching(dir, |n| n.ends_with(TMP_SUFFIX))?;
    }
    if out.adoption.delete_stale_recording && act {
        let p = dir.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}"));
        if fs::remove_file(&p).is_ok() {
            report.stale_recordings_removed += 1;
        }
    }

    match out.adoption.verdict {
        Verdict::Complete => {
            seen.push(ulid.to_string());
            if let Some(m) = &out.manifest {
                // The filename carries the state; a final manifest claiming
                // anything but complete/salvaged is a contradiction — the
                // filename wins, and the contract says alert.
                if !matches!(m.state(), RecordState::Complete | RecordState::Salvaged) {
                    report.problems.push(Problem {
                        target: rel.into(),
                        category: PROBLEM_STATE_MISMATCH.into(),
                        detail: format!(
                            "final manifest carries state '{}' — filename (final) wins",
                            m.state
                        ),
                    });
                }
            }
            if out.facts.undeclared_files > 0 {
                report.problems.push(Problem {
                    target: rel.into(),
                    category: PROBLEM_UNEXPECTED_ENTRY.into(),
                    detail: format!(
                        "{} file(s) not declared by the manifest — kept",
                        out.facts.undeclared_files
                    ),
                });
            }
            if opts.dry_run {
                return Ok(());
            }
            if out.facts.is_session {
                upsert_session(conn, dir, ulid, rel)?;
                report.indexed_sessions += 1;
            } else if let Some(m) = &out.manifest {
                upsert_record(conn, dir, ulid, rel, m)?;
                report.indexed_records += 1;
            }
        }
        Verdict::Corrupt => {
            seen.push(ulid.to_string());
            if recent {
                return Ok(()); // possibly a live commit: defer judgment
            }
            let (category, detail) = match &out.facts.version_refused {
                Some(v) => (
                    PROBLEM_VERSION_REFUSED,
                    format!("manifest format_version '{v}' is newer than this tool reads — upgrade the tool"),
                ),
                None => (
                    PROBLEM_CORRUPT,
                    "final manifest unreadable, artifacts missing/corrupt, or conflicting identity — kept for investigation".to_string(),
                ),
            };
            report.problems.push(Problem { target: rel.into(), category: category.into(), detail });
            // A previously healthy row must not keep vouching for a now
            // unprovable record.
            if !opts.dry_run {
                conn.execute("DELETE FROM records WHERE id=?", [ulid])?;
            }
        }
        Verdict::Salvage { partial } => {
            seen.push(ulid.to_string());
            if !act {
                if opts.dry_run {
                    report.salvaged += 1; // would salvage
                }
                return Ok(());
            }
            match salvage(dir, ulid, rel, &out, partial, report)? {
                Some(m) => {
                    upsert_record(conn, dir, ulid, rel, &m)?;
                    report.indexed_records += 1;
                    report.salvaged += 1;
                }
                None => {
                    // Refused (problem already recorded). Same rule as
                    // Corrupt: a previously healthy row must not keep
                    // vouching for a directory that no longer proves out.
                    conn.execute("DELETE FROM records WHERE id=?", [ulid])?;
                }
            }
        }
        Verdict::FailedEmpty | Verdict::EmptyDir => {
            if !act {
                if opts.dry_run {
                    report.deleted_dirs += 1; // would delete
                }
                seen.push(ulid.to_string());
                return Ok(());
            }
            fs::remove_dir_all(dir)?;
            report.deleted_dirs += 1;
            // NOT pushed to seen: the prune below drops any stale row.
        }
        Verdict::Orphan => {
            seen.push(ulid.to_string());
            if recent {
                return Ok(()); // possibly a live commit: defer judgment
            }
            report.problems.push(Problem {
                target: rel.into(),
                category: PROBLEM_ORPHAN.into(),
                detail: format!(
                    "{} data file(s) without a usable manifest — never auto-adopted",
                    out.facts.undeclared_files
                ),
            });
            if !opts.dry_run {
                conn.execute("DELETE FROM records WHERE id=?", [ulid])?;
            }
        }
    }
    Ok(())
}

/// Fast-path check: the directory is exactly what the index already
/// vouched for — final manifest stat unchanged, entries are exactly the
/// manifest plus its declared artifacts (no residue, nothing undeclared).
fn unchanged(conn: &Connection, dir: &Path, ulid: &str) -> Result<bool, CatalogError> {
    let row: Option<(String, i64, i64, String)> = conn.query_row(
            "SELECT kind, scan_mtime_ns, scan_size, manifest_json FROM records WHERE id=?",
            [ulid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some((kind, mtime_ns, size, manifest_json)) = row else {
        return Ok(false);
    };

    let manifest_name = if kind == KIND_SESSION { SESSION_MANIFEST } else { RECORD_MANIFEST };
    let mut expected: Vec<String> = vec![manifest_name.to_string()];
    let mut declared: Vec<(String, u64)> = Vec::new();
    if kind == KIND_RECORD {
        let m: RecordManifest = match RecordManifest::from_slice(manifest_json.as_bytes()) {
            Ok(m) => m,
            Err(_) => return Ok(false),
        };
        expected.extend(m.artifacts.iter().map(|a| a.path.clone()));
        declared = m.artifacts.iter().map(|a| (a.path.clone(), a.size_bytes)).collect();
    }

    let st = match fs::metadata(dir.join(manifest_name)) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    if file_stat(&st) != (mtime_ns, size) {
        return Ok(false);
    }
    // Artifacts: a complete record is immutable, so a declared-size
    // mismatch or an mtime later than the manifest commit means someone
    // touched it after the fact → deep scan. (True bit rot — content
    // changed, stat unchanged — is only caught by `reindex --full`.)
    for (path, declared_size) in &declared {
        let ast = match fs::metadata(dir.join(path)) {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };
        let (a_mtime, a_size) = file_stat(&ast);
        if a_size != *declared_size as i64 || a_mtime > mtime_ns {
            return Ok(false);
        }
    }
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if !expected.contains(&name) {
            return Ok(false); // residue or undeclared file → deep scan
        }
    }
    Ok(true)
}

fn upsert_record(
    conn: &Connection,
    dir: &Path,
    ulid: &str,
    rel: &str,
    m: &RecordManifest,
) -> Result<(), CatalogError> {
    let st = fs::metadata(dir.join(RECORD_MANIFEST))?;
    let (mtime_ns, size) = file_stat(&st);
    let json = String::from_utf8(m.to_json_vec()?).expect("manifest json is utf-8");
    conn.execute(
        "INSERT OR REPLACE INTO records
         (id, kind, state, dir, created_utc, stream_label, node_id, session_id,
          n_samples, fs_hz, manifest_json, scan_mtime_ns, scan_size)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        rusqlite::params![
            ulid,
            KIND_RECORD,
            m.state,
            rel,
            m.provenance.created_utc,
            m.provenance.stream_label,
            m.provenance.node_id,
            m.provenance.session_id, // pass B overwrites with the effective id
            m.intrinsic.n_samples as i64,
            m.intrinsic.fs_hz,
            json,
            mtime_ns,
            size,
        ],
    )?;
    Ok(())
}

fn upsert_session(conn: &Connection, dir: &Path, ulid: &str, rel: &str) -> Result<(), CatalogError> {
    let path = dir.join(SESSION_MANIFEST);
    let bytes = fs::read(&path)?;
    let s = SessionManifest::from_slice(&bytes)?;
    let (mtime_ns, size) = file_stat(&fs::metadata(&path)?);
    let json = String::from_utf8(s.to_json_vec()?).expect("manifest json is utf-8");
    conn.execute(
        "INSERT OR REPLACE INTO records
         (id, kind, state, dir, created_utc, stream_label, node_id, session_id,
          n_samples, fs_hz, manifest_json, scan_mtime_ns, scan_size)
         VALUES (?,?,?,?,?,NULL,NULL,NULL,NULL,NULL,?,?,?)",
        rusqlite::params![ulid, KIND_SESSION, "complete", rel, s.started_utc, json, mtime_ns, size],
    )?;
    Ok(())
}

/// Salvage recording residue into a `state=salvaged` final manifest:
/// declare exactly the files that self-validated, with freshly computed
/// sizes and hashes. Refuses (keeps everything, alerts) when the result
/// cannot pass `validate_complete` — destruction requires proof, and so
/// does adoption.
fn salvage(
    dir: &Path,
    ulid: &str,
    rel: &str,
    out: &ScanOutcome,
    partial: bool,
    report: &mut ReindexReport,
) -> Result<Option<RecordManifest>, CatalogError> {
    let Some(rec) = &out.recording else { return Ok(None) };
    let mut m = rec.clone();
    m.state = STATE_SALVAGED.to_string();
    m.id = ulid.to_string(); // the directory IS the identity
    let declared: Vec<Artifact> = std::mem::take(&mut m.artifacts);
    let mut evidence_mismatches = 0usize;

    for (name, status) in out.data_files.iter().zip(out.facts.artifacts.iter()) {
        if *status != ArtifactStatus::Verified {
            continue;
        }
        let bytes = fs::read(dir.join(name))?;
        let old = declared.iter().find(|a| a.path == *name);
        // Self-validation can only condemn provable damage; when the
        // recording manifest DID declare a size or hash, that evidence is
        // stronger and must be honoured — a truncated unknown-format file
        // must not be laundered into a hash-vouched salvaged final.
        if let Some(o) = old {
            let size_bad = o.size_bytes != 0 && o.size_bytes != bytes.len() as u64;
            let hash_bad = o
                .blake3
                .as_deref()
                .is_some_and(|h| h != blake3::hash(&bytes).to_hex().as_str());
            if size_bad || hash_bad {
                evidence_mismatches += 1;
                continue; // left on disk, not adopted
            }
        }
        // The dying sink may never have fsynced this file; the manifest
        // about to vouch for its hash must not outlive its content.
        fs::File::open(dir.join(name)).and_then(|f| f.sync_all())?;
        m.artifacts.push(Artifact {
            role: old.map(|a| a.role.clone()).unwrap_or_else(|| {
                Path::new(name)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "data".into())
            }),
            path: name.clone(),
            format: old.map(|a| a.format.clone()).unwrap_or_else(|| {
                match Path::new(name).extension().and_then(|e| e.to_str()) {
                    Some("npy") => "npy".into(),
                    Some("json") => "json".into(),
                    _ => "bin".into(),
                }
            }),
            size_bytes: bytes.len() as u64,
            blake3: Some(blake3::hash(&bytes).to_hex().to_string()),
            extra: serde_json::Map::new(),
        });
    }

    if let Err(e) = m.validate_complete() {
        report.problems.push(Problem {
            target: rel.into(),
            category: PROBLEM_SALVAGE_REFUSED.into(),
            detail: format!("recording residue kept; salvage would not validate: {e}"),
        });
        return Ok(None);
    }

    write_atomic(&dir.join(RECORD_MANIFEST), &m.to_json_vec()?)?;
    let _ = fs::remove_file(dir.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")));
    if partial || evidence_mismatches > 0 {
        report.problems.push(Problem {
            target: rel.into(),
            category: PROBLEM_SALVAGE_PARTIAL.into(),
            detail: format!(
                "some files were not adopted (left on disk): failed self-validation, plus {evidence_mismatches} contradicting the recording manifest's declared size/hash"
            ),
        });
    }
    Ok(Some(m))
}

/// Pass B: rebuild the materialised four-layer attribute view for every
/// indexed entity. Always full — see module docs.
fn materialize_attrs(
    conn: &Connection,
    root: &DataRoot,
    report: &mut ReindexReport,
) -> Result<(), CatalogError> {
    let rows: Vec<(String, String, String)> = conn.prepare("SELECT id, kind, manifest_json FROM records")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;

    let mut sessions: HashMap<String, SessionManifest> = HashMap::new();
    for (id, kind, json) in &rows {
        if kind == KIND_SESSION {
            if let Ok(s) = SessionManifest::from_slice(json.as_bytes()) {
                sessions.insert(id.clone(), s);
            }
        }
    }
    let mut session_anns: HashMap<String, Option<AnnotationFile>> = HashMap::new();

    conn.execute("DELETE FROM record_attrs", [])?;
    {
        let mut ins = conn.prepare(
            "INSERT OR REPLACE INTO record_attrs
             (record_id, key, value, value_num, source_layer) VALUES (?,?,?,?,?)",
        )?;
        let mut set_sid = conn.prepare("UPDATE records SET session_id=? WHERE id=?")?;

        for (id, kind, json) in &rows {
            let attrs: BTreeMap<String, ResolvedAttr> = if kind == KIND_SESSION {
                let Some(s) = sessions.get(id) else { continue };
                let ann = load_annotation(&root.session_annotation(id)?, report);
                resolve_session_attrs(s, ann.as_ref())
            } else {
                let Ok(m) = RecordManifest::from_slice(json.as_bytes()) else { continue };
                let record_ann = load_annotation(&root.record_annotation(id)?, report);
                if let Some(a) =
                    record_ann.as_ref().and_then(|a| a.keys.get("session_id"))
                {
                    if !matches!(a.value, Value::String(_) | Value::Null) {
                        report.problems.push(Problem {
                            target: id.clone(),
                            category: PROBLEM_SESSION_ID_INVALID.into(),
                            detail: format!(
                                "annotation session_id is {} (expected string or null) — falling back to the capture-time snapshot",
                                a.value
                            ),
                        });
                    }
                }
                let sid = effective_session_id(&m, record_ann.as_ref());
                let session = sid.as_ref().and_then(|s| sessions.get(s));
                if let Some(s) = &sid {
                    if session.is_none() {
                        report.problems.push(Problem {
                            target: id.clone(),
                            category: PROBLEM_SESSION_MISSING.into(),
                            detail: format!(
                                "effective session '{s}' is not in the index — session layers unresolved"
                            ),
                        });
                    }
                }
                let session_ann = match (&sid, session) {
                    (Some(s), Some(_)) => session_anns
                        .entry(s.clone())
                        .or_insert_with(|| {
                            root.session_annotation(s)
                                .ok()
                                .and_then(|p| read_annotation_file(&p).ok().flatten())
                        })
                        .clone(),
                    _ => None,
                };
                set_sid.execute(rusqlite::params![sid, id])?;
                resolve_record_attrs(&m, record_ann.as_ref(), session, session_ann.as_ref())
            };

            for (k, a) in &attrs {
                ins.execute(rusqlite::params![
                    id,
                    k,
                    a.value.to_string(),
                    a.value.as_f64(),
                    a.layer.as_str(),
                ])?;
            }
            report.attrs_records += 1;
        }
    }
    Ok(())
}

/// Annotation files whose entity is not in the index: alert (a typo'd
/// ULID or a record that never arrived from the capture machine).
fn detect_dangling_annotations(
    conn: &Connection,
    root: &DataRoot,
    report: &mut ReindexReport,
) -> Result<(), CatalogError> {
    for (sub, kind) in [("records", KIND_RECORD), ("sessions", KIND_SESSION)] {
        let dir = root.path().join(crate::layout::ANNOTATIONS_DIR).join(sub);
        let entries = match fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".json") else { continue };
            let known: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM records WHERE id=? AND kind=?)",
                rusqlite::params![stem, kind],
                |r| r.get(0),
            )?;
            if !known {
                report.problems.push(Problem {
                    target: format!("annotations/{sub}/{name}"),
                    category: PROBLEM_DANGLING_ANNOTATION.into(),
                    detail: "annotation without a matching indexed entity".into(),
                });
            }
        }
    }
    Ok(())
}

/// Prune rows whose directory vanished (the index is a cache; absence on
/// disk IS the truth). Goes through a temp table — a `NOT IN (?,?,…)` would
/// hit SQLite's bound-parameter ceiling and fail every reindex once the
/// catalog outgrows it.
fn prune_missing(conn: &Connection, seen: &[String]) -> Result<u64, CatalogError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS seen_ids(id TEXT PRIMARY KEY);
         DELETE FROM seen_ids;",
    )?;
    {
        let mut ins = conn.prepare("INSERT OR IGNORE INTO seen_ids(id) VALUES (?)")?;
        for id in seen {
            ins.execute([id])?;
        }
    }
    let pruned =
        conn.execute("DELETE FROM records WHERE id NOT IN (SELECT id FROM seen_ids)", [])?;
    conn.execute("DELETE FROM seen_ids", [])?;
    Ok(pruned as u64)
}

/// SQLITE_NOTADB: the file exists but is not a database (torn write,
/// garbage copy). The one error class where deleting the cache is the fix.
fn is_not_a_database(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::NotADatabase
    )
}

fn persist_problems(conn: &Connection, problems: &[Problem]) -> Result<(), CatalogError> {
    conn.execute("DELETE FROM problems", [])?;
    let now = utc_now_rfc3339();
    let mut ins = conn.prepare("INSERT INTO problems(target, category, detail, seen_utc) VALUES (?,?,?,?)")?;
    for p in problems {
        ins.execute(rusqlite::params![p.target, p.category, p.detail, now])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read an annotation file, downgrading damage to a problem (the record
/// must still be indexed — but silently dropping the human layer is not
/// acceptable, hence the alert).
fn load_annotation(path: &Path, report: &mut ReindexReport) -> Option<AnnotationFile> {
    match read_annotation_file(path) {
        Ok(v) => v,
        Err(e) => {
            report.problems.push(Problem {
                target: path.to_string_lossy().into_owned(),
                category: PROBLEM_ANNOTATION_UNREADABLE.into(),
                detail: format!("{e} — treated as absent for this run"),
            });
            None
        }
    }
}

fn file_stat(st: &fs::Metadata) -> (i64, i64) {
    let mtime_ns = st
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    (mtime_ns, st.len() as i64)
}

/// Newest mtime among the directory and its direct entries, compared against
/// now − min_age. Errs on the side of "recent" (refusing to act) when a stat
/// fails.
fn dir_is_recent(dir: &Path, min_age: Duration) -> io::Result<bool> {
    let cutoff = SystemTime::now().checked_sub(min_age).unwrap_or(SystemTime::UNIX_EPOCH);
    let newer = |st: io::Result<fs::Metadata>| -> bool {
        st.and_then(|s| s.modified()).map(|t| t > cutoff).unwrap_or(true)
    };
    if newer(fs::metadata(dir)) {
        return Ok(true);
    }
    for entry in fs::read_dir(dir)? {
        if newer(entry?.metadata()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_matching(dir: &Path, pred: impl Fn(&str) -> bool) -> Result<u64, CatalogError> {
    let mut n = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if pred(&name) {
            fs::remove_file(entry.path())?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::UlidGen;
    use crate::manifest::tests_support::{complete_manifest, npy_bytes};
    use crate::manifest::{AnnotationEntry, FORMAT_VERSION, STATE_RECORDING};
    use crate::writer::{commit_complete_record, ArtifactPayload};
    use serde_json::json;

    /// Fabricate one committed record; returns its ULID.
    fn fab_record(
        root: &DataRoot,
        gen: &mut UlidGen,
        stream: &str,
        dt_ms: Option<f64>,
        session_id: Option<&str>,
    ) -> String {
        let npy = npy_bytes(8, 2);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.id = gen.next_now();
        m.provenance.stream_label = stream.into();
        m.provenance.session_id = session_id.map(str::to_string);
        if let Some(dt) = dt_ms {
            m.intrinsic.annotations = vec![AnnotationEntry::from_blob(
                0xABCD,
                format!(r#"{{"matched":true,"dt_ms":{dt}}}"#).as_bytes(),
            )];
        }
        m.artifacts.clear();
        commit_complete_record(
            root,
            &mut m,
            &[ArtifactPayload {
                role: "waveform",
                path: "waveform.npy",
                format: "npy/f32-le/C/(n,ch)",
                bytes: &npy,
            }],
        )
        .unwrap();
        m.id
    }

    fn fab_session(root: &DataRoot, gen: &mut UlidGen, pen_for_rec0: &str) -> String {
        let id = gen.next_now();
        let s = SessionManifest {
            format_version: FORMAT_VERSION.into(),
            id: id.clone(),
            started_utc: "2026-06-11T00:00:00.000Z".into(),
            graph: None,
            host: "archlinux".into(),
            attrs: json!({"subject": "s01"}).as_object().unwrap().clone(),
            sink_attrs: json!({"rec0": {"pen": pen_for_rec0}}).as_object().unwrap().clone(),
            extra: Default::default(),
        };
        let path = root.session_manifest(&id).unwrap();
        crate::atomic::write_atomic(&path, &s.to_json_vec().unwrap()).unwrap();
        id
    }

    fn write_annotation(path: &Path, pairs: &[(&str, Value)]) {
        let mut a = AnnotationFile::new();
        for (k, v) in pairs {
            a.keys.insert(
                k.to_string(),
                crate::annot::AnnotValue {
                    value: v.clone(),
                    ts: "2026-06-11T02:00:00.000Z".into(),
                    writer: "test".into(),
                    extra: Default::default(),
                },
            );
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, a.to_json_vec().unwrap()).unwrap();
    }

    /// Reindex with the age guard disabled (test dirs are always fresh).
    fn reindex_now(ix: &mut Index) -> ReindexReport {
        ix.reindex(&ReindexOptions { min_age: Some(Duration::ZERO), ..Default::default() })
            .unwrap()
    }

    #[test]
    fn empty_root_reindexes_to_nothing() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert_eq!(rep.scanned_dirs, 0);
        assert!(ix.query(&[], &QueryOpts::default()).unwrap().is_empty());
    }

    #[test]
    fn index_query_filters_end_to_end() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab_record(&root, &mut gen, "pen0", Some(3.5), None);
        let b = fab_record(&root, &mut gen, "pen0", Some(25.0), None);
        let c = fab_record(&root, &mut gen, "finger", None, None);

        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert_eq!(rep.indexed_records, 3);
        assert!(rep.problems.is_empty(), "{:?}", rep.problems);

        let q = |fs: &[&str]| -> Vec<String> {
            let filters: Vec<Filter> = fs.iter().map(|f| Filter::parse(f).unwrap()).collect();
            ix.query(&filters, &QueryOpts::default())
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect()
        };
        assert_eq!(q(&["stream_label=pen0"]), vec![a.clone(), b.clone()]);
        assert_eq!(q(&["dt_ms<20"]), vec![a.clone()]);
        assert_eq!(q(&["dt_ms>=20"]), vec![b.clone()]);
        assert_eq!(q(&["matched"]), vec![a.clone(), b.clone()]);
        // Ne includes records WITHOUT the key (complement of Eq).
        assert_eq!(q(&["stream_label!=pen0"]), vec![c.clone()]);
        assert_eq!(q(&["matched=true", "dt_ms<20"]), vec![a.clone()]);
        // chronological order, limit = most recent N
        let rows = ix
            .query(&[], &QueryOpts { limit: Some(2), ..Default::default() })
            .unwrap();
        assert_eq!(rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(), vec![b, c]);
        // attrs round-trip with provenance
        let row = ix.record(&a).unwrap().unwrap();
        assert_eq!(row.attrs["dt_ms"].value, 3.5);
        assert_eq!(row.attrs["dt_ms"].layer, "record_intrinsic");
        assert_eq!(row.attrs["id"].value, a);
    }

    #[test]
    fn incremental_skips_unchanged_full_rescans() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        for _ in 0..3 {
            fab_record(&root, &mut gen, "pen0", None, None);
        }
        let mut ix = Index::open(&root).unwrap();
        let r1 = reindex_now(&mut ix);
        assert_eq!((r1.indexed_records, r1.skipped_unchanged), (3, 0));
        let r2 = reindex_now(&mut ix);
        assert_eq!((r2.indexed_records, r2.skipped_unchanged), (0, 3));
        let r3 = ix
            .reindex(&ReindexOptions {
                full: true,
                min_age: Some(Duration::ZERO),
                ..Default::default()
            })
            .unwrap();
        assert_eq!((r3.indexed_records, r3.skipped_unchanged), (3, 0));
    }

    #[test]
    fn annotation_edit_updates_attrs_despite_skip_and_tombstone_hides() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab_record(&root, &mut gen, "pen0", None, None);
        let mut ix = Index::open(&root).unwrap();
        reindex_now(&mut ix);

        // Annotate AFTER the first reindex: the record dir is unchanged
        // (fast path), but the attrs must still pick the annotation up.
        write_annotation(
            &root.record_annotation(&a).unwrap(),
            &[("grade", "good".into()), ("deleted", Value::Bool(true))],
        );
        let rep = reindex_now(&mut ix);
        assert_eq!(rep.skipped_unchanged, 1);

        let visible = ix.query(&[], &QueryOpts::default()).unwrap();
        assert!(visible.is_empty(), "tombstoned record must hide by default");
        let all = ix
            .query(&[], &QueryOpts { include_deleted: true, ..Default::default() })
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].attrs["grade"].value, "good");
        assert_eq!(all[0].attrs["grade"].layer, "record_annotation");
    }

    #[test]
    fn corrupt_record_reported_kept_deindexed() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab_record(&root, &mut gen, "pen0", None, None);
        let mut ix = Index::open(&root).unwrap();
        reindex_now(&mut ix);

        // Flip a byte in the artifact: Corrupt → kept, alerted, row removed.
        let npy = root.record_dir(&a).unwrap().join("waveform.npy");
        let mut bytes = fs::read(&npy).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&npy, &bytes).unwrap();

        let rep = reindex_now(&mut ix);
        assert!(rep.problems.iter().any(|p| p.category == PROBLEM_CORRUPT));
        assert!(npy.exists(), "corrupt artifacts are never deleted");
        assert!(ix.record(&a).unwrap().is_none(), "index must stop vouching");
        assert!(ix.problems().unwrap().iter().any(|p| p.category == PROBLEM_CORRUPT));
    }

    #[test]
    fn failed_empty_deleted_only_past_age_guard() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let id = gen.next_now();
        let dir = root.record_dir(&id).unwrap();
        fs::create_dir_all(&dir).unwrap();
        let mut m = complete_manifest("waveform.npy", &npy_bytes(2, 1));
        m.id = id.clone();
        m.state = STATE_RECORDING.into();
        m.artifacts.clear();
        fs::write(
            dir.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")),
            m.to_json_vec().unwrap(),
        )
        .unwrap();

        let mut ix = Index::open(&root).unwrap();
        // Fresh directory + default age guard → kept.
        let rep = ix.reindex(&ReindexOptions::default()).unwrap();
        assert_eq!(rep.deleted_dirs, 0);
        assert!(dir.exists());
        // Age guard lifted → provably-empty residue is deleted.
        let rep = reindex_now(&mut ix);
        assert_eq!(rep.deleted_dirs, 1);
        assert!(!dir.exists());
    }

    #[test]
    fn salvage_full_and_partial() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();

        // Full: a valid npy + parsable recording manifest.
        let id1 = gen.next_now();
        let dir1 = root.record_dir(&id1).unwrap();
        fs::create_dir_all(&dir1).unwrap();
        let npy = npy_bytes(8, 2);
        fs::write(dir1.join("waveform.npy"), &npy).unwrap();
        let mut m = complete_manifest("waveform.npy", &npy);
        m.id = id1.clone();
        m.state = STATE_RECORDING.into();
        m.artifacts.clear(); // optional-until-complete
        fs::write(
            dir1.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")),
            m.to_json_vec().unwrap(),
        )
        .unwrap();

        // Partial: same, plus a provably broken json sidecar.
        let id2 = gen.next_now();
        let dir2 = root.record_dir(&id2).unwrap();
        fs::create_dir_all(&dir2).unwrap();
        fs::write(dir2.join("waveform.npy"), &npy).unwrap();
        fs::write(dir2.join("meta.json"), b"{ broken").unwrap();
        let mut m2 = complete_manifest("waveform.npy", &npy);
        m2.id = id2.clone();
        m2.state = STATE_RECORDING.into();
        m2.artifacts.clear();
        fs::write(
            dir2.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")),
            m2.to_json_vec().unwrap(),
        )
        .unwrap();

        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert_eq!(rep.salvaged, 2);
        assert!(rep.problems.iter().any(|p| p.category == PROBLEM_SALVAGE_PARTIAL));

        for (id, dir) in [(&id1, &dir1), (&id2, &dir2)] {
            assert!(dir.join(RECORD_MANIFEST).exists());
            assert!(!dir.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")).exists());
            let row = ix.record(id).unwrap().unwrap();
            assert_eq!(row.state, STATE_SALVAGED);
        }
        // The salvaged manifest re-verifies as Complete on a full pass.
        let rep = ix
            .reindex(&ReindexOptions {
                full: true,
                min_age: Some(Duration::ZERO),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rep.indexed_records, 2);
        // The broken sidecar was left on disk, undeclared.
        assert!(dir2.join("meta.json").exists());
    }

    #[test]
    fn session_layers_resolve_onto_records() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let sid = fab_session(&root, &mut gen, "A");
        let rid = fab_record(&root, &mut gen, "pen0", None, Some(&sid));
        write_annotation(
            &root.session_annotation(&sid).unwrap(),
            &[("weather", "rainy".into())],
        );

        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert_eq!((rep.indexed_records, rep.indexed_sessions), (1, 1));

        let row = ix.record(&rid).unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(row.attrs["subject"].value, "s01");
        assert_eq!(row.attrs["subject"].layer, "session_intrinsic");
        assert_eq!(row.attrs["pen"].value, "A", "sink_attrs for rec0 apply");
        assert_eq!(row.attrs["weather"].value, "rainy");
        assert_eq!(row.attrs["weather"].layer, "session_annotation");

        // Sessions are queryable as their own kind.
        let sess = ix
            .query(&[], &QueryOpts { sessions: true, ..Default::default() })
            .unwrap();
        assert_eq!(sess.len(), 1);
        assert_eq!(sess[0].attrs["subject"].value, "s01");

        // Records never leak into the session view and vice versa.
        assert_eq!(ix.query(&[], &QueryOpts::default()).unwrap().len(), 1);
    }

    #[test]
    fn dangling_and_missing_session_alerts() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let ghost = gen.next_now();
        let rid = fab_record(&root, &mut gen, "pen0", None, Some(&ghost));
        write_annotation(&root.record_annotation(&ghost).unwrap(), &[("x", 1.into())]);

        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert!(rep
            .problems
            .iter()
            .any(|p| p.category == PROBLEM_DANGLING_ANNOTATION));
        assert!(rep.problems.iter().any(|p| p.category == PROBLEM_SESSION_MISSING));
        // The record still indexes (layers 2/4 simply unresolved).
        assert!(ix.record(&rid).unwrap().is_some());
    }

    #[test]
    fn prune_rows_when_dirs_vanish_and_dry_run_touches_nothing() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab_record(&root, &mut gen, "pen0", None, None);
        let mut ix = Index::open(&root).unwrap();
        reindex_now(&mut ix);
        assert!(ix.record(&a).unwrap().is_some());

        fs::remove_dir_all(root.record_dir(&a).unwrap()).unwrap();
        let rep = reindex_now(&mut ix);
        assert_eq!(rep.pruned_rows, 1);
        assert!(ix.record(&a).unwrap().is_none());

        // Dry run: a fabricated FailedEmpty dir is reported, not deleted,
        // and the index gains no rows. (A parsable v1 recording manifest —
        // one without a format_version is version-refused and kept.)
        let id = gen.next_now();
        let dir = root.record_dir(&id).unwrap();
        fs::create_dir_all(&dir).unwrap();
        let mut m = complete_manifest("waveform.npy", &npy_bytes(2, 1));
        m.id = id.clone();
        m.state = STATE_RECORDING.into();
        m.artifacts.clear();
        fs::write(
            dir.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")),
            m.to_json_vec().unwrap(),
        )
        .unwrap();
        let rep = ix
            .reindex(&ReindexOptions {
                dry_run: true,
                min_age: Some(Duration::ZERO),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rep.deleted_dirs, 1, "would-delete is reported");
        assert!(dir.exists(), "dry run must not touch the disk");
    }

    #[test]
    fn subdir_data_survives_reindex() {
        // The critical case from review: data hiding in a subdirectory must
        // never be swept by the EmptyDir/FailedEmpty deletion — what was
        // never scanned is never "provably nothing of value".
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let id = gen.next_now();
        let dir = root.record_dir(&id).unwrap();
        let sub = dir.join("backup");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("waveform.npy"), npy_bytes(4, 2)).unwrap();

        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert_eq!(rep.deleted_dirs, 0);
        assert!(sub.join("waveform.npy").exists(), "nested data must survive");
        assert!(rep.problems.iter().any(|p| p.category == PROBLEM_ORPHAN));
    }

    #[test]
    fn salvage_refused_deindexes_stale_row() {
        // A record indexed as complete whose directory later degrades into
        // unsalvageable recording residue: the index must stop vouching
        // (same rule as Corrupt), not list a phantom complete row forever.
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab_record(&root, &mut gen, "pen0", None, None);
        let mut ix = Index::open(&root).unwrap();
        reindex_now(&mut ix);

        let dir = root.record_dir(&a).unwrap();
        let mut m = complete_manifest("waveform.npy", &fs::read(dir.join("waveform.npy")).unwrap());
        m.id = a.clone();
        m.state = STATE_RECORDING.into();
        m.intrinsic.channels.clear(); // salvage will fail validate_complete
        m.artifacts.clear();
        fs::remove_file(dir.join(RECORD_MANIFEST)).unwrap();
        fs::write(
            dir.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")),
            m.to_json_vec().unwrap(),
        )
        .unwrap();

        let rep = reindex_now(&mut ix);
        assert!(rep.problems.iter().any(|p| p.category == PROBLEM_SALVAGE_REFUSED));
        assert!(ix.record(&a).unwrap().is_none(), "index must stop vouching");
        assert!(
            dir.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")).exists(),
            "refused residue stays on disk"
        );
    }

    #[test]
    fn tombstone_only_honoured_from_record_annotation_layer() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        // A capture-time blob carrying deleted=true (forgery vector) …
        let npy = npy_bytes(8, 2);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.id = gen.next_now();
        m.intrinsic.annotations =
            vec![AnnotationEntry::from_blob(1, br#"{"deleted":true}"#)];
        m.artifacts.clear();
        commit_complete_record(
            &root,
            &mut m,
            &[ArtifactPayload {
                role: "waveform",
                path: "waveform.npy",
                format: "npy/f32-le/C/(n,ch)",
                bytes: &npy,
            }],
        )
        .unwrap();
        // … and a session whose attrs carry deleted=true, with a record in it.
        let sid = {
            let id = gen.next_now();
            let s = SessionManifest {
                format_version: FORMAT_VERSION.into(),
                id: id.clone(),
                started_utc: "2026-06-11T00:00:00.000Z".into(),
                graph: None,
                host: "t".into(),
                attrs: json!({"deleted": true}).as_object().unwrap().clone(),
                sink_attrs: Default::default(),
                extra: Default::default(),
            };
            crate::atomic::write_atomic(
                &root.session_manifest(&id).unwrap(),
                &s.to_json_vec().unwrap(),
            )
            .unwrap();
            id
        };
        let in_session = fab_record(&root, &mut gen, "pen0", None, Some(&sid));

        let mut ix = Index::open(&root).unwrap();
        reindex_now(&mut ix);
        let visible = ix.query(&[], &QueryOpts::default()).unwrap();
        let ids: Vec<_> = visible.iter().map(|r| r.id.clone()).collect();
        assert!(ids.contains(&m.id), "blob-layer deleted must NOT hide: {ids:?}");
        assert!(ids.contains(&in_session), "session-layer deleted must NOT hide");
    }

    #[test]
    fn state_mismatch_in_final_manifest_is_alerted() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab_record(&root, &mut gen, "pen0", None, None);
        // Vandalise the state field while keeping the manifest valid JSON
        // and the artifact list intact (sizes/hashes still verify).
        let p = root.record_manifest(&a).unwrap();
        let mut v: Value =
            serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        v["state"] = "recording".into();
        fs::write(&p, serde_json::to_vec_pretty(&v).unwrap()).unwrap();

        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert!(
            rep.problems.iter().any(|p| p.category == PROBLEM_STATE_MISMATCH),
            "{:?}",
            rep.problems
        );
        // Still indexed — the filename wins.
        assert!(ix.record(&a).unwrap().is_some());
    }

    #[test]
    fn invalid_session_id_annotation_falls_back_and_alerts() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let sid = fab_session(&root, &mut gen, "A");
        let a = fab_record(&root, &mut gen, "pen0", None, Some(&sid));
        write_annotation(
            &root.record_annotation(&a).unwrap(),
            &[("session_id", 42.into())], // typo: not a string
        );
        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert!(rep
            .problems
            .iter()
            .any(|p| p.category == PROBLEM_SESSION_ID_INVALID));
        let row = ix.record(&a).unwrap().unwrap();
        assert_eq!(
            row.session_id.as_deref(),
            Some(sid.as_str()),
            "typo must not detach the record from its session"
        );
    }

    #[test]
    fn garbage_index_file_self_heals() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab_record(&root, &mut gen, "pen0", None, None);
        fs::create_dir_all(root.index_dir()).unwrap();
        fs::write(root.index_dir().join("catalog.sqlite"), b"this is not sqlite").unwrap();

        let mut ix = Index::open(&root).unwrap();
        assert!(ix.schema_was_reset, "caller must be told to reindex");
        reindex_now(&mut ix);
        assert!(ix.record(&a).unwrap().is_some());
    }

    #[test]
    fn recent_orphan_window_defers_judgment() {
        // The sink's artifact→manifest commit window scans as Orphan; on a
        // fresh directory that must neither alert nor (de)index — judgment
        // waits for the next run.
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let id = gen.next_now();
        let dir = root.record_dir(&id).unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("waveform.npy"), npy_bytes(4, 2)).unwrap(); // no manifest yet

        let mut ix = Index::open(&root).unwrap();
        // Default age guard: fresh dir → defer (no orphan alert).
        let rep = ix.reindex(&ReindexOptions::default()).unwrap();
        assert!(
            !rep.problems.iter().any(|p| p.category == PROBLEM_ORPHAN),
            "{:?}",
            rep.problems
        );
        // Age guard lifted → the orphan is reported.
        let rep = reindex_now(&mut ix);
        assert!(rep.problems.iter().any(|p| p.category == PROBLEM_ORPHAN));
    }

    #[test]
    fn salvage_honours_declared_hash_evidence() {
        // A truncated unknown-format file passes self-validation (cannot
        // prove damage), but the recording manifest declared its hash —
        // that evidence must keep it out of the salvaged final.
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let id = gen.next_now();
        let dir = root.record_dir(&id).unwrap();
        fs::create_dir_all(&dir).unwrap();

        let npy = npy_bytes(8, 2);
        fs::write(dir.join("waveform.npy"), &npy).unwrap();
        let full = vec![7u8; 1000];
        fs::write(dir.join("video.bin"), &full[..500]).unwrap(); // truncated!

        let mut m = complete_manifest("waveform.npy", &npy);
        m.id = id.clone();
        m.state = STATE_RECORDING.into();
        m.artifacts = vec![Artifact {
            role: "video".into(),
            path: "video.bin".into(),
            format: "bin".into(),
            size_bytes: 1000,
            blake3: Some(blake3::hash(&full).to_hex().to_string()),
            extra: Default::default(),
        }];
        fs::write(
            dir.join(format!("{RECORD_MANIFEST}{RECORDING_SUFFIX}")),
            m.to_json_vec().unwrap(),
        )
        .unwrap();

        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert_eq!(rep.salvaged, 1);
        assert!(rep.problems.iter().any(|p| p.category == PROBLEM_SALVAGE_PARTIAL));
        let row = ix.record(&id).unwrap().unwrap();
        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(root.record_manifest(&id).unwrap()).unwrap(),
        )
        .unwrap();
        let adopted: Vec<&str> = manifest["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["path"].as_str().unwrap())
            .collect();
        assert_eq!(adopted, vec!["waveform.npy"], "{row:?}");
        assert!(dir.join("video.bin").exists(), "rejected file stays on disk");
    }

    #[test]
    fn non_ulid_dir_alerted_untouched() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let stray = root.records_dir().join("01").join("not-a-ulid");
        fs::create_dir_all(&stray).unwrap();
        fs::write(stray.join("x.bin"), b"x").unwrap();
        let mut ix = Index::open(&root).unwrap();
        let rep = reindex_now(&mut ix);
        assert!(rep.problems.iter().any(|p| p.category == PROBLEM_NOT_A_ULID));
        assert!(stray.exists());
    }

    #[test]
    fn filter_parse_accepts_the_grammar() {
        let f = Filter::parse("dt_ms<20").unwrap();
        assert_eq!((f.op, f.value_num), (FilterOp::Lt, Some(20.0)));
        let f = Filter::parse("stream_label=pen0").unwrap();
        assert_eq!(f.op, FilterOp::Eq);
        assert_eq!(f.value_json, "\"pen0\"");
        assert_eq!(f.value_num, None);
        let f = Filter::parse("matched=true").unwrap();
        assert_eq!(f.value_json, "true");
        let f = Filter::parse("pen!=A").unwrap();
        assert_eq!(f.op, FilterOp::Ne);
        let f = Filter::parse("grade").unwrap();
        assert_eq!(f.op, FilterOp::Exists);
        assert!(Filter::parse("dt_ms<abc").is_err(), "ordering needs numbers");
        assert!(Filter::parse("=x").is_err());
        assert!(Filter::parse("").is_err());
        // Numbers equal across int/float representations via value_num.
        let f = Filter::parse("fs_hz=4000").unwrap();
        assert_eq!(f.value_num, Some(4000.0));
    }

    #[test]
    fn eq_number_matches_float_storage() {
        let t = tempfile::tempdir().unwrap();
        let root = DataRoot::new(t.path());
        let mut gen = UlidGen::new();
        let a = fab_record(&root, &mut gen, "pen0", Some(20.0), None);
        let mut ix = Index::open(&root).unwrap();
        reindex_now(&mut ix);
        let rows = ix
            .query(&[Filter::parse("dt_ms=20").unwrap()], &QueryOpts::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, a);
        // fs_hz stored as 4000.0 must match integer-spelled filter.
        let rows = ix
            .query(&[Filter::parse("fs_hz=4000").unwrap()], &QueryOpts::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
    }
}
