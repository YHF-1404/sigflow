//! Configuration documents: resolution and validation.
//!
//! The document is a plain JSON/TOML tree shaped by a [`DocSchema`]. This
//! module is the authority on two questions the editor, the CLI and the
//! plugin that downloads values to hardware must all answer identically:
//!
//! 1. **What is this field's effective value, and where did it come from?**
//!    ([`resolve_slot_field`] / [`resolve_group_field`])
//! 2. **Is this document fit to send to a device?** ([`validate`])
//!
//! # Document layout
//!
//! ```toml
//! schema = "dexhand.motor_params"
//! schema_version = 1
//!
//! # Libraries: <library id>.<entry id>
//! [libraries.motor.dm3510]
//! pole_pairs = 1
//! rated_current_ma = 590
//!
//! [libraries.profile.base]
//! vel_kp_ua_per_kcps = 600
//!
//! [libraries.profile.hold]
//! extends = "base"            # only where LibraryDecl.inheritable
//! max_torque_permille = 400
//!
//! # Tree: nodes.<node id>, matching DocSchema::root
//! [nodes.bus]
//! cycle_us = 1000
//!
//! [[nodes.axis]]              # a collection of instances
//!   [nodes.axis.identity]     # a group node inside the instance
//!   name = "s0"
//!   index = 0
//!   [nodes.axis.motor]        # a slot node
//!   ref = "dm3510"
//!   [nodes.axis.motor.overrides]
//!   rated_current_ma = 540    # this axis only
//!   [[nodes.axis.profiles]]   # a collection of library references
//!   ref = "base"
//! ```
//!
//! Two reserved keys inside a node's table: `ref` and `overrides` on a slot,
//! `extends` on a library entry. Everything else is a field. Keys the schema
//! does not know are left untouched — a document written by a newer binary
//! must survive an older one's edit-and-save.
//!
//! # The third state
//!
//! A field can be *set to a value*, *inherited*, or **unset** — and unset is
//! not zero. A firmware that reads "current-loop Kp = 0" as "derive it from
//! R, L and the PWM period" makes that distinction load-bearing: rendering
//! an unset field as `0` and saving it writes a decision nobody made. Hence
//! [`Provenance::Unset`], and hence [`ResolvedField::value`] is an `Option`.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::manifest::ParameterDescriptor;
use crate::param::{ParamRange, ParamType, ParamValue};
use crate::schema::{
    CollectionItem, DocSchema, NodeDecl, ParamGroup, RuleKind, Severity, SlotDecl,
};

/// Reserved keys inside a document node / library entry.
pub const KEY_REF: &str = "ref";
pub const KEY_OVERRIDES: &str = "overrides";
pub const KEY_EXTENDS: &str = "extends";

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Where an effective value came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "from")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum Provenance {
    /// Written on this instance's slot, overriding the library entry.
    Override,
    /// Written on the referenced library entry itself.
    Entry { entry: String },
    /// Reached by following `extends` up from the referenced entry.
    Inherited { entry: String },
    /// Written directly on a group node (no library involved).
    Local,
    /// Not written anywhere; the schema's declared default applies.
    Default,
    /// Not written anywhere and no default — see the module docs on why this
    /// is distinct from a value of zero.
    Unset,
}

/// One field's effective value plus the story of how it got there.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ResolvedField {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub value: Option<ParamValue>,
    pub provenance: Provenance,
    /// What the value would become if the local override were removed.
    /// Only present when [`Provenance::Override`] applies — it is what the
    /// "revert to inherited" affordance needs to show before it acts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub inherited: Option<ParamValue>,
}

/// Resolve a field of a group node (`nodes.bus`, an axis's `travel`).
pub fn resolve_group_field(
    group: &ParamGroup,
    node: Option<&Value>,
    field_id: &str,
) -> ResolvedField {
    let desc = group.field(field_id);
    let raw = node.and_then(|n| n.get(field_id));
    let (value, provenance) = match (raw, desc) {
        (Some(v), Some(d)) => (coerce(d.param_type, v), Provenance::Local),
        (Some(_), None) => (None, Provenance::Local),
        (None, Some(d)) => match &d.default {
            Some(dv) => (Some(dv.clone()), Provenance::Default),
            None => (None, Provenance::Unset),
        },
        (None, None) => (None, Provenance::Unset),
    };
    ResolvedField {
        id: field_id.to_string(),
        value,
        provenance,
        inherited: None,
    }
}

/// Resolve a field of a slot node: local override, then the referenced entry,
/// then its `extends` chain, then the schema default.
pub fn resolve_slot_field(
    schema: &DocSchema,
    doc: &Value,
    library_id: &str,
    slot: Option<&Value>,
    field_id: &str,
) -> ResolvedField {
    let group = schema.library_group(library_id);
    let desc = group.and_then(|g| g.field(field_id));
    let ptype = desc.map(|d| d.param_type);

    let entry_id = slot.and_then(|s| s.get(KEY_REF)).and_then(Value::as_str);

    // The chain below the override, computed first so `inherited` can show
    // what reverting would yield.
    let below = match entry_id {
        Some(e) => resolve_entry_chain(schema, doc, library_id, e, field_id, ptype),
        None => None,
    }
    .or_else(|| {
        desc.and_then(|d| d.default.clone())
            .map(|v| (v, Provenance::Default))
    });

    let override_raw = slot
        .and_then(|s| s.get(KEY_OVERRIDES))
        .and_then(|o| o.get(field_id));

    if let Some(raw) = override_raw {
        return ResolvedField {
            id: field_id.to_string(),
            value: ptype.and_then(|t| coerce(t, raw)),
            provenance: Provenance::Override,
            inherited: below.map(|(v, _)| v),
        };
    }

    match below {
        Some((v, p)) => ResolvedField {
            id: field_id.to_string(),
            value: Some(v),
            provenance: p,
            inherited: None,
        },
        None => ResolvedField {
            id: field_id.to_string(),
            value: None,
            provenance: Provenance::Unset,
            inherited: None,
        },
    }
}

