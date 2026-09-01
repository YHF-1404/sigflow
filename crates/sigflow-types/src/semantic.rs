use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

/// Dotted-path semantic type describing the signal kind and its properties.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct SemanticType {
    /// Dotted path, e.g. "audio.pcm", "sensor.vibration", "touch.coordinates"
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub sample_rate_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub channels: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub dtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub clock_domain: Option<String>,
}

/// 样本的线上 / 存储表示。`SemanticType.dtype` 只能是这五个名字之一，缺省
/// （没写）= f32——今天所有按浮点读样本的消费方隐含的就是它。
///
/// 整数 dtype 的口（采集卡、仿真的 ADC 码）配合列契约的 `scale`/`offset` 把码
/// 换成物理量：`unit`/`bound` 指物理量，`decode`/`bits` 作用在码上。连线规则：
/// 生产口是整数 dtype 时，消费口必须显式声明同一 dtype；消费口没写 dtype =
/// 只收 f32/f64——让一个按浮点读的消费方收到 i16 就是静默读错。示波器按原生
/// dtype 存（不制造分辨率），tap 换成物理量 f32 再发。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum Dtype {
    I8,
    I16,
    I32,
    F32,
    F64,
}

impl Dtype {
    pub fn as_str(self) -> &'static str {
        match self {
            Dtype::I8 => "i8",
            Dtype::I16 => "i16",
            Dtype::I32 => "i32",
            Dtype::F32 => "f32",
            Dtype::F64 => "f64",
        }
    }

    /// 一个样本几个字节。
    pub fn elem_bytes(self) -> usize {
        match self {
            Dtype::I8 => 1,
            Dtype::I16 => 2,
            Dtype::I32 | Dtype::F32 => 4,
            Dtype::F64 => 8,
        }
    }

    pub fn is_integer(self) -> bool {
        matches!(self, Dtype::I8 | Dtype::I16 | Dtype::I32)
    }
}

impl std::str::FromStr for Dtype {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "i8" => Ok(Dtype::I8),
            "i16" => Ok(Dtype::I16),
            "i32" => Ok(Dtype::I32),
            "f32" => Ok(Dtype::F32),
            "f64" => Ok(Dtype::F64),
            other => Err(format!("unknown dtype {other:?} (want i8|i16|i32|f32|f64)")),
        }
    }
}

impl SemanticType {
    /// 解析后的 dtype：没写 = f32；写了不认识的名字 = 错（不猜）。
    pub fn dtype(&self) -> Result<Dtype, String> {
        match &self.dtype {
            None => Ok(Dtype::F32),
            Some(s) => s.parse(),
        }
    }
}

#[cfg(test)]
mod dtype_tests {
    use super::*;

    #[test]
    fn 缺省_f32_五个名字都认_别的拒() {
        let mut t = SemanticType {
            kind: "timeseries.signal".into(),
            sample_rate_hz: None,
            channels: None,
            dtype: None,
            unit: None,
            clock_domain: None,
        };
        assert_eq!(t.dtype().unwrap(), Dtype::F32);
        for (name, d, bytes) in [
            ("i8", Dtype::I8, 1),
            ("i16", Dtype::I16, 2),
            ("i32", Dtype::I32, 4),
            ("f32", Dtype::F32, 4),
            ("f64", Dtype::F64, 8),
        ] {
            t.dtype = Some(name.into());
            assert_eq!(t.dtype().unwrap(), d);
            assert_eq!(d.elem_bytes(), bytes);
            assert_eq!(d.as_str(), name);
            assert_eq!(serde_json::to_string(&d).unwrap(), format!("\"{name}\""));
        }
        t.dtype = Some("u16".into());
        assert!(t.dtype().is_err(), "不认识的名字不猜");
        assert!(Dtype::I16.is_integer() && !Dtype::F32.is_integer());
    }
}

/// Content-hash identifier for the binary payload layout (sha256-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct PayloadSchemaId(#[cfg_attr(feature = "ts", ts(type = "number"))] pub u64);

/// How samples are batched per transmission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum BatchSpec {
    Fixed(u32),
    Range(u32, u32),
    Negotiable,
}

/// Overflow behavior when a subscriber queue is full.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum BackpressurePolicy {
    DropOldest,
    DropNewest,
}
