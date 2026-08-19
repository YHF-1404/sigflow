//! Structured configuration documents: the shape declaration.
//!
//! A [`ParameterDescriptor`] describes **one knob**. That is enough for a
//! plugin's own flat parameter list, and it is where `visible_when`, `range`
//! and `unit` already live. What it cannot express is the shape real
//! machine configuration has:
//!
//! - **repetition** — four axes, five operating-condition groups per axis;
//! - **libraries** — a motor *type* (`dm3510`) whose values are shared by
//!   every axis that fits one, edited in one place;
//! - **references** — an axis *slot* pointing at a library entry, optionally
//!   overriding a field or two locally;
//! - **inheritance** — one library entry extending another (`hold` extends
//!   `base`, overriding three fields).
//!
//! This module adds exactly that layer and nothing else: fields are still
//! [`ParameterDescriptor`]s. A [`DocSchema`] groups them ([`ParamGroup`]),
//! declares which groups form reusable libraries ([`LibraryDecl`]), and
//! declares the document's tree of collections and slots ([`NodeDecl`]).
//!
//! The schema is authored as its own TOML file inside the plugin package and
//! referenced from the manifest (see `DocumentDecl` in
//! [`crate::manifest`]) — a real machine schema runs to hundreds of lines
//! and nesting that into `manifest.toml` as arrays-of-tables is unreadable.
//!
//! # The document that goes with it
//!
//! The instance data is a plain TOML/JSON tree whose shape this schema
//! describes; [`crate::doc`] resolves and validates it. Keeping the document
//! untyped is deliberate — a field the current binary does not know about
//! must survive a load/edit/save round trip untouched (the same reason
//! `NodeConfig::archived_params` exists), and a typed struct with catch-all
//! maps buys nothing over the tree it would wrap.
//!
//! # What is deliberately NOT here
//!
//! *Flattening.* Inheritance and overrides are resolved only at the last
//! step before a value leaves for a device. The document and the UI keep the
//! unflattened form, because flattening destroys exactly the information an
//! operator needs: whether `600` is this axis's own decision or something it
//! inherited from `base`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::manifest::ParameterDescriptor;

// ---------------------------------------------------------------------------
// Schema root
// ---------------------------------------------------------------------------

/// The shape of one configuration document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct DocSchema {
    /// Globally unique, namespaced like a plugin name
    /// (e.g. `dexhand.motor_params`).
    pub id: String,
    /// Bumped when a change is not backward compatible with existing
    /// documents. Additive field changes do not bump it — an older document
    /// simply leaves the new field unset.
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub note: Option<String>,
    /// Field groups, referenced by id from [`LibraryDecl::group`] and
    /// [`GroupDecl::group`]. A group is the reusable unit of "a form's worth
    /// of fields".
    #[serde(default)]
    pub groups: Vec<ParamGroup>,
    /// Libraries of reusable named entries (motor types, gearbox types,
    /// operating-condition groups).
    #[serde(default)]
    pub libraries: Vec<LibraryDecl>,
    /// The document tree below the root.
    #[serde(default)]
    pub root: Vec<NodeDecl>,
    /// Cross-field constraints that no single field's `range` can express.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl DocSchema {
    pub fn group(&self, id: &str) -> Option<&ParamGroup> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub fn library(&self, id: &str) -> Option<&LibraryDecl> {
        self.libraries.iter().find(|l| l.id == id)
    }

    /// The [`ParamGroup`] backing a library's entries.
    pub fn library_group(&self, library_id: &str) -> Option<&ParamGroup> {
        self.group(&self.library(library_id)?.group)
    }
}

// ---------------------------------------------------------------------------
// Field groups
// ---------------------------------------------------------------------------

/// A named set of fields — one form's worth.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ParamGroup {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub note: Option<String>,
    /// Optional headings the form groups fields under, in display order.
    /// Fields name one via [`ParameterDescriptor::section`]; fields naming
    /// none (or an unknown one) render before the first section.
    #[serde(default)]
    pub sections: Vec<SectionDecl>,
    /// The fields themselves — the same descriptor a plugin uses for its own
    /// parameters, so `range` / `unit` / `visible_when` / `editable` all mean
    /// what they already mean.
    #[serde(default)]
    pub fields: Vec<ParameterDescriptor>,
}

impl ParamGroup {
    pub fn field(&self, id: &str) -> Option<&ParameterDescriptor> {
        self.fields.iter().find(|f| f.id == id)
    }
}

