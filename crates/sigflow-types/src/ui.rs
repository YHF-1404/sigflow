//! UI widget instance schema.
//!
//! A widget is an instance of a `PluginCategory::UiWidget` plugin attached to a
//! node. Multiple instances per node, distinguished by `alias`. Bindings to
//! the node's compute plugin (param/port/state/action) are added separately
//! via `widget bind`; a widget with `bind = None` is an orphan, which is a
//! legal state.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "ts")]
use ts_rs::TS;

// ---------------------------------------------------------------------------
// Widget instance (persisted in node.toml under `[[widgets]]`).
// ---------------------------------------------------------------------------

/// A single widget instance attached to a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct UiWidget {
    /// Unique within the node. Doubles as a human label.
    pub alias: String,
    /// Widget plugin name (e.g. `sigflow.ui.slider`). Single-version globally
    /// so we do not lock to a content hash here.
    pub plugin: String,
    /// Version recorded at install time, informational.
    pub version: String,
    pub layout: Layout,
    /// `None` means the widget is an orphan (legal state — added but not yet
    /// bound to anything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub bind: Option<WidgetBinding>,
    /// Free-form widget-instance config (e.g. `{"orientation": "horizontal"}`).
    /// Validated against the widget plugin's `config_schema` at `widget_add`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub config: HashMap<String, serde_json::Value>,
}

/// How a widget instance connects to a node's compute surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct WidgetBinding {
    pub kind: BindKind,
    /// Descendant of the host node that owns the target, as a `/`-separated
    /// child-id path relative to the host (`"dsp"`, `"dsp/filter"`). `None`
    /// binds the host node itself. Relative — like `ParentPortDecl.bind_node`
    /// — so bindings survive copy/export/import of the host container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub node: Option<String>,
    /// Param id, port id, action id, document id, or empty string for
    /// `State` / `Logs`.
    pub target: String,
    /// Only meaningful when `kind == Port` — tap subscription config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub tap: Option<TapConfig>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum BindKind {
    Param,
    Port,
    State,
    Action,
    /// The node's log stream (`node.log`), read over `get_logs`. Like `State`
    /// it addresses the node as a whole, so `target` is empty.
    Logs,
    /// A configuration document the node's plugin declares
    /// (`PluginManifest::documents`); `target` is the document id. Unlike a
    /// param binding, what the widget receives is a schema plus a tree, not
    /// one scalar.
    Doc,
}

impl BindKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BindKind::Param => "param",
            BindKind::Port => "port",
            BindKind::State => "state",
            BindKind::Action => "action",
            BindKind::Logs => "logs",
            BindKind::Doc => "doc",
        }
    }

    /// Whether this kind addresses the node itself rather than a named item on
    /// its compute surface (and so takes an empty `target`).
    pub fn is_whole_node(self) -> bool {
        matches!(self, BindKind::State | BindKind::Logs)
    }
}

/// Logical position and size on the node UI panel.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct Layout {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 40.0,
        }
    }
}

