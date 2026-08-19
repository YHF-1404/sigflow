use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

/// Runtime parameter value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum ParamValue {
    F64(f64),
    U32(u32),
    I32(i32),
    Bool(bool),
    String(String),
    Enum(String),
}

/// Constraint on a parameter's valid range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum ParamRange {
    F64Range { min: f64, max: f64 },
    U32Range { min: u32, max: u32 },
    I32Range { min: i32, max: i32 },
    EnumVariants(Vec<String>),
    None,
}

/// The data type of a parameter.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum ParamType {
    F64,
    U32,
    I32,
    Bool,
    String,
    Enum,
}

/// Fixed-layout scalar for control connections (iceoryx2 payload).
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct ControlScalar {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub timestamp_ns: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub seq: u64,
    pub value: f64,
}

/// How a value is shown to a human, when the wire unit is not the unit anyone
/// thinks in.
///
/// The conversion is deliberately affine and self-contained
/// (`shown = stored * scale`): a conversion that needs *another* field —
/// counts to rpm needs the encoder's counts-per-rev — is not expressible
/// here, and should not be faked. Those belong in a derived read-out, or in
/// `note` until there is one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct DisplayHint {
    /// Unit shown next to the field, overriding `ParameterDescriptor::unit`
    /// (which stays the wire unit — the one the device actually receives).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub unit: Option<String>,
    /// `shown = stored * scale`. Per-mille rendered as a percentage: `0.1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub scale: Option<f64>,
    /// Decimal places for display; the stored value keeps full precision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub decimals: Option<u32>,
}

/// Friction deliberately placed in front of a field whose wrong value breaks
/// something expensive.
///
/// This is not the same axis as `editable`. An over-current trip level is
/// fully editable and must stay so — it is simply not a knob anyone should
/// turn while sweeping gains, and the machine-safety fields sitting next to
/// the tuning fields is exactly how they get turned by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum ParamGuard {
    /// Editing prompts for confirmation, quoting the field's `note`.
    Confirm,
    /// The field renders locked until explicitly unlocked, per editing
    /// session. For protection limits and mechanical travel.
    Unlock,
}