/// Walk `entry` and its `extends` ancestors for the first written value.
fn resolve_entry_chain(
    schema: &DocSchema,
    doc: &Value,
    library_id: &str,
    entry_id: &str,
    field_id: &str,
    ptype: Option<ParamType>,
) -> Option<(ParamValue, Provenance)> {
    let inheritable = schema.library(library_id).is_some_and(|l| l.inheritable);
    let mut seen = HashSet::new();
    let mut cur = entry_id.to_string();
    let mut depth = 0usize;

    loop {
        if !seen.insert(cur.clone()) {
            return None; // cycle — validate() reports it; resolution just stops
        }
        let entry = entry(doc, library_id, &cur)?;
        if let Some(raw) = entry.get(field_id) {
            let v = ptype.and_then(|t| coerce(t, raw))?;
            let p = if depth == 0 {
                Provenance::Entry { entry: cur }
            } else {
                Provenance::Inherited { entry: cur }
            };
            return Some((v, p));
        }
        if !inheritable {
            return None;
        }
        let parent = entry.get(KEY_EXTENDS).and_then(Value::as_str)?;
        cur = parent.to_string();
        depth += 1;
    }
}

/// All effective fields of a slot, in schema order.
pub fn resolve_slot(
    schema: &DocSchema,
    doc: &Value,
    library_id: &str,
    slot: Option<&Value>,
) -> Vec<ResolvedField> {
    let Some(group) = schema.library_group(library_id) else {
        return Vec::new();
    };
    group
        .fields
        .iter()
        .map(|f| resolve_slot_field(schema, doc, library_id, slot, &f.id))
        .collect()
}

/// All effective fields of a group node, in schema order.
pub fn resolve_group(group: &ParamGroup, node: Option<&Value>) -> Vec<ResolvedField> {
    group
        .fields
        .iter()
        .map(|f| resolve_group_field(group, node, &f.id))
        .collect()
}

// ---------------------------------------------------------------------------
// Document access
// ---------------------------------------------------------------------------

/// `doc.libraries.<library>.<entry>`.
pub fn entry<'a>(doc: &'a Value, library_id: &str, entry_id: &str) -> Option<&'a Value> {
    doc.get("libraries")?.get(library_id)?.get(entry_id)
}

/// Entry ids of one library, sorted for stable display.
pub fn entry_ids(doc: &Value, library_id: &str) -> Vec<String> {
    doc.get("libraries")
        .and_then(|l| l.get(library_id))
        .and_then(Value::as_object)
        .map(|m| {
            let mut v: Vec<String> = m.keys().cloned().collect();
            v.sort();
            v
        })
        .unwrap_or_default()
}

/// `doc.nodes.<id>`.
pub fn node<'a>(doc: &'a Value, id: &str) -> Option<&'a Value> {
    doc.get("nodes")?.get(id)
}

/// Items of a collection node, as a slice.
pub fn items(node: Option<&Value>) -> &[Value] {
    node.and_then(Value::as_array).map_or(&[], |v| v.as_slice())
}

/// How many slots across the document reference `entry_id` of `library_id`.
///
/// The number the top bar shows next to a library entry: editing an entry
/// used by four axes changes four axes, and the operator should learn that
/// before typing, not after a bench test.
pub fn reference_count(schema: &DocSchema, doc: &Value, library_id: &str, entry_id: &str) -> usize {
    let mut n = 0;
    walk_slots(schema, doc, &mut |lib, _path, slot| {
        if lib == library_id && slot.get(KEY_REF).and_then(Value::as_str) == Some(entry_id) {
            n += 1;
        }
    });
    n
}

/// Visit every slot value in the document: `(library id, path, slot value)`.
pub fn walk_slots(schema: &DocSchema, doc: &Value, f: &mut impl FnMut(&str, &str, &Value)) {
    walk_decls(&schema.root, doc.get("nodes"), "", f);
}

fn walk_decls(
    decls: &[NodeDecl],
    nodes: Option<&Value>,
    prefix: &str,
    f: &mut impl FnMut(&str, &str, &Value),
) {
    for d in decls {
        let path = join(prefix, d.id());
        let v = nodes.and_then(|n| n.get(d.id()));
        match d {
            NodeDecl::Group(_) => {}
            NodeDecl::Slot(s) => {
                if let Some(v) = v {
                    f(&s.library, &path, v);
                }
            }
            NodeDecl::Collection(c) => {
                for (i, item) in items(v).iter().enumerate() {
                    let ipath = join(&path, &i.to_string());
                    match &c.item {
                        CollectionItem::Ref { library, .. } => f(library, &ipath, item),
                        CollectionItem::Instance { children } => {
                            walk_decls(children, Some(item), &ipath, f)
                        }
                    }
                }
            }
        }
    }
}