/// A heading inside a [`ParamGroup`]'s form.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct SectionDecl {
    pub id: String,
    pub label: String,
    /// Shown under the heading. The place for the "why these values" prose
    /// that otherwise only exists as a comment in someone's TOML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// Libraries
// ---------------------------------------------------------------------------

/// A library of reusable named entries, all shaped by one [`ParamGroup`].
///
/// A library entry is *shared*: editing `dm3510` changes every axis that
/// references it. That is the point (one motor type, one set of values) and
/// also the single biggest footgun in this kind of editor, so the UI shows a
/// reference count and offers save-as-new-entry; see
/// [`SlotDecl::allow_override`] for the per-instance escape hatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct LibraryDecl {
    /// e.g. `motor`. Entries live under this key in the document.
    pub id: String,
    /// [`ParamGroup::id`] shaping each entry.
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    /// Whether an entry may extend another entry of the same library
    /// (`extends = "base"`). Inheritance is resolved on read, never stored
    /// flattened — see the module docs.
    #[serde(default)]
    pub inheritable: bool,
}

// ---------------------------------------------------------------------------
// Document tree
// ---------------------------------------------------------------------------

/// One node in the document tree.
///
/// The three variants are also the three behaviours the tree UI must offer,
/// and keeping them distinct is what stops "I dragged something in the UI and
/// changed the machine definition":
///
/// | variant      | user may                          |
/// |--------------|-----------------------------------|
/// | [`GroupDecl`]      | edit fields                 |
/// | [`SlotDecl`]       | change the reference (and override, if allowed) |
/// | [`CollectionDecl`] | add / remove / reorder items |
///
/// An axis *has* a motor because the schema says so — that slot cannot be
/// added or deleted. How many axes exist is a collection, and is the user's
/// to decide.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum NodeDecl {
    Group(GroupDecl),
    Slot(SlotDecl),
    Collection(CollectionDecl),
}

impl NodeDecl {
    pub fn id(&self) -> &str {
        match self {
            NodeDecl::Group(g) => &g.id,
            NodeDecl::Slot(s) => &s.id,
            NodeDecl::Collection(c) => &c.id,
        }
    }

    pub fn label(&self) -> Option<&str> {
        match self {
            NodeDecl::Group(g) => g.label.as_deref(),
            NodeDecl::Slot(s) => s.label.as_deref(),
            NodeDecl::Collection(c) => c.label.as_deref(),
        }
    }
}

/// Fields owned by this node directly (an axis's travel limits, the bus
/// section) — no library, no sharing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct GroupDecl {
    pub id: String,
    /// [`ParamGroup::id`].
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
}

/// A reference to one entry of a library.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct SlotDecl {
    pub id: String,
    /// [`LibraryDecl::id`] this slot picks from.
    pub library: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    /// Whether the instance may override individual fields of the referenced
    /// entry locally ("this axis only"). `false` = the slot can only swap
    /// which entry it points at, which is the right answer whenever a
    /// deviation from the type would be a lie about the hardware.
    #[serde(default)]
    pub allow_override: bool,
    /// Nameplate slot: records *which* entry is installed but produces no
    /// downloadable values and renders read-only. For components that are
    /// documented rather than configured (a drive board whose parameters are
    /// fixed in its own firmware).
    #[serde(default)]
    pub nameplate: bool,
}

/// A repeatable node: the axes, the operating-condition groups of an axis.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct CollectionDecl {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    /// The shape of each item.
    pub item: CollectionItem,
    /// Field of the item used as its display name in the tree. For
    /// [`CollectionItem::Instance`] it names a field of one of the item's
    /// groups (`identity/name`); for [`CollectionItem::Ref`] the entry id is
    /// the name and this is ignored. Absent = ordinal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub max: Option<u32>,
}

/// What one item of a [`CollectionDecl`] is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum CollectionItem {
    /// Each item is a library reference — an axis's list of operating
    /// conditions, each pointing at a `profile` entry.
    Ref {
        library: String,
        #[serde(default)]
        allow_override: bool,
    },
    /// Each item is an instance with its own sub-tree — the axes, each with
    /// slots for motor/gearbox/encoder plus its own groups.
    Instance { children: Vec<NodeDecl> },
}

// ---------------------------------------------------------------------------
// Cross-field rules
// ---------------------------------------------------------------------------

