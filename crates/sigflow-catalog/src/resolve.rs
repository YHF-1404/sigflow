//! Four-layer attribute resolution (ratified, written down once, used
//! everywhere): for any key,
//!
//! ```text
//! record annotation > session annotation > record intrinsic > session intrinsic
//! ```
//!
//! merged key-wise, with per-key provenance. BIDS' lesson is that nobody gets
//! inheritance right at query time, so reindex materialises this resolved
//! view into `record_attrs(record_id, key, value, source_layer)` — but the
//! resolution itself is this pure function, shared by the indexer and
//! `data show`.
//!
//! Layer contents:
//! - **Record intrinsic** (layer 3): scalar fields flattened out of the
//!   record manifest — provenance (`stream_label`, `node_id`, `host`,
//!   `capture_seq`, `session_id`, …), intrinsic (`t0_ns`, `fs_hz`,
//!   `n_samples`, …, plus channel-count summaries), and the top-level scalar
//!   fields of every capture-time annotation blob (the touch-merge JSON is
//!   where `dt_ms` / `matched` live — range-filtering those is the design's
//!   motivating query).
//! - **Session intrinsic** (layer 4): the session manifest's global `attrs`,
//!   overlaid by `sink_attrs[<node_id>]` for the record's producing sink
//!   (per-sink expansion is *internal structure* of layer 4 — the outside
//!   view stays four layers).
//! - **Session / record annotations** (layers 2 / 1): the `keys` of the
//!   mutable annotation files.
//!
//! Protected keys: a record's identity and original timestamps may never be
//! overridden by annotation layers (`id`, `t0_ns`, `trigger_t0_ns`,
//! `created_utc`). `session_id` is deliberately NOT protected — effective
//! attribution is annotatable by design (the intrinsic value is the pointer
//! snapshot at capture time: evidence, not truth).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::annot::AnnotationFile;
use crate::manifest::{RecordManifest, SessionManifest, ROLE_TRIGGER};

/// Where a resolved value came from (the four layers, plus the canonical
/// string each is written as in `record_attrs.source_layer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    SessionIntrinsic,
    RecordIntrinsic,
    SessionAnnotation,
    RecordAnnotation,
}

impl Layer {
    pub fn as_str(&self) -> &'static str {
        match self {
            Layer::SessionIntrinsic => "session_intrinsic",
            Layer::RecordIntrinsic => "record_intrinsic",
            Layer::SessionAnnotation => "session_annotation",
            Layer::RecordAnnotation => "record_annotation",
        }
    }
}

/// A resolved attribute: the winning value and which layer produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAttr {
    pub value: Value,
    pub layer: Layer,
}

/// Keys the annotation layers may never override (identity and original
/// timestamps). Their record-intrinsic values always win.
pub const PROTECTED_KEYS: &[&str] = &["id", "t0_ns", "trigger_t0_ns", "created_utc"];

/// The effective session id used to pick layers 2/4: a record-annotation
/// `session_id` (attribution repair) overrides the intrinsic pointer
/// snapshot. An explicit `null` un-attributes (a valid repair); any other
/// non-string value is an invalid override and falls back to the intrinsic
/// snapshot — a typo must not silently detach a record from its session
/// (the indexer alerts on this case).
pub fn effective_session_id(
    manifest: &RecordManifest,
    record_ann: Option<&AnnotationFile>,
) -> Option<String> {
    if let Some(a) = record_ann.and_then(|a| a.keys.get("session_id")) {
        return match &a.value {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            _ => manifest.provenance.session_id.clone(),
        };
    }
    manifest.provenance.session_id.clone()
}