fn join(prefix: &str, seg: &str) -> String {
    if prefix.is_empty() {
        seg.to_string()
    } else {
        format!("{prefix}/{seg}")
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// One problem found in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct Finding {
    /// `/`-separated location, collection items addressed by ordinal
    /// (`axis/0/motor/pole_pairs`). Ordinals rather than names because a
    /// duplicate name is itself a finding, and a finding must never be
    /// unaddressable.
    pub path: String,
    pub severity: Severity,
    pub message: String,
    /// [`crate::schema::Rule::id`] when the finding came from a rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub rule: Option<String>,
}

impl Finding {
    fn error(path: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            path: path.into(),
            severity: Severity::Error,
            message: message.into(),
            rule: None,
        }
    }
    fn warn(path: impl Into<String>, message: impl Into<String>) -> Finding {
        Finding {
            path: path.into(),
            severity: Severity::Warn,
            message: message.into(),
            rule: None,
        }
    }
}

/// Check a document against its schema.
///
/// Returns everything wrong at once, ordered by document position: an
/// operator fixing a config wants the whole list, not the first error.
/// `Error` findings block download; `Warn` findings (a value outside the
/// bench-proven range, an unreferenced library entry) do not.
pub fn validate(schema: &DocSchema, doc: &Value) -> Vec<Finding> {
    let mut out = Vec::new();
    validate_libraries(schema, doc, &mut out);
    validate_decls(schema, doc, &schema.root, doc.get("nodes"), "", &mut out);
    validate_rules(schema, doc, &mut out);
    out
}

fn validate_libraries(schema: &DocSchema, doc: &Value, out: &mut Vec<Finding>) {
    for lib in &schema.libraries {
        let Some(group) = schema.group(&lib.group) else {
            out.push(Finding::error(
                format!("libraries/{}", lib.id),
                format!("schema 缺失字段组 `{}`", lib.group),
            ));
            continue;
        };
        for id in entry_ids(doc, &lib.id) {
            let path = format!("libraries/{}/{}", lib.id, id);
            let Some(e) = entry(doc, &lib.id, &id) else {
                continue;
            };
            if let Some(parent) = e.get(KEY_EXTENDS).and_then(Value::as_str) {
                if !lib.inheritable {
                    out.push(Finding::error(
                        path.clone(),
                        format!("库 `{}` 不允许条目继承，但 `{id}` 写了 extends", lib.id),
                    ));
                } else if entry(doc, &lib.id, parent).is_none() {
                    out.push(Finding::error(
                        path.clone(),
                        format!("继承的条目 `{parent}` 不存在"),
                    ));
                } else if extends_cycle(doc, &lib.id, &id) {
                    out.push(Finding::error(path.clone(), "继承链成环".to_string()));
                }
            }
            check_fields(group, e, &path, out);
        }
    }
}

fn extends_cycle(doc: &Value, library_id: &str, start: &str) -> bool {
    let mut seen = HashSet::new();
    let mut cur = start.to_string();
    loop {
        if !seen.insert(cur.clone()) {
            return true;
        }
        let Some(e) = entry(doc, library_id, &cur) else {
            return false;
        };
        match e.get(KEY_EXTENDS).and_then(Value::as_str) {
            Some(p) => cur = p.to_string(),
            None => return false,
        }
    }
}

fn validate_decls(
    schema: &DocSchema,
    doc: &Value,
    decls: &[NodeDecl],
    nodes: Option<&Value>,
    prefix: &str,
    out: &mut Vec<Finding>,
) {
    for d in decls {
        let path = join(prefix, d.id());
        let v = nodes.and_then(|n| n.get(d.id()));
        match d {
            NodeDecl::Group(g) => match schema.group(&g.group) {
                Some(group) => {
                    if let Some(v) = v {
                        check_fields(group, v, &path, out);
                    }
                }
                None => out.push(Finding::error(
                    path,
                    format!("schema 缺失字段组 `{}`", g.group),
                )),
            },
            NodeDecl::Slot(s) => validate_slot(schema, doc, s, v, &path, out),
            NodeDecl::Collection(c) => {
                let n = items(v).len() as u32;
                if let Some(min) = c.min {
                    if n < min {
                        out.push(Finding::error(
                            path.clone(),
                            format!("至少需要 {min} 项，当前 {n}"),
                        ));
                    }
                }
                if let Some(max) = c.max {
                    if n > max {
                        out.push(Finding::error(
                            path.clone(),
                            format!("最多 {max} 项，当前 {n}"),
                        ));
                    }
                }
                for (i, item) in items(v).iter().enumerate() {
                    let ipath = join(&path, &i.to_string());
                    match &c.item {
                        CollectionItem::Ref {
                            library,
                            allow_override,
                        } => {
                            let decl = SlotDecl {
                                id: c.id.clone(),
                                library: library.clone(),
                                label: None,
                                allow_override: *allow_override,
                                nameplate: false,
                            };
                            validate_slot(schema, doc, &decl, Some(item), &ipath, out);
                        }
                        CollectionItem::Instance { children } => {
                            validate_decls(schema, doc, children, Some(item), &ipath, out)
                        }
                    }
                }
            }
        }
    }
}

