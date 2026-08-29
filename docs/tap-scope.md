# tap 示波器：触发、选列、run/stop，以及图上的测量/光标/刻度

2026-08-29 定稿（hml 拍板，七条拍板项 + 多 tap 选列三条都按建议）。类型已落在
sigflow `feat/tap-scope`（`crates/sigflow-types/src/ui.rs`：`TapConfig.channels`、
`TapConfig.trigger`、`ColumnRef`、`TapTrigger`、`TrigSlope`、`TrigMode`，带校验与
测试）；控件声明在 `plugins/ui/waveform-chart/manifest.toml` 头。本文是给
sigflow-core 那边的任务书：壳体、RPC、CLI、前端都在那个仓。

## 0. 一句话

**tap = 一台示波器。** 一个口上开几台都行，每台自己选列、自己的触发、自己的
run/stop；节点只管把同一条采样轴上的量都放进一个口并声明列契约。tap 只做「把哪
些样本给你」——触发、释抑、预触发、run/stop 住壳体；「怎么看」——耦合、缩放、光
标、测量——全在浏览器，且只算显示的那份数据（测量和曲线是同一个证人）。tap 不
改数据（AC 不在 tap 做）。

## 1. 现状里逼出这个设计的四条（对着代码）

1. `TapManager` 以 port_id 为键，一个口一份 `Tap`，`viewers` 计数；后来者的
   refresh/stats 覆盖先来者，`window_samples`/`mode` 变了整个重建。侧栏的 port
   tap 用 `STREAM_TAP_CONFIG`，控件若是 window 模式，侧栏一开控件的 tap 就被
   重建成 stream——**今天就存在的碰撞**。加触发和 run/stop 之后会升级成"别人把
   我的示波器停了 / 去触发了"。
2. 线协议 v2 帧只有 `port_id/seq/ts/sample_index/channels/flags/samples/stats`，
   广播给所有连接、客户端按 port_id 过滤。没地方放"这窗是触发来的、触发点在第
   几拍"，同口多 tap 的帧也分不开。
3. window 模式不看断（stream 已经看了）；`compute_stats` 把所有通道交织在一起
   算——多通道口上那行 `rms · peak` 没有意义。
4. 前端一根共用 Y 轴；`TapHub` 按（路径,口,配置 JSON）共用会话，一条 tap 一条
   WebSocket；绑定的 tap 段持久化在节点配置，前端建控件默认 stream；面控件不能
   放大，只有 `presentation="page"` 的控件有浮窗 + 最大化。

## 2. 类型（已落，sigflow `feat/tap-scope`）

```toml
# node.toml 里一个绑定的 tap 段
[widgets.bind.tap]
window_samples = 400
refresh_hz = 30
mode = "window"                       # window | frame | stream；缺省 0→frame、>0→window
channels = [{ column = "pwm_cnt" }, { column = "va" }, { column = "iq", group = 2 }, { channel = 7 }]
[widgets.bind.tap.trigger]
source = { column = "pwm_cnt" }       # 不必在 channels 里
level = 0
slope = "rising"                      # rising | falling | either
hysteresis = 0                        # 信号单位，≥ 0
holdoff_samples = 0                   # 拍
mode = "auto"                         # auto | normal | single
position = 0.5                        # 触发拍前面占窗的比例
```

- `ColumnRef` 文本形式（CLI）：`iq`、`2/iq`（组序 0 起，同 `column_groups.label`
  的 `{i}`）、`@3`（下标，给无契约的口）。`FromStr`/`Display` 在类型里。
- `TapConfig::effective_mode()`、`same_geometry()`（几何 = mode + window_samples +
  channels）、`validate()`（触发只认 window；position ∈ [0,1]；hysteresis ≥ 0；
  level 有限；显式 window 但窗长 0 拒——今天 `Tap::new` 会开 0 长的环然后 `% 0`
  panic；refresh_hz > 0）。壳体 `create_tap`/`set_tap` 和 CLI 都先过它。