/// 一台示波器 = 一份 tap。
///
/// 一个口上可以开任意多份 tap，每份各自选列、各自触发、各自 run/stop——节点
/// 把同一条采样轴上的量都放进一个口并声明列契约就够了，不必为了分开看而拆口。
/// 配置持久化在绑定它的控件的 `bind.tap` 里（node.toml），也就是 CLI
/// `widget bind` 的 `--window/--refresh/--mode/--channels/--trig-*` 写的那一份；
/// 绑定里没有 tap 段时前端按控件类型兜底。
///
/// 三种模式（[`TapMode`]；`mode` 缺省由 `window_samples` 推：0 = frame，
/// > 0 = window）：
/// - window：最近 N 拍的环，按 `refresh_hz` 出窗；带 [`TapConfig::trigger`]
///   时窗对齐到触发拍。
/// - frame：生产方每帧原样转发——生产方自己开窗的口（示波器窗、热图、估计）。
/// - stream：每一拍都发，按 `refresh_hz` 打包并带样本轴元数据（首样本序号 /
///   通道数 / 断），客户端自己拼一段可回翻、带断标的历史。
///
/// **几何** = `mode` + `window_samples` + `channels`：三者决定环的布局，改了
/// 必须重建（攒的窗丢掉，见 [`TapConfig::same_geometry`]）；其余项
/// （`refresh_hz` / `stats` / `trigger`）就地生效。run/stop 不在这里——那是这
/// 个观察者的运行态，不是声明。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct TapConfig {
    /// window 模式的窗长（拍数，每通道）。`0` 且 `mode` 未声明 = frame 模式。
    pub window_samples: u32,
    /// 每秒最多出几帧——按墙钟，不按样本数（tap 不知道采样率）。
    pub refresh_hz: f32,
    /// Built-in statistics to compute: `rms`, `peak`, `min`, `max`, `mean`.
    ///
    /// 它是把发出去的样本**所有列交织在一起**算的，多列口上没有意义；示波器
    /// 的测量在浏览器按列算。留着给 CLI / 测试。
    #[serde(default)]
    pub stats: Vec<String>,
    /// Explicit mode override. `None` keeps the `window_samples` heuristic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub mode: Option<TapMode>,
    /// 只抽这些列进环、只发这些列。空 = 整口全部列（老配置的含义不变）。
    ///
    /// 按列 id 存，不按下标——口加一列，下标就悄悄指错（同别名那课）。壳体建
    /// tap 时对着口的列契约解析成通道号，解析不到就拒绝并列出可选列；帧上的
    /// `channels` 数 = 这里选中的列数，顺序 = 这里的顺序（有 `repeat_by` 的口
    /// 不带组序的引用按组序展开）。它住在 tap 里而不只住在图例里，是带宽：
    /// window 模式每次刷新都拷整窗，25 列只看 5 列时线上的字节差 5 倍。
    #[serde(default)]
    pub channels: Vec<ColumnRef>,
    /// 触发。只对 window 模式有意义——frame / stream 带触发校验不过：生产方
    /// 自己开窗的口壳体没法重对齐，装作能触发就是藏问题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub trigger: Option<TapTrigger>,
}

impl TapConfig {
    /// 生效的模式：显式声明优先，否则 `window_samples == 0` 是 frame、> 0 是
    /// window。壳体、CLI、前端都从这一处问，不各推各的。
    pub fn effective_mode(&self) -> TapMode {
        self.mode.unwrap_or(if self.window_samples == 0 {
            TapMode::Frame
        } else {
            TapMode::Window
        })
    }

    /// 几何相同 = 环布局不用重建，改动可以就地生效（攒的窗留着）。
    pub fn same_geometry(&self, other: &TapConfig) -> bool {
        self.effective_mode() == other.effective_mode()
            && self.window_samples == other.window_samples
            && self.channels == other.channels
    }

    /// 声明层面的自洽性——不需要口描述子就能查的那部分。列 id 能不能在口上
    /// 找到、触发源在多组口上有没有说清是哪一组，都要到壳体拿着描述子才知道。
    pub fn validate(&self) -> Result<(), String> {
        if !(self.refresh_hz.is_finite() && self.refresh_hz > 0.0) {
            return Err(format!("refresh_hz must be > 0 (got {})", self.refresh_hz));
        }
        let mode = self.effective_mode();
        if mode == TapMode::Window && self.window_samples == 0 {
            return Err("window mode needs window_samples > 0".to_string());
        }
        for c in &self.channels {
            c.validate().map_err(|e| format!("channels: {e}"))?;
        }
        if let Some(t) = &self.trigger {
            if mode != TapMode::Window {
                return Err(format!(
                    "trigger only applies to window mode (this tap is {})",
                    mode.as_str()
                ));
            }
            t.validate().map_err(|e| format!("trigger: {e}"))?;
        }
        Ok(())
    }
}

/// Tap operating mode (see [`TapConfig::mode`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum TapMode {
    Window,
    Frame,
    Stream,
}

impl TapMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TapMode::Window => "window",
            TapMode::Frame => "frame",
            TapMode::Stream => "stream",
        }
    }
}

impl std::str::FromStr for TapMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "window" => Ok(TapMode::Window),
            "frame" => Ok(TapMode::Frame),
            "stream" => Ok(TapMode::Stream),
            other => Err(format!(
                "unknown tap mode {other:?} (want window|frame|stream)"
            )),
        }
    }
}