/// A constraint spanning more than one field.
///
/// Deliberately a closed set of kinds rather than an expression language: the
/// constraints this domain actually has are few in shape (an upper bound
/// borrowed from a neighbouring component, an index that must be dense), and
/// an expression language grows into a small programming language with its
/// own parser, its own error messages and its own bugs.
///
/// Paths are `/`-separated and relative to [`Rule::scope`]; a `*` segment
/// fans out over every item of a collection. Both sides may fan out, in
/// which case each left value is compared against every right value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct Rule {
    pub id: String,
    /// Collection path this rule is evaluated once per item of
    /// (`"axis"`), or absent for once over the whole document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub scope: Option<String>,
    #[serde(flatten)]
    pub kind: RuleKind,
    #[serde(default = "default_severity")]
    pub severity: Severity,
    /// Shown to the operator when the rule fails. Write it as the sentence
    /// you would say out loud; `{left}` / `{right}` / `{value}` interpolate.
    pub message: String,
}

fn default_severity() -> Severity {
    Severity::Error
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "rule")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum RuleKind {
    /// `left <= right`, e.g. a profile's speed cap against the gearbox's
    /// maximum input speed.
    Le { left: String, right: String },
    /// `left >= right`.
    Ge { left: String, right: String },
    /// `min <= field <= max`, all three being paths (constant bounds belong
    /// in the field's own `range`).
    Between {
        field: String,
        min: String,
        max: String,
    },
    /// No two items of `collection` share a value of `field`.
    Unique { collection: String, field: String },
    /// `field` over `collection` is exactly `0..n` — the requirement behind
    /// any index that a device selects by ordinal at runtime.
    DenseIndex { collection: String, field: String },
    /// The entry referenced by `slot` must be one of `allowed`.
    Subset { slot: String, allowed: Vec<String> },
    /// The entry referenced by `slot` must also be referenced by some item of
    /// `collection` — an axis's default operating condition has to be one of
    /// the groups actually downloaded to it, or the device selects an ordinal
    /// that is not there.
    MemberOf { slot: String, collection: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum Severity {
    /// Blocks download.
    Error,
    /// Shown, does not block. Recommended-range violations land here.
    Warn,
}

// ---------------------------------------------------------------------------
// Transaction lifecycle
// ---------------------------------------------------------------------------

/// Where a document stands between "someone typed a number" and "the device
/// is running on it".
///
/// This is a contract type rather than a UI detail because the states are not
/// the editor's invention — they are what a device with a shadow copy and an
/// explicit commit actually has. Rendering a plain save button over that
/// hides the one failure this class of system is most expensive to debug:
/// values that were written, acknowledged, and never took effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum DocState {
    /// Edited in the editor, not yet written to the document file.
    Draft,
    /// Written and valid; the device has not been told.
    Saved,
    /// Sent to the device's shadow copy, not yet committed. Values are
    /// readable back but nothing is running on them.
    Staged,
    /// Committed — this is what the device is running on.
    Applied,
    /// Committed and persisted on the device, so it survives a power cycle.
    Stored,
    /// Device and document disagree (checksum readback mismatch, or the
    /// device reports it is running something else).
    Diverged,
}

/// What the consuming plugin last did with the document, as written to
/// [`crate::manifest::DocumentDecl::status_file`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct DocStatusFile {
    /// Fingerprint of the document this status describes. Compared against
    /// the document as it is now: if they differ, the file has moved on since
    /// the device was last written and the editor must say so rather than
    /// show a green light for a version nobody is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub doc_fingerprint: Option<String>,
    /// When the plugin wrote this, ISO 8601.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub at: Option<String>,
    pub sync: DocSyncStatus,
    /// Values for fields the schema marks `readback`, keyed by document path
    /// then field id — `{"axis/0/status": {"config_state": "…"}}`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub readback: HashMap<String, HashMap<String, serde_json::Value>>,
}

/// A device's own account of what it is running, surfaced next to
/// [`DocState`] so the editor never has to infer it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct DocSyncStatus {
    pub state: DocState,
    /// Checksum of the document as the editor computed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub expected_checksum: Option<String>,
    /// Checksum read back from the device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub device_checksum: Option<String>,
    /// Plain-language account of where the device's values came from
    /// ("compiled-in defaults" / "loaded from flash" / "downloaded this
    /// session"). Free-form because only the consuming plugin knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub device_origin: Option<String>,
    /// Per-target results, when a document feeds more than one device. Keyed
    /// by the collection item's key (`s0`, `s1`) so a partial failure names
    /// which axis failed rather than collapsing to one red light.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub targets: HashMap<String, TargetStatus>,
}

/// One device's download result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct TargetStatus {
    pub state: DocState,
    /// Failure detail in the device's own terms — an abort code and what it
    /// means, not "write failed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub error: Option<String>,
}