- 序列化：`channels` 永远带 `[]`（TS 里是必填，前端的常量配置对象要补
  `channels: []`）；`trigger` 缺省不出现。
- TS 绑定重新生成：在 sigflow-core 目录
  `SIGFLOW_TS_OUT=$PWD/webui/generated cargo test --manifest-path ../sigflow/Cargo.toml -p sigflow-types --features ts export_bindings`
  （sigflow 工作树要在 `feat/tap-scope` 上；两仓是路径依赖）。

## 3. 壳体（sigflow-core `crates/sigflow-shell/src/tap.rs` + `shell.rs`）

### 3.1 tap 身份

- `TapManager { taps: HashMap<TapId, Tap>, by_port: HashMap<String, Vec<TapId>>, next: u32 }`；
  `create_tap` 返回 `TapId`；`feed(port, data, meta) -> Vec<TapFrame>`（该口上每
  个 tap 各走一遍）；`viewers` 计数删掉。
- `set_tap(id, config)`：`same_geometry` 为真就地换配置（环、触发状态保留；
  `position` 变了只是 pre 变）；否则原地重建、id 不变，应答带 `rebuilt: true`。
- `tap_control(id, Run | Stop | Single)`。
- 源列解析在 shell.rs 的 handler 里做（tap.rs 不认识描述子）：`ColumnRef::Column`
  对着口的 `columns`/`column_groups` 解析成交织下标 `group × cols.len() + col`；
  找不到 → 拒并列出可选列 id；`Channel(n)` 直接用。触发源：口**声明了**
  `column_groups` 而引用不带 group → 拒（"源只能是一个通道"，按声明判，不看运
  行时几组）。
- **组数只有帧知道**（`ColumnGroups`："运行时由帧的通道数除以组内列数得出"），
  所以不带 group 的选列引用解析成 `EveryGroup{col, ncols}`，到 feed 时按帧宽展
  开；`TapManager` 记每个口最近一次见到的通道数（有没有 tap 都记），
  `create_tap`/`set_tap`/`list_taps` 应答里的 `channels` 按它展开，口还没出过数据
  时按一组算——应答要带 `port_channels`（没见过 = null），让前端知道这份展开是
  按数据还是按假设，不靠"帧的通道数和句柄对不上"去猜；对不上时再拿
  `list_taps` 刷新。
- feed 时若选列或**触发源**的下标在这一帧的布局里够不着（帧宽变了、带组序的引
  用指到了不存在的组），这一帧当"通道数变"：清环、什么都不发，等布局对上再
  从头攒。**不得 panic**（触发源与选列同样要查）。应答里 `channels` 为空而声明
  非空，就是"这个布局下够不着"的出口。
- 契约与帧宽要自洽：无组的契约要求帧宽 == 列数，有组的要求帧宽是列数的整数
  倍；不满足同样当"布局不合"不发——传进来的契约是一句主张（见 3.6），帧是证
  人，两者对不上时不能按错的契约给列贴名。
- 无契约的口 `channels` 只能是 `@n`。
- 环按字节封顶：`window_samples × 选中列数 × 4 B` 超过 **64 MiB/tap** 时
  `create_tap` / `set_tap` 直接拒（按已知布局估，没见过布局每项按一组算）——
  `window_samples` 是 u32，不封顶的话一个错的绑定就是几 GiB 的分配，而那个数在
  系统里没有别的出口。

### 3.2 选列

feed 时按 scan 抽选中的列进环/累加器（所有模式都适用），帧上 `channels` = 选中
数、顺序 = 声明顺序。环只按选中列开。

### 3.3 触发状态机（window 模式）

环以 scan 计：容量 `window_samples` scan × 选中列数。维护 `abs`（自环重启起的
scan 数）、`contig`（自上次断起的 scan 数）、`prev`（源列上一拍的比较态
Low/High/未知）、`last_trig`、`pending: Option<{trig_abs, post_left}>`、
`captured: Option<{samples, trig_offset}>`、`running`。