/// 指一列。
///
/// - 按列契约的 id：`{ column = "iq" }`；有 `repeat_by` 的口可带组序（0 起，
///   同 `column_groups.label` 的 `{i}`）：`{ column = "iq", group = 2 }`。
///   选列时不带组序 = 每组都要；做触发源时多组口必须带组序（源只能是一个
///   通道，壳体拿描述子校验）。
/// - 按通道下标：`{ channel = 3 }`——给没有列契约的口；有契约的口也认，但那
///   就回到了"口加一列就错位"的老路，声明里别这么写。
///
/// 文本形式（CLI）：`iq`、`2/iq`、`@3`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum ColumnRef {
    Column {
        column: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        group: Option<u32>,
    },
    Channel {
        channel: u32,
    },
}

impl ColumnRef {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            ColumnRef::Column { column, .. } if column.trim().is_empty() => {
                Err("empty column id".to_string())
            }
            _ => Ok(()),
        }
    }
}

impl std::str::FromStr for ColumnRef {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty column ref".to_string());
        }
        if let Some(n) = s.strip_prefix('@') {
            return n
                .parse::<u32>()
                .map(|channel| ColumnRef::Channel { channel })
                .map_err(|_| format!("bad channel index in {s:?} (want @<n>)"));
        }
        if let Some((g, id)) = s.split_once('/') {
            let group = g
                .parse::<u32>()
                .map_err(|_| format!("bad group in {s:?} (want <group>/<column>)"))?;
            if id.is_empty() {
                return Err(format!("missing column id in {s:?}"));
            }
            return Ok(ColumnRef::Column {
                column: id.to_string(),
                group: Some(group),
            });
        }
        Ok(ColumnRef::Column {
            column: s.to_string(),
            group: None,
        })
    }
}

impl std::fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ColumnRef::Column {
                column,
                group: Some(g),
            } => write!(f, "{g}/{column}"),
            ColumnRef::Column {
                column,
                group: None,
            } => f.write_str(column),
            ColumnRef::Channel { channel } => write!(f, "@{channel}"),
        }
    }
}

/// 示波器触发。住在 tap 里——它决定"哪一窗给你"，不是画法。
///
/// 壳体按 scan 逐拍跑（一次 feed 可含多拍）：
/// - 源列在 feed 时读整行，所以**不要求在 `channels` 里**（示波器本就可以
///   外触发）。
/// - 电平与 tap 收到的原始值比。显示侧的 AC 耦合只是把画法平移，不改比较；
///   电平线画在源列的轴上，源列开了 AC 就画在 `level − mean` 处。
/// - NaN 拍透明：不触发、不更新比较状态（NaN = 本拍不适用）。
/// - `pre = round(position × window_samples)`，`post = window_samples − pre`。
///   armed = 连续段已攒够 pre 拍 ∧ 没有等待中的捕获 ∧ 距上次触发拍 ≥
///   `holdoff_samples` ∧ running。触发在拍 t：post 拍到齐那一刻切窗
///   `[t − pre, t + post)`——不等刷新 tick，等了环会把预触发段冲掉。
/// - 断（帧头声明的 / 跳号 / 通道数变）清环、取消等待中的捕获、重新攒 pre。
/// - normal / auto 出帧按 `refresh_hz` 截流，一个周期内多次触发只留最新一窗；
///   single 立刻出帧，然后 tap 自动 stopped（那一帧带 STOPPED 位）。
/// - auto：running、没有等待中的捕获、且连续 2 个窗长没触发 → 按 refresh 发
///   自由跑的窗（帧上不带 TRIGGERED 位）。"2 个窗长"写死。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(TS))]
pub struct TapTrigger {
    pub source: ColumnRef,
    /// 信号原单位。
    pub level: f64,
    #[serde(default)]
    pub slope: TrigSlope,
    /// 迟滞带宽（信号单位，≥ 0）。rising：进 Low 要 `v ≤ level − h/2`，触发要
    /// `v ≥ level + h/2`；falling 对称；either 两边都认。缺省 0——单位不知道
    /// 就选不出一个非零缺省；UI 给"抗噪 = 源列可见 pk-pk 的 5%"一键写入。
    #[serde(default)]
    pub hysteresis: f64,
    /// 释抑：触发后至少再过这么多拍才能再触发。按拍数——与 `window_samples`
    /// 同度量；率随参数变的口（rotor）时间换算不出来。UI 有声明的率时并排显
    /// 示时间。
    #[serde(default)]
    pub holdoff_samples: u32,
    #[serde(default)]
    pub mode: TrigMode,
    /// 触发拍前面占窗的比例，[0, 1]，缺省 0.5（示波器屏中）。
    #[serde(default = "default_trigger_position")]
    pub position: f32,
}

