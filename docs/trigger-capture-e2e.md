# 触发捕获端到端验证（软件闭环）

用纯软件信号（sine_generator）验证整条触发捕获链，无需任何硬件。
目标：软件触发源 `fire` → trigger_capture 按**触发事件的物理时刻**（host_monotonic）
在信号缓冲里定位窗口 → 把窗口分块发出。验证 sine 与 trigger_source 同在
host_monotonic 域、trigger_capture 直接按时间对齐、无需跨时钟域转换。

> 真机版（屏幕震动 + 笔敲击）：把 `sine_generator` 换成 `usb_adc`（信号取
> `adc_out`），**不再需要单独的 `trigger_source`**——笔的物理触发直接接设备
> GPIO PB4/PB5，`usb_adc` 自带 `trigger_pen0`/`trigger_pen1` 两路 marker 输出
> （在采集时钟域锁存、用与 `adc_out` 相同的锚点+恢复速率映射成 host_monotonic
> 时刻）。连线改成：`usb_adc.adc_out → cap.signal`、`usb_adc.trigger_pen0 →
> cap.trigger`（pen1 同理接另一个 cap）。其余步骤不变。
>
> 注意：marker 的样本定位带一个固定的“设备生产↔host 接收”流水线偏移（USB/环缓
> 缓冲，几 ms 量级；无 ISO 丢包时恒定、可标定）。精确对齐需后续的“逐帧带内设备
> 计数”方案。ms 级笔敲窗口足够。

---

## CLI 约定（重要，先看）

- 二进制：`./target/<arch>/release/sigflow-cli`。下文写成 `sigflow`，建议先 alias：
  `alias sigflow ./target/aarch64-apple-darwin/release/sigflow-cli`
- **全局 `-n/--node`**：所有"作用于单个节点"的命令(`status`/`param`/`action`/
  `processing`/`save`/`describe`/`plugin set…`/`widget …`/`parent-port`)都用全局
  `-n <route|id>` 指定目标节点;省略则用当前 attach 的会话节点。**不再需要先 attach**。
- **拓扑命令**(`add`/`del`/`start`/`stop`/`ls`/`attach`):节点作位置参数。
- **关系命令**(`connect`/`control-connect` 等):节点对作位置参数,打到拥有连线的容器(会话/`-n` 节点)。
- 节点寻址:裸 id(如 `cap`,不重名即可)或路由(`/root/cap`)。`attach` 仍可设默认节点(省去每次带 `-n`)。

> 本文档对应的代码改动需重新编译/安装：
> - 重新 `cargo build -p sigflow-cli`（新增了 `status --json`）。
> - 重新 build + install `trigger-capture`（新增了捕获成功日志）：见步骤 1。

---

## 节点图

```
sine_generator.signal_out ──→ trigger_capture.signal
trigger_source.trigger    ──→ trigger_capture.trigger
trigger_capture.capture   ──→ data_monitor.signal_in   (仅作观察落点)
```

| 节点 | 插件 | 角色 / 端口 |
|---|---|---|
| `sig`  | `sigflow.core.sine_generator@0.1.0` | 信号源，producer `signal_out` |
| `trig` | `sigflow.io.trigger_source@0.1.0`   | 软件触发源，producer `trigger`，action `fire` |
| `cap`  | `sigflow.io.trigger_capture@0.1.0`  | 捕获，consumer `signal`/`trigger`，producer `capture` |
| `mon`  | `sigflow.core.data_monitor@0.1.0`   | 观察落点，consumer `signal_in` |

---

## 步骤

### 1. 编译 + 安装四个插件

```fish
cargo build -p sigflow-cli
for p in sine-generator trigger-source trigger-capture data-monitor
    (cd plugins/native/$p; and ./build.sh)
    sigflow plugin install plugins/native/$p/pkg
end
sigflow plugin list        # 确认四个都在
```

### 2. 启动根节点（会自动 attach）

```fish
sigflow /tmp/sigflow-trigger-e2e        # 无子命令 + 路径 = 起一个根节点并 attach
sigflow ls                              # 看到根节点
```

### 3. 在根下加四个子节点

```fish
sigflow add sig
sigflow add trig
sigflow add cap
sigflow add mon
sigflow ls                              # 看到 sig/trig/cap/mon
```

### 4. 给每个节点指派插件（全局 `-n`，无需 attach）