每拍（一次 feed 可含多拍——200 行一帧的口触发和 post 到齐都可能在同一 feed 里）：

1. 写环；`abs += 1; contig += 1`。
2. 读源列 v。NaN → 跳过 3–4（不触发、不更新 prev）。
3. 比较态：rising 时 `v ≤ level − h/2` 进 Low、`v ≥ level + h/2` 进 High；跨越 =
   prev==Low ∧ 现在 High（falling 对称，either 两边都认）。h = 0 时就是
   `prev < level ∧ v ≥ level`。
4. armed = `contig ≥ pre` ∧ `pending.is_none()` ∧ `t − last_trig ≥ holdoff`
   ∧ running（t = 本次跨越拍）。跨越且 armed → `pending = {trig_abs: t}`。
   **触发在写环之前判**：position = 1（post = 0）时窗是 `[t − pre, t)`，触发拍
   一写进去就会把 `t − pre` 那拍冲掉。
5. **触发拍本身算 post 的第一拍**：`pending` 存在且 `abs == t + post` → **立刻**
   从环切出 `[t−pre, t+post)` 到 `captured`（`trig_offset = pre ∈ [0, window]`；
   position = 1 时 = window，触发拍不在窗里、T 标记在右缘——前端要接受
   `trig_offset == 帧长`），`last_trig = t`，`pending = None`。single 模式：立刻出
   帧（flags TRIGGERED|STOPPED），`running = false`。
6. feed 末尾：`captured` 存在且到刷新点（`frame_count == 0` 或距上次出帧 ≥ 周期）
   → 出帧（TRIGGERED）；没到就留着，后来的捕获覆盖它（一个周期内多次触发只留
   最新）。**注意**：留着的捕获要等下一次 feed 才有机会发——数据停了它最多晚一
   个刷新周期；不为此加定时器。
7. auto：running ∧ `pending.is_none()` ∧ 环已满 ∧ `abs − max(last_trig, 环重启点)
   ≥ 2 × window_samples` → 按刷新点出自由跑的窗（不带 TRIGGERED，
   `trig_offset = 0`）。"2 个窗长"写死并注释。normal：没触发不出帧。
8. 断（帧头 `disc` ∨ 跳号——只有 `n_samples > 0` 的帧才谈跳号，老帧头 /
   `feed_tap` 注入没有轴 ∨ 口通道数变 ∨ 选列或触发源下标够不着）→ 清环
   （`contig = 0`、环内容作废）、`pending = None`、`prev = 未知`；**已切好、在等
   刷新点的 `captured` 留着**（它是完整的一窗）；之后要重新攒满（无触发时也要攒
   满一窗才出）。**没有触发的 window 模式同样清环**（拍板 ②：与 stream 一致；
   SDK 按约定维护 sample_index，不维护的生产方会表现成"窗永远填不满"——响亮，
   不是静默拼假窗）。
9. `pre = round(position × window_samples)`，`post = window_samples − pre`。
   position = 1 时 post = 0：触发拍到齐即切窗，允许。

### 3.4 run / stop / single（三种模式都认）

- stopped：`feed` 对这个 tap 直接返回（不写环、不出帧）；环留着。stop 那一刻
  stream 累加器里有货就带 STOPPED 位冲出去；window 模式里切好、还没到刷新点的
  `captured` 同样带 STOPPED 冲出（它比屏幕上那窗新，示波器的 Stop 显示的是最
  后一次采集）。
- run：清环、`pending = None`、`captured = None`、`prev = 未知`、re-arm，并让下一
  帧带 DISCONTINUITY；**清环**是因为停了一段再跑，旧样本和新样本之间隔着一段不
  存在的时间。`single` 命令 = run + 下一帧发完就停（三种模式都认；window 带触发
  时 = 捕一次）。