fn validate_slot(
    schema: &DocSchema,
    doc: &Value,
    decl: &SlotDecl,
    slot: Option<&Value>,
    path: &str,
    out: &mut Vec<Finding>,
) {
    let Some(group) = schema.library_group(&decl.library) else {
        out.push(Finding::error(
            path,
            format!("schema 缺失库 `{}`", decl.library),
        ));
        return;
    };
    let Some(slot) = slot else { return };

    match slot.get(KEY_REF).and_then(Value::as_str) {
        Some(r) => {
            if entry(doc, &decl.library, r).is_none() {
                out.push(Finding::error(
                    path,
                    format!("`{}` 库里没有条目 `{r}`", decl.library),
                ));
            }
        }
        None => out.push(Finding::error(path, "槽位未选择库条目".to_string())),
    }

    if let Some(ov) = slot.get(KEY_OVERRIDES) {
        let empty = ov.as_object().is_none_or(|m| m.is_empty());
        if !empty && !decl.allow_override {
            out.push(Finding::error(
                format!("{path}/overrides"),
                if decl.nameplate {
                    "铭牌槽位只记录装了哪一条，不接受本地覆盖".to_string()
                } else {
                    "该槽位不允许本地覆盖，改型号库或另存为新条目".to_string()
                },
            ));
        }
        check_fields(group, ov, &format!("{path}/overrides"), out);
    }
}

/// Type / range / recommended-range checks over one table of raw field values.
fn check_fields(group: &ParamGroup, table: &Value, path: &str, out: &mut Vec<Finding>) {
    let Some(map) = table.as_object() else { return };
    for (k, raw) in map {
        if k == KEY_EXTENDS || k == KEY_REF || k == KEY_OVERRIDES {
            continue;
        }
        let Some(desc) = group.field(k) else {
            // Unknown keys are preserved, not rejected: a document written by
            // a newer schema must round-trip through an older binary.
            continue;
        };
        let fpath = format!("{path}/{k}");
        let Some(v) = coerce(desc.param_type, raw) else {
            out.push(Finding::error(
                fpath,
                format!(
                    "`{}` 期望 {:?}，得到 {}",
                    desc.label.as_deref().unwrap_or(k),
                    desc.param_type,
                    raw
                ),
            ));
            continue;
        };
        if let Some(msg) = range_violation(&desc.range, &v) {
            out.push(Finding::error(fpath.clone(), msg));
            continue;
        }
        if let Some(rec) = &desc.recommended {
            if let Some(msg) = range_violation(rec, &v) {
                out.push(Finding::warn(
                    fpath,
                    format!("{msg}（实测推荐区间，超出不阻止下发）"),
                ));
            }
        }
    }
}