/// Resolve the four-layer attribute view for one record. The caller picks
/// the session via [`effective_session_id`] and supplies its manifest and
/// annotation file (both optional — records outside any session resolve to
/// layers 1+3 only).
pub fn resolve_record_attrs(
    manifest: &RecordManifest,
    record_ann: Option<&AnnotationFile>,
    session: Option<&SessionManifest>,
    session_ann: Option<&AnnotationFile>,
) -> BTreeMap<String, ResolvedAttr> {
    let mut out: BTreeMap<String, ResolvedAttr> = BTreeMap::new();
    let mut set = |map: &mut BTreeMap<String, ResolvedAttr>, k: &str, v: Value, layer: Layer| {
        map.insert(k.to_string(), ResolvedAttr { value: v, layer });
    };

    // ---- layer 4: session intrinsic (lowest precedence) ----
    if let Some(s) = session {
        for (k, v) in &s.attrs {
            set(&mut out, k, v.clone(), Layer::SessionIntrinsic);
        }
        // Per-sink attrs for the record's producing sink override the
        // session-global ones, still within layer 4.
        if let Some(Value::Object(sink)) = s.sink_attrs.get(&manifest.provenance.node_id) {
            for (k, v) in sink {
                set(&mut out, k, v.clone(), Layer::SessionIntrinsic);
            }
        }
    }

    // ---- layer 3: record intrinsic ----
    let p = &manifest.provenance;
    let i = &manifest.intrinsic;
    {
        let l = Layer::RecordIntrinsic;
        if let Some(sid) = &p.session_id {
            set(&mut out, "session_id", Value::String(sid.clone()), l);
        }
        if let Some(g) = &p.graph {
            set(&mut out, "graph", Value::String(g.clone()), l);
        }
        set(&mut out, "node_id", Value::String(p.node_id.clone()), l);
        set(&mut out, "plugin", Value::String(p.plugin.clone()), l);
        set(&mut out, "host", Value::String(p.host.clone()), l);
        set(&mut out, "capture_seq", p.capture_seq.into(), l);
        set(&mut out, "stream_label", Value::String(p.stream_label.clone()), l);
        flatten_scalars(&mut out, &p.extra, l, &mut set);

        set(&mut out, "clock", Value::String(i.clock.clone()), l);
        set(&mut out, "fs_hz", json_f64(i.fs_hz), l);
        set(&mut out, "fs_source", Value::String(i.fs_source.clone()), l);
        set(&mut out, "n_samples", i.n_samples.into(), l);
        set(&mut out, "sample_index", i.sample_index.into(), l);
        set(&mut out, "gap_samples", i.gap_samples.into(), l);
        set(&mut out, "channels", (i.channels.len() as u64).into(), l);
        let trig = i.channels.iter().filter(|c| c.role == ROLE_TRIGGER).count() as u64;
        set(&mut out, "trig_channels", trig.into(), l);
        flatten_scalars(&mut out, &i.extra, l, &mut set);

        // Capture-time annotation blobs: top-level scalars become queryable
        // bare keys (`dt_ms`, `matched`, `x`, `y`, …). Later entries win on
        // collision within the layer.
        for ann in &i.annotations {
            if let Value::Object(o) = &ann.data {
                flatten_scalars(&mut out, o, l, &mut set);
            }
        }
    }

    // ---- layer 2: session annotation ----
    if let Some(a) = session_ann {
        for (k, av) in &a.keys {
            if PROTECTED_KEYS.contains(&k.as_str()) {
                continue;
            }
            set(&mut out, k, av.value.clone(), Layer::SessionAnnotation);
        }
    }

    // ---- layer 1: record annotation (highest precedence) ----
    if let Some(a) = record_ann {
        for (k, av) in &a.keys {
            if PROTECTED_KEYS.contains(&k.as_str()) {
                continue;
            }
            set(&mut out, k, av.value.clone(), Layer::RecordAnnotation);
        }
    }

    // ---- protected keys: canonical sources, force-set last so nothing —
    // not even a same-layer flattened annotation blob field — can shadow
    // identity or original timestamps ----
    set(&mut out, "id", Value::String(manifest.id.clone()), Layer::RecordIntrinsic);
    set(&mut out, "t0_ns", i.t0_ns.into(), Layer::RecordIntrinsic);
    if let Some(t) = p.trigger_t0_ns {
        set(&mut out, "trigger_t0_ns", t.into(), Layer::RecordIntrinsic);
    } else {
        out.remove("trigger_t0_ns"); // a blob/annotation must not invent one
    }
    set(&mut out, "created_utc", Value::String(p.created_utc.clone()), Layer::RecordIntrinsic);

    out
}