fn default_trigger_position() -> f32 {
    0.5
}

impl TapTrigger {
    pub fn validate(&self) -> Result<(), String> {
        self.source.validate().map_err(|e| format!("source: {e}"))?;
        if !self.level.is_finite() {
            return Err(format!("level must be finite (got {})", self.level));
        }
        if !(self.hysteresis.is_finite() && self.hysteresis >= 0.0) {
            return Err(format!("hysteresis must be >= 0 (got {})", self.hysteresis));
        }
        if !(self.position.is_finite() && (0.0..=1.0).contains(&self.position)) {
            return Err(format!(
                "position must be within [0, 1] (got {})",
                self.position
            ));
        }
        Ok(())
    }
}

/// 触发沿。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum TrigSlope {
    #[default]
    Rising,
    Falling,
    Either,
}

impl TrigSlope {
    pub fn as_str(self) -> &'static str {
        match self {
            TrigSlope::Rising => "rising",
            TrigSlope::Falling => "falling",
            TrigSlope::Either => "either",
        }
    }
}

impl std::str::FromStr for TrigSlope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "rising" => Ok(TrigSlope::Rising),
            "falling" => Ok(TrigSlope::Falling),
            "either" => Ok(TrigSlope::Either),
            other => Err(format!(
                "unknown trigger slope {other:?} (want rising|falling|either)"
            )),
        }
    }
}

/// 触发模式：auto 没触发也按刷新率给自由跑的窗（找电平时屏幕不冻住）；
/// normal 只在触发时出窗，其间显示保持上一窗；single 触发一次就停。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS))]
pub enum TrigMode {
    #[default]
    Auto,
    Normal,
    Single,
}

impl TrigMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TrigMode::Auto => "auto",
            TrigMode::Normal => "normal",
            TrigMode::Single => "single",
        }
    }
}

impl std::str::FromStr for TrigMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "auto" => Ok(TrigMode::Auto),
            "normal" => Ok(TrigMode::Normal),
            "single" => Ok(TrigMode::Single),
            other => Err(format!(
                "unknown trigger mode {other:?} (want auto|normal|single)"
            )),
        }
    }
}

#[cfg(test)]
mod tap_config_tests {
    use super::*;

    fn window(n: u32) -> TapConfig {
        TapConfig {
            window_samples: n,
            refresh_hz: 30.0,
            stats: vec![],
            mode: None,
            channels: vec![],
            trigger: None,
        }
    }

    fn trig(source: &str) -> TapTrigger {
        TapTrigger {
            source: source.parse().unwrap(),
            level: 0.0,
            slope: TrigSlope::Rising,
            hysteresis: 0.0,
            holdoff_samples: 0,
            mode: TrigMode::Auto,
            position: 0.5,
        }
    }