fn range_violation(range: &ParamRange, v: &ParamValue) -> Option<String> {
    let n = numeric(v);
    match range {
        ParamRange::F64Range { min, max } => {
            let n = n?;
            (n < *min || n > *max).then(|| format!("超出范围 {min} … {max}（当前 {n}）"))
        }
        ParamRange::U32Range { min, max } => {
            let n = n?;
            (n < *min as f64 || n > *max as f64)
                .then(|| format!("超出范围 {min} … {max}（当前 {n}）"))
        }
        ParamRange::I32Range { min, max } => {
            let n = n?;
            (n < *min as f64 || n > *max as f64)
                .then(|| format!("超出范围 {min} … {max}（当前 {n}）"))
        }
        ParamRange::EnumVariants(variants) => {
            let s = scalar_string(v);
            (!variants.contains(&s))
                .then(|| format!("`{s}` 不是允许的取值（{}）", variants.join(" / ")))
        }
        ParamRange::None => None,
    }
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

fn validate_rules(schema: &DocSchema, doc: &Value, out: &mut Vec<Finding>) {
    for rule in &schema.rules {
        for (scope_path, scope_decls, scope_nodes) in rule_scopes(schema, doc, rule.scope.as_deref())
        {
            let mut push = |path: String, message: String| {
                out.push(Finding {
                    path,
                    severity: rule.severity,
                    message,
                    rule: Some(rule.id.clone()),
                });
            };
            match &rule.kind {
                RuleKind::Le { left, right } | RuleKind::Ge { left, right } => {
                    let ge = matches!(rule.kind, RuleKind::Ge { .. });
                    let ls = resolve_path(schema, doc, scope_decls, scope_nodes, &scope_path, left);
                    let rs =
                        resolve_path(schema, doc, scope_decls, scope_nodes, &scope_path, right);
                    for (lp, lv) in &ls {
                        for (_, rv) in &rs {
                            let (Some(a), Some(b)) = (numeric(lv), numeric(rv)) else {
                                continue;
                            };
                            let bad = if ge { a < b } else { a > b };
                            if bad {
                                push(lp.clone(), interpolate(&rule.message, a, b));
                            }
                        }
                    }
                }
                RuleKind::Between { field, min, max } => {
                    let fs = resolve_path(schema, doc, scope_decls, scope_nodes, &scope_path, field);
                    let lo = resolve_path(schema, doc, scope_decls, scope_nodes, &scope_path, min);
                    let hi = resolve_path(schema, doc, scope_decls, scope_nodes, &scope_path, max);
                    for (fp, fv) in &fs {
                        let Some(v) = numeric(fv) else { continue };
                        let below = lo.iter().filter_map(|(_, x)| numeric(x)).any(|m| v < m);
                        let above = hi.iter().filter_map(|(_, x)| numeric(x)).any(|m| v > m);
                        if below || above {
                            push(fp.clone(), rule.message.clone());
                        }
                    }
                }
                RuleKind::Unique { collection, field } => {
                    let mut seen: HashMap<String, usize> = HashMap::new();
                    for (i, (p, v)) in
                        collection_field(schema, doc, scope_decls, scope_nodes, &scope_path, collection, field)
                            .into_iter()
                            .enumerate()
                    {
                        let s = scalar_string(&v);
                        if let Some(prev) = seen.insert(s.clone(), i) {
                            push(p, format!("{} （与第 {prev} 项重复）", rule.message));
                        }
                    }
                }
                RuleKind::DenseIndex { collection, field } => {
                    let got =
                        collection_field(schema, doc, scope_decls, scope_nodes, &scope_path, collection, field);
                    let mut nums: Vec<(String, i64)> = got
                        .iter()
                        .filter_map(|(p, v)| numeric(v).map(|n| (p.clone(), n as i64)))
                        .collect();
                    nums.sort_by_key(|(_, n)| *n);
                    for (want, (p, n)) in nums.iter().enumerate() {
                        if *n != want as i64 {
                            push(
                                p.clone(),
                                format!("{}（应为 {want}，实为 {n}）", rule.message),
                            );
                        }
                    }
                }
                RuleKind::Subset { slot, allowed } => {
                    for (p, v) in slot_refs(scope_decls, scope_nodes, &scope_path, slot) {
                        if !allowed.contains(&v) {
                            push(p, format!("{}（`{v}` 不在允许集合内）", rule.message));
                        }
                    }
                }
                RuleKind::MemberOf { slot, collection } => {
                    let members: Vec<String> = slot_refs(
                        scope_decls,
                        scope_nodes,
                        &scope_path,
                        &format!("{collection}/*"),
                    )
                    .into_iter()
                    .map(|(_, r)| r)
                    .collect();
                    for (p, v) in slot_refs(scope_decls, scope_nodes, &scope_path, slot) {
                        if !members.contains(&v) {
                            push(
                                p,
                                format!("{}（`{v}` 不在 `{collection}` 里）", rule.message),
                            );
                        }
                    }
                }
            }
        }
    }
}

/// The (path, decls, nodes) triples a rule is evaluated against: once for the
/// whole document, or once per item of the scope collection.
fn rule_scopes<'a>(
    schema: &'a DocSchema,
    doc: &'a Value,
    scope: Option<&str>,
) -> Vec<(String, &'a [NodeDecl], Option<&'a Value>)> {
    let Some(scope) = scope else {
        return vec![(String::new(), schema.root.as_slice(), doc.get("nodes"))];
    };
    let mut cur: Vec<(String, &[NodeDecl], Option<&Value>)> =
        vec![(String::new(), schema.root.as_slice(), doc.get("nodes"))];
    for seg in scope.split('/').filter(|s| !s.is_empty()) {
        let mut next = Vec::new();
        for (path, decls, nodes) in cur {
            let Some(NodeDecl::Collection(c)) = decls.iter().find(|d| d.id() == seg) else {
                continue;
            };
            let CollectionItem::Instance { children } = &c.item else {
                continue;
            };
            let v = nodes.and_then(|n| n.get(seg));
            for (i, item) in items(v).iter().enumerate() {
                next.push((
                    join(&join(&path, seg), &i.to_string()),
                    children.as_slice(),
                    Some(item),
                ));
            }
        }
        cur = next;
    }
    cur
}

/// One position while walking a rule path: absolute path, the declarations
/// still in scope, the node value, and the slot (library id + value) when the
/// walk has landed on one.
type Cursor<'a> = (
    String,
    &'a [NodeDecl],
    Option<&'a Value>,
    Option<(&'a str, &'a Value)>,
);