/// Resolve a *session's* own attribute view (two layers: annotation over
/// manifest attrs). Used when indexing session rows.
pub fn resolve_session_attrs(
    session: &SessionManifest,
    session_ann: Option<&AnnotationFile>,
) -> BTreeMap<String, ResolvedAttr> {
    let mut out = BTreeMap::new();
    for (k, v) in &session.attrs {
        out.insert(k.clone(), ResolvedAttr { value: v.clone(), layer: Layer::SessionIntrinsic });
    }
    out.insert(
        "id".to_string(),
        ResolvedAttr { value: Value::String(session.id.clone()), layer: Layer::SessionIntrinsic },
    );
    out.insert(
        "started_utc".to_string(),
        ResolvedAttr {
            value: Value::String(session.started_utc.clone()),
            layer: Layer::SessionIntrinsic,
        },
    );
    out.insert(
        "host".to_string(),
        ResolvedAttr { value: Value::String(session.host.clone()), layer: Layer::SessionIntrinsic },
    );
    if let Some(a) = session_ann {
        for (k, av) in &a.keys {
            if k == "id" || k == "started_utc" {
                continue; // session identity/origin is protected likewise
            }
            out.insert(
                k.clone(),
                ResolvedAttr { value: av.value.clone(), layer: Layer::SessionAnnotation },
            );
        }
    }
    out
}

/// Insert only scalar (string/number/bool) values; arrays/objects/null stay
/// in their structured home (the manifest itself) — they are not filterable
/// and flattening them would just bloat the index.
fn flatten_scalars(
    out: &mut BTreeMap<String, ResolvedAttr>,
    map: &Map<String, Value>,
    layer: Layer,
    set: &mut impl FnMut(&mut BTreeMap<String, ResolvedAttr>, &str, Value, Layer),
) {
    for (k, v) in map {
        if matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
            set(out, k, v.clone(), layer);
        }
    }
}