- `set_tap` 换 trigger：等待中的捕获与比较态作废；`oneshot` 按**新**配置重算
  （从 Single 换到 Auto 要清掉，否则下一帧带 STOPPED 把 tap 停了）。
- 状态 `TapState { running, armed, trig_mode: Option<TrigMode>, triggers: u64,
  stopped_by: Option<"user"|"single"> }`，`create_tap`/`set_tap`/`tap_control` 应答
  都带；`list_taps` 逐 tap 报 `{tap_id, port_id, config, state}`。

### 3.5 线协议 v3

```
[0x03][port_id_len:u16 LE][port_id][tap_id:u32][seq:u64][timestamp_ns:u64]
[sample_index:u64][channels:u32][flags:u32][trig_offset:u32][num_values:u32]
[samples f32 LE × N（按 channels 交织）][rms,peak,min,max,mean : f64 × 5]
```

flags：bit0 DISCONTINUITY、bit1 INDEXED（既有），bit2 `TRIGGERED`，bit3
`STOPPED`（single 捕到的那一帧；也用于 stop 后如果还有最后一帧要发的情形）。
`trig_offset` 只在 TRIGGERED 时有意义。v1/v2 解码保留；`stats` 字段不动。
window 帧的 `timestamp_ns` 仍是完成这一窗的那个 payload 的 t0（今天就这样，图
不用它）。

### 3.6 RPC

| 方法 | 参数 | 应答 |
|---|---|---|
| `create_tap` | `{port_id, config, columns?, column_groups?}` | `{tap_id, channels: [{index, column: string\|null, group}], port_channels: n\|null, state}` |
| `set_tap` | `{tap_id, config, columns?, column_groups?}` | `{channels, port_channels, state, rebuilt: bool}` |
| `tap_control` | `{tap_id, cmd: "run"\|"stop"\|"single"}` | `{state}` |
| `remove_tap` | `{tap_id}` | `{ok}` |
| `list_taps` | — | `{taps: [{tap_id, port_id, config, state, channels, port_channels}]}` |

`channels` 应答是解析结果：每个发出去的通道来自口的哪个下标、哪一列、哪一组（无
契约的口 column = null、group = 0），按口最近一次见到的通道数展开；`port_channels`
说这个数是多少（null = 口还没出过数据，展开是按一组的假设）。前端拿它给图例分组
配色，不再按 `resolveColumns(port, nCh)` 假设 nCh = 组数 × 列数。`config` 先
`validate()`，错误原样回给调用方。

**消费口的契约**：它是父容器在快照里沿连线补的（`resolve_consumer_columns`），子
壳体自己不知道。所以 `create_tap` / `set_tap` 接受可选的 `columns` /
`column_groups`（`PortDescriptor` 同形），**只在本壳体的口没声明列时采用**，本壳
体声明了的以本壳体为准；前端从快照里的口描述子带上，`set_tap` 也要再带。不带
时消费口上的 `{column: …}` 会被拒（"port has no column contract — refer to
channels by index (@n)"）。传进来的契约是调用方的主张，帧宽与它不自洽时按 3.1
"布局不合"处理。

`feed_tap`（注入）照旧按 port_id 喂，广播每个出帧；参数加可选 `disc`，应答加
`frames` 数组（同口多 tap），旧的 `frame` 字段保留为首帧。

### 3.7 第二步（不挡触发，可后做）：按连接过滤 + 断线回收

server.rs 里 handler 签名不带连接身份，但服务端能看方法名和应答：`create_tap`
成功 → 把 `tap_id` 记到这条连接的 `owned`；`remove_tap` → 移除；连接关闭 → 对
`owned` 里每个 id 合成一次 `remove_tap`。二进制帧改成 `send_tap(tap_id, bytes)`
只进拥有者的数据队列。这一步同时修掉 tap.rs 注释里承认的"客户端不发 remove_tap
就断线会漏减账"的泄漏，和同口多 tap 后的 N² 扇出。