    #[test]
    fn 老配置不带新字段照样加载_新字段取缺省() {
        // 线上的绑定、前端的常量对象都是这个形状；升级不能让它们读不回来。
        let c: TapConfig =
            serde_json::from_str(r#"{"window_samples":1024,"refresh_hz":30,"stats":[]}"#).unwrap();
        assert!(c.channels.is_empty(), "空 = 整口全部列");
        assert!(c.trigger.is_none());
        assert_eq!(c.effective_mode(), TapMode::Window);
        assert!(c.validate().is_ok());

        let f: TapConfig = serde_json::from_str(r#"{"window_samples":0,"refresh_hz":5}"#).unwrap();
        assert_eq!(f.effective_mode(), TapMode::Frame, "--window 0 就是 frame");
    }

    #[test]
    fn 序列化永远带_channels_触发缺省不写() {
        // JSON 得和类型说一样的话：Vec 永远带 []，Option 缺省不出现。
        let s = serde_json::to_string(&window(1024)).unwrap();
        assert!(s.contains(r#""channels":[]"#), "{s}");
        assert!(!s.contains("trigger"), "{s}");
    }

    #[test]
    fn 列引用三种文本形式往返_json形状按声明() {
        let cases: [(&str, ColumnRef, &str); 3] = [
            (
                "iq",
                ColumnRef::Column {
                    column: "iq".into(),
                    group: None,
                },
                r#"{"column":"iq"}"#,
            ),
            (
                "2/iq",
                ColumnRef::Column {
                    column: "iq".into(),
                    group: Some(2),
                },
                r#"{"column":"iq","group":2}"#,
            ),
            ("@3", ColumnRef::Channel { channel: 3 }, r#"{"channel":3}"#),
        ];
        for (text, want, json) in cases {
            let parsed: ColumnRef = text.parse().unwrap();
            assert_eq!(parsed, want, "{text}");
            assert_eq!(parsed.to_string(), text, "Display 要能回到 CLI 写法");
            assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
            let back: ColumnRef = serde_json::from_str(json).unwrap();
            assert_eq!(back, want, "{json}");
        }
        assert!("".parse::<ColumnRef>().is_err());
        assert!("@x".parse::<ColumnRef>().is_err());
        assert!("a/iq".parse::<ColumnRef>().is_err(), "组序要是数字");
        assert!("2/".parse::<ColumnRef>().is_err());
    }

    #[test]
    fn 触发只认_window_模式() {
        let mut c = window(0);
        c.trigger = Some(trig("pwm_cnt"));
        let e = c.validate().unwrap_err();
        assert!(e.contains("window"), "{e}");

        let mut s = window(0);
        s.mode = Some(TapMode::Stream);
        s.trigger = Some(trig("pwm_cnt"));
        assert!(s.validate().is_err(), "stream 带触发也拒");

        let mut w = window(400);
        w.trigger = Some(trig("pwm_cnt"));
        assert!(w.validate().is_ok());
    }

    #[test]
    fn 触发参数越界要拒() {
        let mut w = window(400);
        let mut t = trig("iq");
        t.position = 1.5;
        w.trigger = Some(t.clone());
        assert!(w.validate().unwrap_err().contains("position"));

        t.position = 0.5;
        t.hysteresis = -1.0;
        w.trigger = Some(t.clone());
        assert!(w.validate().unwrap_err().contains("hysteresis"));

        t.hysteresis = 0.0;
        t.level = f64::NAN;
        w.trigger = Some(t.clone());
        assert!(w.validate().unwrap_err().contains("level"));

        t.level = 0.0;
        t.source = ColumnRef::Column {
            column: " ".into(),
            group: None,
        };
        w.trigger = Some(t);
        assert!(w.validate().unwrap_err().contains("source"));
    }

    #[test]
    fn 显式_window_但窗长_0_要拒() {
        // 今天的 Tap::new 会开一个 0 长的环，push 时 `% 0` 就 panic。
        let mut c = window(0);
        c.mode = Some(TapMode::Window);
        assert!(c.validate().is_err());
        let mut z = window(1024);
        z.refresh_hz = 0.0;
        assert!(z.validate().is_err());
    }

    #[test]
    fn 几何是模式加窗长加列_其余就地生效() {
        let a = window(1024);
        let mut b = a.clone();
        b.refresh_hz = 5.0;
        b.stats = vec!["rms".into()];
        b.trigger = Some(trig("iq"));
        assert!(a.same_geometry(&b), "刷新率/统计/触发变了不重建");

        let mut c = a.clone();
        c.channels = vec!["iq".parse().unwrap()];
        assert!(!a.same_geometry(&c), "选列变了环布局就变了");

        let mut d = a.clone();
        d.window_samples = 2048;
        assert!(!a.same_geometry(&d));

        // 显式 window 与推出来的 window 是同一个几何。
        let mut e = a.clone();
        e.mode = Some(TapMode::Window);
        assert!(a.same_geometry(&e));
    }

    #[test]
    fn 触发段的_toml_形状就是文档里写的那个() {
        // 绑定持久化在 node.toml；手写的配置也得能读回来，缺省项可省。
        let t: TapConfig = toml::from_str(
            r#"
window_samples = 400
refresh_hz = 30
mode = "window"
channels = [{ column = "pwm_cnt" }, { column = "va" }, { column = "iq", group = 2 }, { channel = 7 }]

[trigger]
source = { column = "pwm_cnt" }
level = 0
slope = "falling"
holdoff_samples = 350
mode = "normal"
"#,
        )
        .unwrap();
        assert_eq!(t.channels.len(), 4);
        assert_eq!(t.channels[3], ColumnRef::Channel { channel: 7 });
        let tr = t.trigger.as_ref().unwrap();
        assert_eq!(tr.slope, TrigSlope::Falling);
        assert_eq!(tr.mode, TrigMode::Normal);
        assert_eq!(tr.hysteresis, 0.0, "缺省 0");
        assert_eq!(tr.position, 0.5, "缺省屏中");
        assert!(t.validate().is_ok());
    }
}