/// f64 → JSON number; non-finite values (never produced by the sink, but a
/// hand-edited manifest could hold them) map to null rather than panicking.
fn json_f64(v: f64) -> Value {
    serde_json::Number::from_f64(v).map(Value::Number).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annot::AnnotValue;
    use crate::manifest::tests_support::complete_manifest;
    use crate::manifest::{AnnotationEntry, FORMAT_VERSION};

    fn ann(pairs: &[(&str, Value)]) -> AnnotationFile {
        let mut a = AnnotationFile::new();
        for (k, v) in pairs {
            a.keys.insert(
                k.to_string(),
                AnnotValue {
                    value: v.clone(),
                    ts: "2026-06-11T02:00:00.000Z".into(),
                    writer: "test".into(),
                    extra: Map::new(),
                },
            );
        }
        a
    }

    fn session() -> SessionManifest {
        SessionManifest {
            format_version: FORMAT_VERSION.into(),
            id: "01JXSESSION0000000000AAAAA".into(),
            started_utc: "2026-06-11T00:00:00.000Z".into(),
            graph: None,
            host: "archlinux".into(),
            attrs: serde_json::json!({"subject": "s01", "pen": "session-default"})
                .as_object()
                .unwrap()
                .clone(),
            sink_attrs: serde_json::json!({"rec0": {"pen": "A"}}).as_object().unwrap().clone(),
            extra: Map::new(),
        }
    }

    #[test]
    fn layer_precedence_and_provenance() {
        let npy = crate::manifest::tests_support::npy_bytes(4, 2);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.provenance.session_id = Some("01JXSESSION0000000000AAAAA".into());
        // node_id=rec0 → sink_attrs pen=A beats session-global pen.
        let s = session();
        let s_ann = ann(&[("subject", "s01-corrected".into()), ("weather", "rainy".into())]);
        let r_ann = ann(&[("grade", "good".into()), ("subject", "s01-final".into())]);

        let attrs = resolve_record_attrs(&m, Some(&r_ann), Some(&s), Some(&s_ann));

        // layer 4 internal: sink-attr beats global within session intrinsic
        assert_eq!(attrs["pen"].value, "A");
        assert_eq!(attrs["pen"].layer, Layer::SessionIntrinsic);
        // layer 2 beats layer 4
        assert_eq!(attrs["weather"].value, "rainy");
        assert_eq!(attrs["weather"].layer, Layer::SessionAnnotation);
        // layer 1 beats layer 2 beats layer 4
        assert_eq!(attrs["subject"].value, "s01-final");
        assert_eq!(attrs["subject"].layer, Layer::RecordAnnotation);
        // layer 3 fields present with provenance
        assert_eq!(attrs["stream_label"].value, "pen0");
        assert_eq!(attrs["stream_label"].layer, Layer::RecordIntrinsic);
        assert_eq!(attrs["grade"].value, "good");
    }

    #[test]
    fn annotation_blob_scalars_are_flattened_and_filterable() {
        let npy = crate::manifest::tests_support::npy_bytes(4, 2);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.intrinsic.annotations.push(AnnotationEntry::from_blob(
            0xABCD,
            br#"{"matched":true,"dt_ms":3.5,"slot":1,"nested":{"x":1}}"#,
        ));
        let attrs = resolve_record_attrs(&m, None, None, None);
        assert_eq!(attrs["dt_ms"].value, 3.5);
        assert_eq!(attrs["matched"].value, true);
        assert_eq!(attrs["slot"].value, 1);
        assert!(!attrs.contains_key("nested"), "non-scalars stay structured");
        assert_eq!(attrs["trig_channels"].value, 0);
        assert_eq!(attrs["channels"].value, 1);
    }

    #[test]
    fn protected_keys_resist_annotation_and_blob_forgery() {
        let npy = crate::manifest::tests_support::npy_bytes(4, 2);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.intrinsic.t0_ns = 777;
        // A malicious/buggy capture-time blob trying to shadow identity:
        m.intrinsic.annotations.push(AnnotationEntry::from_blob(
            1,
            br#"{"id":"FORGED","t0_ns":1,"created_utc":"1970-01-01T00:00:00Z"}"#,
        ));
        let r_ann = ann(&[("id", "ALSO-FORGED".into()), ("t0_ns", 2.into())]);
        let attrs = resolve_record_attrs(&m, Some(&r_ann), None, None);
        assert_eq!(attrs["id"].value, m.id.as_str());
        assert_eq!(attrs["t0_ns"].value, 777);
        assert_eq!(attrs["created_utc"].value, "2026-06-10T08:30:12.345Z");
        assert!(!attrs.contains_key("trigger_t0_ns"), "blob must not invent one");
    }

    #[test]
    fn session_id_is_annotatable_attribution_repair() {
        let npy = crate::manifest::tests_support::npy_bytes(4, 2);
        let mut m = complete_manifest("waveform.npy", &npy);
        m.provenance.session_id = Some("01JXSESSION0000000000AAAAA".into());
        // Repair: re-attribute to another session.
        let r_ann = ann(&[("session_id", "01JXSESSION0000000000BBBBB".into())]);
        assert_eq!(
            effective_session_id(&m, Some(&r_ann)).as_deref(),
            Some("01JXSESSION0000000000BBBBB")
        );
        // Un-attribute: null override → None.
        let r_null = ann(&[("session_id", Value::Null)]);
        assert_eq!(effective_session_id(&m, Some(&r_null)), None);
        // Invalid (non-string) override: fall back to the snapshot — a typo
        // must not silently detach the record.
        let r_bad = ann(&[("session_id", 42.into())]);
        assert_eq!(
            effective_session_id(&m, Some(&r_bad)).as_deref(),
            Some("01JXSESSION0000000000AAAAA")
        );
        // No annotation → intrinsic pointer snapshot.
        assert_eq!(
            effective_session_id(&m, None).as_deref(),
            Some("01JXSESSION0000000000AAAAA")
        );
        // And the resolved view shows the override with its provenance.
        let attrs = resolve_record_attrs(&m, Some(&r_ann), None, None);
        assert_eq!(attrs["session_id"].value, "01JXSESSION0000000000BBBBB");
        assert_eq!(attrs["session_id"].layer, Layer::RecordAnnotation);
    }

    #[test]
    fn records_outside_any_session_resolve_two_layers() {
        let npy = crate::manifest::tests_support::npy_bytes(4, 2);
        let m = complete_manifest("waveform.npy", &npy);
        let attrs = resolve_record_attrs(&m, None, None, None);
        assert!(!attrs.contains_key("session_id"));
        assert_eq!(attrs["host"].value, "test");
    }

    #[test]
    fn session_own_view_merges_annotation_over_attrs() {
        let s = session();
        let s_ann = ann(&[("subject", "fixed".into()), ("id", "FORGED".into())]);
        let attrs = resolve_session_attrs(&s, Some(&s_ann));
        assert_eq!(attrs["subject"].value, "fixed");
        assert_eq!(attrs["subject"].layer, Layer::SessionAnnotation);
        assert_eq!(attrs["id"].value, s.id.as_str(), "session id protected");
        assert_eq!(attrs["pen"].value, "session-default");
    }
}