## 4. CLI（`crates/sigflow-cli`）

`widget bind <alias> port:<id> [--window N] [--refresh HZ] [--mode window|frame|stream]
[--channels a,b,2/c,@3] [--stats …] [--trig-source COL] [--trig-level V]
[--trig-slope rising|falling|either] [--trig-mode auto|normal|single]
[--trig-holdoff N] [--trig-hyst H] [--pretrigger 0.5]`

- `--mode` 今天缺（stream 从 CLI 表达不出来），补上。
- 任一 `--trig-*` 出现就要 `--trig-source` 和 `--trig-level`；其余取缺省。
- 组装完先 `TapConfig::validate()`，再发 `widget_bind`。
- `widget list` 里把 channels / trigger 打印出来。

示例（dex 的 foc-sil.sh 会用）：
```
sigflow-cli widget add scope_pwm sigflow.ui.waveform_chart
sigflow-cli widget bind scope_pwm port:pwm --window 0                # 生产方开窗的口：frame
sigflow-cli widget add scope_iq sigflow.ui.waveform_chart
sigflow-cli widget bind scope_iq port:rotor --mode window --window 400 --refresh 30 \
    --channels iq,iq_ref,id --trig-source z --trig-level 0.5 --trig-slope rising --trig-mode normal
```

## 5. 前端（`webui/src/lib`）

### 5.1 tap.ts

- v3 解码（`tapId`、`triggered`、`stopped`、`trigOffset`）；v1/v2 仍解。
- **每个子节点一条 WebSocket、上面复用多个 tap**：`TapLink(path)` 管 socket 与
  请求 id，`create_tap` 应答的 tap_id → 回调表，帧按 tap_id 分发。断线重连时对
  每个活着的 tap 重新 create（配置在句柄里）。
- 两种句柄：`TapHub.subscribe(path, port, config, cb)` 保持共用语义（状态控件那
  一族：同（路径,口,配置）一份 tap），键仍含整个配置 JSON；新增
  `TapHub.own(path, port, config, cb) -> TapHandle`（示波器独占）：
  `handle.update(config)`（几何相同走 `set_tap`，不同则 set_tap 会原地重建）、
  `handle.control(cmd)`、`handle.state`、`handle.channels`（解析结果）、
  `handle.close()`。
- 每种句柄的 `channels: []` 都要写全（TS 必填）。

### 5.2 WidgetView / ScopeChart

- waveform_chart 走 `own`；tapKey 只含几何，`bind.tap` 的其余变化走
  `handle.update`（否则回写绑定会触发重订阅，4096 窗 @200 Hz 要 20 s 重新填满）。
- 触发 UI：右缘电平标记（画在源列的轴上；源列 AC 时画在 level − mean）、顶缘预
  触发 T 标记、面板（源/沿/电平/迟滞/释抑/模式；释抑在有 `declared_rate_hz` 时并
  排显示时间；"抗噪"一键 = 源列可见 pk-pk 的 5% 写进 hysteresis）；徽章
  Auto / Trig'd / Armed / Stop；Run / Stop / Single。改动即时 `set_tap`，手势结束
  / 输入提交后防抖回写 `widget_bind`（同 alias/kind/target，只换 tap）。
- 时间轴：TRIGGERED 帧 t=0 在 `trigOffset`，前负后正；自由跑 / stream 右缘为 0。
- 逐通道刻度：每通道 `{perDiv, offset, linked}`，8 格网格；初始按通道自适应到 6
  格、1-2-5 步进；**缺省按契约 `unit` 共享**（同 unit 的列同一刻度），无契约全
  体共享；有契约但没写 unit 的列**各自独立**——两列能不能比，只有共同的 unit 说
  了算，没声明就不推断（`count` 和 `miss_total` 都没 unit 不等于它们同一把尺）；
  单通道可解锁。活动通道（一个或没有）：Y 沟槽滚轮/拖只动活动通道，没
  有活动通道动全体；双击重置。通道状态行 `iq  50 mA/div  ↕0  AC` 代替轴上标数。
  `y_min/y_max` 都给了 = 所有通道初始锁到同一量程。