/// Resolve a rule path to `(absolute path, effective value)` pairs. A `*`
/// segment fans out over a collection.
///
/// A **leading `/` means from the document root**, ignoring the rule's scope.
/// A per-axis rule routinely needs something that is not per-axis — a limit
/// against the bus voltage, say — and without this it simply could not name
/// it, which is a rule that silently never fires.
fn resolve_path(
    schema: &DocSchema,
    doc: &Value,
    decls: &[NodeDecl],
    nodes: Option<&Value>,
    prefix: &str,
    path: &str,
) -> Vec<(String, ParamValue)> {
    let (decls, nodes, prefix) = if path.starts_with('/') {
        (schema.root.as_slice(), doc.get("nodes"), "")
    } else {
        (decls, nodes, prefix)
    };
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some((field, nav)) = segs.split_last() else {
        return Vec::new();
    };
    let mut cursors: Vec<Cursor> = vec![(prefix.to_string(), decls, nodes, None)];

    for seg in nav {
        let mut next = Vec::new();
        for (path, decls, nodes, slot) in cursors {
            if *seg == "*" {
                // The fan-out already happened at the collection id; `*` is
                // the readable way to spell "and each of them".
                next.push((path, decls, nodes, slot));
                continue;
            }
            let Some(d) = decls.iter().find(|d| d.id() == *seg) else {
                continue;
            };
            let v = nodes.and_then(|n| n.get(*seg));
            let p = join(&path, seg);
            match d {
                NodeDecl::Group(_) => next.push((p, &[][..], v, None)),
                NodeDecl::Slot(s) => {
                    if let Some(v) = v {
                        next.push((p, &[][..], None, Some((s.library.as_str(), v))));
                    }
                }
                NodeDecl::Collection(c) => {
                    for (i, item) in items(v).iter().enumerate() {
                        let ip = join(&p, &i.to_string());
                        match &c.item {
                            CollectionItem::Ref { library, .. } => {
                                next.push((ip, &[][..], None, Some((library.as_str(), item))))
                            }
                            CollectionItem::Instance { children } => {
                                next.push((ip, children.as_slice(), Some(item), None))
                            }
                        }
                    }
                }
            }
        }
        cursors = next;
    }

    cursors
        .into_iter()
        .filter_map(|(path, decls, nodes, slot)| {
            let r = match slot {
                Some((lib, v)) => resolve_slot_field(schema, doc, lib, Some(v), field),
                None => {
                    // A bare group node: find its declared group to type the field.
                    let _ = decls;
                    let group = group_of(schema, &path)?;
                    resolve_group_field(group, nodes, field)
                }
            };
            r.value.map(|v| (join(&path, field), v))
        })
        .collect()
}

/// The [`ParamGroup`] backing the group node at `path` (walked from the root
/// so a rule can name `identity/index` without the schema being re-threaded).
fn group_of<'a>(schema: &'a DocSchema, path: &str) -> Option<&'a ParamGroup> {
    let mut decls = schema.root.as_slice();
    let mut found: Option<&ParamGroup> = None;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if seg.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let d = decls.iter().find(|d| d.id() == seg)?;
        match d {
            NodeDecl::Group(g) => {
                found = schema.group(&g.group);
                decls = &[];
            }
            NodeDecl::Collection(c) => {
                decls = match &c.item {
                    CollectionItem::Instance { children } => children.as_slice(),
                    CollectionItem::Ref { .. } => &[],
                };
                found = None;
            }
            NodeDecl::Slot(_) => {
                found = None;
                decls = &[];
            }
        }
    }
    found
}

fn collection_field(
    schema: &DocSchema,
    doc: &Value,
    decls: &[NodeDecl],
    nodes: Option<&Value>,
    prefix: &str,
    collection: &str,
    field: &str,
) -> Vec<(String, ParamValue)> {
    resolve_path(
        schema,
        doc,
        decls,
        nodes,
        prefix,
        &format!("{collection}/*/{field}"),
    )
}

/// One position while walking a slot path: absolute path, declarations still
/// in scope, node value, and the slot value once the walk lands on one.
type SlotCursor<'a> = (String, &'a [NodeDecl], Option<&'a Value>, Option<&'a Value>);

