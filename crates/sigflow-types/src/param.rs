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