- DC/AC：AC = 减本帧均值（stream 减可见跨度均值）；测量/光标读耦合后的值并标
  (AC)。
- 光标：时间一对（Δt、1/Δt、各可见通道两处的值）；幅值一对挂活动通道的轴。
- 测量条：勾选通道各一行（见 5.3）；`show_stats` 只管首次是否展开。
- 本地存储（localStorage，每控件实例一份，键用列 id）：通道 `{coupling, perDiv,
  offset, linked}`、光标开关与位置（时间光标存相对触发点/右缘的拍数，幅值存列 id
  + 值）、测量勾选、测量条开合。**图例隐藏仍不存**（声明的通道集在 `bind.tap.
  channels`，那是"这台看什么"；隐藏是瞥一眼）。
- 弹出：通用机制——任何面控件可弹到 page 那套浮窗里（复用 App.svelte 的
  pagehost：拖动/最大化/关闭），里面渲染同一个 WidgetView，ScopeChart 收
  `full` prop 才显示触发面板、测量表、光标读数；面上只留 Run/Stop 与徽章。
- 删掉现在混通道的 `statsText`（`rms · peak`）。

### 5.3 scope-measure.ts（纯函数 + vitest）

输入：一条通道在门内的样本（耦合后）、可选 `dt`（秒/拍，来自 declared_rate_hz）。
- 垂直：`max`、`min`、`pkpk`；`top/base` 直方图法（100 桶，mid = (max+min)/2 之
  上的众数 = top、之下 = base）——**众数不明显就回落到 max/min**（IEEE 181 的
  做法：众数桶的计数不到该半边平均桶计数的 2 倍，就当没有平顶）。锯齿 / 三角
  （pwm 计数器、三角载波——正是这套控件要看的信号）的直方图是平的，不回落的话
  top 落在 mid 附近、amplitude 只剩 pk-pk 的一半，mid 电平跟着错、duty 从 0.5
  变成 0.75；正弦在极值处有明显众数（≈ 6 倍于平均桶），不受影响。
  `amplitude = top − base`；`mean`、`rms`；有 ≥1 整周期时 `cycMean`/`cycRms`
  （首个上升沿到最后一个上升沿之间）。NaN 跳过。
- 水平：`mid = (top+base)/2`，迟滞 ±10% amplitude 的状态机找沿，沿位置在两拍间
  线性插值；`period` = 相邻同向沿间距均值 + `cycles` 数；`freq = 1/period`；
  `posWidth`/`negWidth`；`duty`；`rise`/`fall` = 10%→90%（对 base/top）。NaN 拍
  断链（链从头再来）。
- 没有 dt 时所有时间量以拍为单位并标明；找不到沿 → `null`（UI 显「—」），**不是 0**。
- 测试：正弦（幅值/period/rms 已知）、带过冲的方波（amplitude ≠ pkpk）、脉冲
  （±width、duty）、直流（period null）、含 NaN 段、无 dt 时的拍数单位、少于一个
  周期。

## 6. 测试与验收

壳体单测（tap.rs，**变异验证**：把 holdoff 判断、迟滞判断、断清环各去掉一次，
对应测试必须红）：
- 同口两份 tap 独立（不同窗长/选列；remove 一个另一个照发）；
- 选列：只发选中列、顺序按声明、`group` 展开、`@n`；
- rising / falling / either：窗里 `trig_offset` 处正是跨越点；
- 迟滞带内的噪声不触发；释抑内的第二次跨越不触发；
- position 0 / 0.5 / 1 的窗边界；pre 没攒够不 arm；
- 一次 feed 多拍（200 行）里触发与 post 到齐都在同一 feed；
- 刷新截流：一个周期内多次触发只留最新；
- single 出帧带 STOPPED、之后 feed 不出帧、run 后再捕一次；
- auto 2 窗没触发出自由跑窗（无 TRIGGERED），触发后回到触发窗；normal 不触发不出帧；
- 断（声明 / 跳号 / 通道数变）清环、取消捕获，重新攒满才出——无触发的 window 也一样；
- NaN 透明；stop 丢样本；run 清环；
- v3 编解码往返、v1/v2 仍能解。