/// `(path, referenced entry id)` for every slot the path selects. A `*`
/// segment is a readable no-op: the fan-out already happened at the
/// collection id it follows.
fn slot_refs(
    decls: &[NodeDecl],
    nodes: Option<&Value>,
    prefix: &str,
    path: &str,
) -> Vec<(String, String)> {
    let mut cursors: Vec<SlotCursor> = vec![(prefix.to_string(), decls, nodes, None)];

    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if seg == "*" {
            continue;
        }
        let mut next = Vec::new();
        for (p, decls, nodes, _) in cursors {
            let Some(d) = decls.iter().find(|d| d.id() == seg) else {
                continue;
            };
            let v = nodes.and_then(|n| n.get(seg));
            let np = join(&p, seg);
            match d {
                NodeDecl::Slot(_) => {
                    if let Some(v) = v {
                        next.push((np, &[][..], None, Some(v)));
                    }
                }
                NodeDecl::Group(_) => next.push((np, &[][..], v, None)),
                NodeDecl::Collection(c) => {
                    for (i, item) in items(v).iter().enumerate() {
                        let ip = join(&np, &i.to_string());
                        match &c.item {
                            CollectionItem::Ref { .. } => {
                                next.push((ip, &[][..], None, Some(item)))
                            }
                            CollectionItem::Instance { children } => {
                                next.push((ip, children.as_slice(), Some(item), None))
                            }
                        }
                    }
                }
            }
        }
        cursors = next;
    }

    cursors
        .into_iter()
        .filter_map(|(p, _, _, slot)| {
            slot.and_then(|s| s.get(KEY_REF))
                .and_then(Value::as_str)
                .map(|r| (p, r.to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Scalars
// ---------------------------------------------------------------------------

/// Interpret a raw document scalar as the declared parameter type.
///
/// TOML has no `f32`/`u32` distinction, so an integer is accepted wherever a
/// float is declared (and vice versa when the float is integral) — a config
/// file that says `pos_ff_gain = 1` must not be a type error.
pub fn coerce(ty: ParamType, raw: &Value) -> Option<ParamValue> {
    match ty {
        ParamType::F64 => raw.as_f64().map(ParamValue::F64),
        ParamType::U32 => match raw.as_f64() {
            Some(n) if n >= 0.0 && n <= u32::MAX as f64 && n.fract() == 0.0 => {
                Some(ParamValue::U32(n as u32))
            }
            _ => None,
        },
        ParamType::I32 => match raw.as_f64() {
            Some(n) if n >= i32::MIN as f64 && n <= i32::MAX as f64 && n.fract() == 0.0 => {
                Some(ParamValue::I32(n as i32))
            }
            _ => None,
        },
        ParamType::Bool => raw.as_bool().map(ParamValue::Bool),
        ParamType::String => raw.as_str().map(|s| ParamValue::String(s.to_string())),
        ParamType::Enum => raw.as_str().map(|s| ParamValue::Enum(s.to_string())),
    }
}

/// The scalar rendered without its type tag — the form `visible_when` and
/// enum ranges compare against.
pub fn scalar_string(v: &ParamValue) -> String {
    match v {
        ParamValue::F64(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        ParamValue::U32(n) => n.to_string(),
        ParamValue::I32(n) => n.to_string(),
        ParamValue::Bool(b) => b.to_string(),
        ParamValue::String(s) | ParamValue::Enum(s) => s.clone(),
    }
}

fn numeric(v: &ParamValue) -> Option<f64> {
    match v {
        ParamValue::F64(n) => Some(*n),
        ParamValue::U32(n) => Some(*n as f64),
        ParamValue::I32(n) => Some(*n as f64),
        ParamValue::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    }
}

fn interpolate(msg: &str, left: f64, right: f64) -> String {
    msg.replace("{left}", &fmt_num(left))
        .replace("{value}", &fmt_num(left))
        .replace("{right}", &fmt_num(right))
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

/// One change to a document.
///
/// Edits are sent rather than a whole rewritten document, for three reasons
/// that all matter here:
///
/// - **Comments survive.** These documents are hand-written and their comments
///   carry the reasoning ("600 stable, 1200 oscillates"). Rewriting the file
///   from a parsed tree throws that away; a surgical edit does not.
/// - **Unknown keys never leave the shell**, so no client bug can drop the
///   field a newer schema added.
/// - **A concurrent edit elsewhere in the file is not clobbered**, which a
///   whole-document PUT would do by construction.
///
/// `path` is the document address the row was rendered from
/// (`axis/0/motor`, `libraries/motor/dm3510`, `axis/0/travel`), and it is
/// what decides where the value lands — see [`EditTarget`]. That is the whole
/// of the "edit the type, or override it here" choice: one is a path into
/// `libraries/`, the other a path into the instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum DocEdit {
    /// Write a field. Raw scalar, coerced against the field's declared type.
    SetField {
        path: String,
        field: String,
        value: Value,
    },
    /// Remove a field, so it falls back to whatever it inherited — the
    /// "revert to inherited" action, and the only way back to [`Provenance::Unset`].
    UnsetField { path: String, field: String },
    /// Point a slot at a different library entry.
    SetRef {
        path: String,
        #[serde(rename = "ref")]
        entry: String,
    },
    /// Create a library entry, optionally seeded from an existing one
    /// (save-as-new-type) or extending it (a new variant of a base).
    AddEntry {
        library: String,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        copy_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        extends: Option<String>,
    },
    /// Delete a library entry. Refused while anything still references it.
    RemoveEntry { library: String, id: String },
    /// Append an item to a collection. `entry` seeds the reference for a
    /// collection of references; an instance collection starts empty and is
    /// filled by the edits that follow it in the same batch.
    AddItem {
        path: String,
        // A separate `serde` attribute: ts-rs silently ignores a rename that
        // shares an attribute with other keys, and the binding then disagrees
        // with the wire.
        #[serde(rename = "ref")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        entry: Option<String>,
    },
    /// Remove one item of a collection, by ordinal.
    RemoveItem { path: String, index: u32 },
}

/// Where a document path's fields are written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "target")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum EditTarget {
    /// The library entry itself — changes every slot pointing at it.
    LibraryEntry { library: String, entry: String },
    /// This instance's override of the entry it references.
    SlotOverride {
        library: String,
        allow_override: bool,
        nameplate: bool,
    },
    /// A group node's own fields.
    Group { group: String },
}

/// What a document path addresses, per the schema.
///
/// Collection items are addressed by ordinal (`axis/0/motor`), so a path
/// stays valid across a rename and unambiguous across a duplicate name.
pub fn resolve_target(schema: &DocSchema, path: &str) -> Option<EditTarget> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.first() == Some(&"libraries") {
        return match segs.as_slice() {
            [_, lib, entry] => Some(EditTarget::LibraryEntry {
                library: (*lib).to_string(),
                entry: (*entry).to_string(),
            }),
            _ => None,
        };
    }

    let mut decls: &[NodeDecl] = &schema.root;
    let mut found: Option<EditTarget> = None;
    for seg in segs {
        if seg.chars().all(|c| c.is_ascii_digit()) {
            continue; // collection ordinal — the decl was set by its collection
        }
        let d = decls.iter().find(|d| d.id() == seg)?;
        match d {
            NodeDecl::Group(g) => {
                found = Some(EditTarget::Group {
                    group: g.group.clone(),
                });
                decls = &[];
            }
            NodeDecl::Slot(s) => {
                found = Some(EditTarget::SlotOverride {
                    library: s.library.clone(),
                    allow_override: s.allow_override,
                    nameplate: s.nameplate,
                });
                decls = &[];
            }
            NodeDecl::Collection(c) => {
                found = match &c.item {
                    CollectionItem::Ref {
                        library,
                        allow_override,
                    } => Some(EditTarget::SlotOverride {
                        library: library.clone(),
                        allow_override: *allow_override,
                        nameplate: false,
                    }),
                    CollectionItem::Instance { .. } => None,
                };
                decls = match &c.item {
                    CollectionItem::Instance { children } => children.as_slice(),
                    CollectionItem::Ref { .. } => &[],
                };
            }
        }
    }
    found
}

/// The declaration a document path names, ordinals skipped.
///
/// `axis` and `axis/0` both resolve to the axis collection — an ordinal
/// selects an item, it does not change what kind of thing is there.
pub fn resolve_decl<'a>(schema: &'a DocSchema, path: &str) -> Option<&'a NodeDecl> {
    let mut decls: &[NodeDecl] = &schema.root;
    let mut found: Option<&NodeDecl> = None;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if seg.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let d = decls.iter().find(|d| d.id() == seg)?;
        found = Some(d);
        decls = match d {
            NodeDecl::Collection(c) => match &c.item {
                CollectionItem::Instance { children } => children.as_slice(),
                CollectionItem::Ref { .. } => &[],
            },
            _ => &[],
        };
    }
    found
}