```fish
sigflow -n sig  plugin set sigflow.core.sine_generator@0.1.0 -y
sigflow -n trig plugin set sigflow.io.trigger_source@0.1.0   -y
sigflow -n cap  plugin set sigflow.io.trigger_capture@0.1.0  -y
sigflow -n mon  plugin set sigflow.core.data_monitor@0.1.0   -y
```

### 5. 连线（`connect <from_node> <from_port> <to_node> <to_port>`）

```fish
sigflow connect sig  signal_out  cap  signal
sigflow connect trig trigger     cap  trigger
sigflow connect cap  capture     mon  signal_in
sigflow connection-list                 # 确认三条
```

### 6. 调参（全局 `-n`，无需 attach）

```fish
sigflow -n cap param set window_samples 2048
sigflow -n cap param set trigger_pos    0.3
# 前触发样本若被淘汰导致部分窗，可调大余量：
# sigflow -n cap param set latency_margin_ms 100
```

### 7. 启动处理（每节点一个循环，全局 `-n`）

```fish
for n in sig trig cap mon
    sigflow -n $n processing start
end
```

### 8. 开火并观察

**触发**：

```fish
sigflow -n trig action invoke fire
```

**两种观察方式**：

1. **cap 节点日志（最直接）**——trigger_capture 每次捕获打一行到 stderr，
   壳体把每个节点的 stdout/stderr 重定向到 **`<节点目录>/node.log`**：
   ```fish
   tail -f /tmp/sigflow-trigger-e2e/children/cap/node.log
   ```
   开火后应出现：
   ```
   trigger_capture: capture #1 — 2048 samples/ch from index <N> (t0=<...> ns)
   ```
   部分窗会打 `PARTIAL capture …, K pre-trigger samples missing`。
   （根节点日志在 `<root>/node.log`，子节点在 `<root>/children/<id>/node.log`。
   注意：node.log 重定向是新加的，需重编 `sigflow-cli` + `sigflow-shell` 并**重启图**后才生效。）

2. **mon 的延时 node-state（纯 CLI 可查）**——捕获送达前 mon 没收到任何帧、无延时项；
   送达后其输入口出现延时统计，`count` = 本次捕获被分成的帧数：
   ```fish
   sigflow -n mon status --json    # 开火前：无 "latency" 字段
   #  开火后再查：
   #  "latency": { "signal_in": { "total": { "count": <N>, ... }, "hop": {...} } }
   ```
   `count` 从无→有（且每次 `fire` 再增）即证明捕获窗口已端到端送达。

多按几次 `sigflow -n trig action invoke fire`，每次对应一个捕获段。
也可给 trig 设周期自动开火：`sigflow -n trig param set period_ms 2000`。

---

## 排查

- **日志/延时都不动**：
  - 确认四节点都 `processing start`（`sigflow -n <n> status` 看 `State: running`）。
  - 确认连线与端口名（`sigflow connection-list`；端口名以 manifest 为准）。
  - `window_samples` 太大或 `trigger_pos` 太靠近 1 → 需要更深历史缓冲，先用 2048 / 0.3。
- **`status --json` 不认 `--json`**：CLI 没重编，`cargo build -p sigflow-cli`。
- **cap 日志没有 capture 行**：trigger-capture 没重装，重跑步骤 1 的 build/install（旧版无此日志）。
- **触发被吞**：单 tick 内多次 `fire` 只留最新一个（trigger 口走 last-wins）；手动单击没问题。
- **部分窗（日志 PARTIAL / gap_samples>0）**：前触发样本被环缓淘汰，调大 `latency_margin_ms`。

---

## 涉及的提交

| 提交 | 内容 |
|---|---|
| `19391a3` | 触发捕获插件（时间域窗口捕获） |
| `f909527` | 绝对单调 t0 锚点 |
| `c374727` | 延时机制（每帧 latency + node-state 统计） |
| `b05aa4b` | 固件 GET_DEVICE_TIME 采样计数器（sd-adc，分支 `temp_20260531_devclock`） |
| `f91872c` | host 时钟恢复 PLL → 精确 t0（真机验证 ±4ppm） |
| `b33788b` | 软件触发源 + 壳体空帧跳过 |
| `a442d4f` | sine 实时配速 + 节点 node.log + CLI `status --json` + 捕获日志 + 本文档 |
| （待提交） | CLI 全局 `-n/--node`（统一单节点寻址，去掉 attach 来回切） |