shell.rs：列 id 解析（含 group 展开、多组口触发源缺 group 拒、未知列拒且列出可
选）、stream 带触发拒（validate）、`set_tap` 几何相同不重建 / 不同重建。

前端 vitest：tap.ts v3 解码与按 tap_id 分发；scope-measure；刻度按 unit 共享；AC
耦合；光标换算。

真口验收（dex 的 foc_sil，假 HOME + 独立端口，别碰 9500）：
- rotor：`z` rising 0.5 触发 → 每窗触发点处 `z = 1`、`z_deg` 读数稳定；`iq` 电平
  触发 normal 模式；两台示波器各选不同列，一台 stop 另一台照跑；
- pwm（frame 口）：测 `pwm_cnt` 周期 12.5 µs ⇒ 80 kHz；va/vb/vc 与载波同图；
- stream 口 stop 后历史可翻、run 后从新样本起。

## 7. 分支、顺序、注意事项

- sigflow-core 现在的工作树在 `fix/tap-header-disc`；`feat/motor-disc`（8ee1a25）
  和 `fix/tap-header-disc`（eb0c555、4e42e9f）都未合 main、未推。新活从
  `fix/tap-header-disc` 开 `feat/tap-scope`，先把 `feat/motor-disc` 合进来（两支文
  件不重叠；`FeedMeta.disc`、`declared_rate_hz` 传播、MotorDisc 的 stream tap 都
  要在）。主干合并顺序归 hml。
- sigflow 工作树留在 `feat/tap-scope`（新类型），两仓路径依赖。
- 顺序：3.1 身份 → 3.2 选列 → 3.3/3.4 触发与控制 → 3.5/3.6 协议与 RPC → 4 CLI
  → 5 前端 → 6 真口验收 → 3.7。
- **别覆盖 `target/release/sigflow-shell`**：dex 在 9500 上跑的图用的是它。构建用
  单独的 `CARGO_TARGET_DIR`；验收图用假 `HOME`（`~/.sigflow/session.json` 和
  `~/.sigflow/plugins` 都在 HOME 下，直接 `sigflow-cli start` 会顶掉 dex 的全局
  会话）+ 端口 9502/9503；假 HOME 里要重装 data_monitor / waveform-chart /
  foc-sil 包。webui 页面不自动连，headless 截图要先点 Connect
  （playwright-core@1.50 + `~/.cache/ms-playwright/chromium-1134`）。
- 另一套触发：`plugins/native/trigger-capture`（`docs/trigger-capture-e2e.md`）是
  数据面里按物理时刻对齐的**捕获节点**，产出 `capture` 口给下游处理/记录；本文
  的触发是观测路径上的示波器触发，两者并存、不替代。

## 8. 已拍板（2026-08-29）

1. 触发设置持久化到绑定，不进 localStorage。
2. window 模式遇断清环（与 stream 一致）。
3. 释抑按拍数，UI 换算时间。
4. 预触发缺省 50%；auto 超时 2 窗写死。
5. 全功能 chrome 走通用"弹出到浮窗"，不另做 page 控件。
6. 服务端按连接过滤 tap 帧作为第二步。
7. 缩放缺省按 `unit` 共享。
8. 选列住 tap（带宽）；声明的通道集与图例临时隐藏分开存；一个口一条采样轴是
   "全放一个口"唯一的边界。