/// [`resolve_decl`], narrowed to collections.
pub fn resolve_collection<'a>(
    schema: &'a DocSchema,
    path: &str,
) -> Option<&'a crate::schema::CollectionDecl> {
    match resolve_decl(schema, path)? {
        NodeDecl::Collection(c) => Some(c),
        _ => None,
    }
}

/// The [`ParamGroup`] whose fields a path accepts.
pub fn target_group<'a>(schema: &'a DocSchema, target: &EditTarget) -> Option<&'a ParamGroup> {
    match target {
        EditTarget::LibraryEntry { library, .. } | EditTarget::SlotOverride { library, .. } => {
            schema.library_group(library)
        }
        EditTarget::Group { group } => schema.group(group),
    }
}

// ---------------------------------------------------------------------------
// Fingerprint
// ---------------------------------------------------------------------------

/// A stable fingerprint of every effective value in the document.
///
/// Used for "the file and the device disagree" without needing the device's
/// own checksum algorithm — that one is the downloading plugin's business
/// (it is computed over the device's binary layout, see the CiA402 slave's
/// CRC32 object). This one answers a different and equally necessary
/// question: has the document changed since the last successful download.
pub fn fingerprint(schema: &DocSchema, doc: &Value) -> String {
    let mut flat: BTreeMap<String, String> = BTreeMap::new();
    collect(schema, doc, &schema.root, doc.get("nodes"), "", &mut flat);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (k, v) in &flat {
        for b in k.as_bytes().iter().chain(b"=").chain(v.as_bytes()).chain(b";") {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

fn collect(
    schema: &DocSchema,
    doc: &Value,
    decls: &[NodeDecl],
    nodes: Option<&Value>,
    prefix: &str,
    out: &mut BTreeMap<String, String>,
) {
    for d in decls {
        let path = join(prefix, d.id());
        let v = nodes.and_then(|n| n.get(d.id()));
        match d {
            NodeDecl::Group(g) => {
                if let Some(group) = schema.group(&g.group) {
                    for r in resolve_group(group, v) {
                        if let Some(val) = r.value {
                            out.insert(join(&path, &r.id), scalar_string(&val));
                        }
                    }
                }
            }
            NodeDecl::Slot(s) => {
                for r in resolve_slot(schema, doc, &s.library, v) {
                    if let Some(val) = r.value {
                        out.insert(join(&path, &r.id), scalar_string(&val));
                    }
                }
            }
            NodeDecl::Collection(c) => {
                for (i, item) in items(v).iter().enumerate() {
                    let ipath = join(&path, &i.to_string());
                    match &c.item {
                        CollectionItem::Ref { library, .. } => {
                            for r in resolve_slot(schema, doc, library, Some(item)) {
                                if let Some(val) = r.value {
                                    out.insert(join(&ipath, &r.id), scalar_string(&val));
                                }
                            }
                        }
                        CollectionItem::Instance { children } => {
                            collect(schema, doc, children, Some(item), &ipath, out)
                        }
                    }
                }
            }
        }
    }
}

/// Whether a field is currently relevant, per its `visible_when` conditions,
/// against a resolved sibling set. Mirrors the plugin-parameter rule: a
/// condition naming an unknown field is ignored rather than hiding the knob.
pub fn field_visible(desc: &ParameterDescriptor, siblings: &[ResolvedField]) -> bool {
    let Some(conds) = &desc.visible_when else {
        return true;
    };
    conds.iter().all(|c| {
        match siblings
            .iter()
            .find(|s| s.id == c.param)
            .and_then(|s| s.value.as_ref())
        {
            Some(v) => c.equals.contains(&scalar_string(v)),
            None => true,
        }
    })
}
