# 示波器（scope）的类型契约

2026-08-30。hml 试用 tap 示波器后决定重做：示波器成为**生产口所在壳体里的一台采集
引擎**（深度到 8 GB、采样率到 100 MS/s、通道数增加降采样率、生产方不自己开窗、超预
算走抽取），tap 退回纯订阅（保留选列），`waveform_chart` 退回看波形。引擎、RPC、线
协议、前端在 sigflow-core `docs/scope.md`（feat/scope）；本文是类型层的契约，落在
sigflow `feat/scope`（从 feat/tap-scope 分出）。`tap-scope.md` 里"触发住在 tap 里"的部
分作废，tap 身份 / 选列 / 线协议 v3 / 按连接过滤保留。

## 1. dtype 与码→物理量（`semantic.rs`、`manifest.rs`）

- `Dtype` 枚举 `i8 | i16 | i32 | f32 | f64`（`elem_bytes`、`is_integer`、`FromStr`）；
  `SemanticType.dtype` 仍是字符串，但只能是这五个名字之一，**没写 = f32**
  （`SemanticType::dtype()`，不认识的名字是错、不猜）。
- `ColumnDecl.scale` / `offset`（缺省 1 / 0）：`物理量 = 码 × scale + offset`。
  `unit`/`bound` 指物理量；`decode`/`bits` 作用在**码**上。
- 连线规则（sigflow-core 的 connect 校验）：生产口 dtype 是整数时，消费口必须显式声明
  同一 dtype；消费口没写 dtype = 只收 f32/f64。理由：今天的消费方都按浮点读样本，
  收到 i16 就是静默读错。
- tap 对整数口按 scale/offset 换成物理量 f32 再发（线协议不变）；示波器按原生 dtype
  存，视图/测量时才换算——示波器不制造分辨率。

## 2. 协商（`PortDescriptor.negotiable`）

```toml
[[ports]]
id = "adc"
negotiable = { rate_param = "fs_hz" }
```

- `rate_param`：f64 Hz 的 hot_change 参数。消费方（示波器 / 录制器）写它请求的率；生
  产方就近取自己支持的率，**回读值 = 接受的率**，并且**每一帧带 `sfrate01` 注解**说实
  际率——帧是证人，回读只是应答。
- 声明了 `negotiable` 的口**不得**声明 `declared_rate_hz`（`PortDescriptor::validate`）：
  那条只给编译期常量，协商口的率是运行时的。
- 降率只许峰值检测（`FLAG_PEAK_PAIRS`）或均值，**不许丢样本抽取**——那是混叠。SDK 的
  `Decimator` 助手二期随第一个实现 `rate_param` 的生产方一起做。
- 选列协商（生产方只算、只发子集）**这一轮不定**：帧只有一个注解槽（`[schema_id |
  bytes]`），子集会让帧的列布局和契约分家，要有带列表的流注解（rate + cols 合一个
  schema）才能做——二期定。一期的预算只靠 `rate_param` + 入口抽取。

## 3. `FLAG_PEAK_PAIRS`（`frame.rs`，bit3）

每 scan 每列 `(min, max)` 交织，`n_samples` 计的是 scan 数（对数），payload =
`n_samples × 列数 × 2 × elem`；率（declared 或 sfrate01）是抽取后的率。只在生产方响应
协商时出现，所以：声明了 `negotiable` 的口，其消费方必须认这个标记——示波器按对存，
tap 作为 `TAP_FLAG_PEAK_PAIRS` 传下去、图画 min/max 竖条，**不许只取一半**。非 signal
列（enum / bitfield / counter）的对也是 `(min, max)`，两者不等 = 这一 bin 里变过。

## 4. `ScopeConfig`（`ui.rs`，持久化在 `WidgetBinding.scope`）

TOML 形状在 `ScopeConfig` 的文档注释里。要点：

- `channels`（空 = 整口）、`depth_bytes`（> 0，上限归壳体环境变量）、
  `budget_bytes_per_s`（缺省 400 MB/s；**按存储字节算**——峰值存储每 scan 一对，
  `D = ceil(2 × fs_native × Σbytes / budget)`）、`refresh_hz`。
- `timebase`：`span_s` 或 `span_scans` 二选一必填；`position ∈ [0,1]` 缺省 0.5 = 触发点
  在屏幕的位置——它是时基（水平位置旋钮），不是触发的属性，`ScopeTrigger` 里没有。
- `ScopeTrigger` = 原 TapTrigger 去掉 position、加 `kind: edge | pulse | window`（一期只
  实现 edge；pulse / window 的参数二期加可选字段）。状态机语义同前（迟滞、释抑按拍、
  NaN 透明、断清连续段、触发拍算 post 第一拍），加亚采样相位 `frac`；峰值存储时
  rising 在 max 平面、falling 在 min 平面。
- `acq`：`normal | average | persist`；`average_n ≥ 2` 且 average 必须有触发；
  `persist_decay ∈ [0,1]`，**1 = 不衰减**（没有 "inf"）。
- `vertical[]`：逐通道 `v_div > 0`、`offset`、`on`、`coupling dc|ac`、`interp
  none|linear|sinc`、`bw_limit_hz ≥ 0`；`channel` 必须在 `channels` 里（空 = 整口时不
  查）。AC = 减去当前记录窗内的均值，密度 / 视图 / 测量都按耦合后的值。
- `measure`：`channels ⊆ channels`、`gate screen|cursors`；`gate = cursors` 时
  `cursors = { a, b }` 必须给（相对时间零点的存储拍数，可为负，a ≠ b）——光标是
  setup 的一部分，UI 拖光标即时 `scope_set`、手势结束回写；引擎不会拿屏幕代替。
  测量在引擎里按记录算，随 meas 帧发，不适用 = NaN 不是 0。
- **拍数的度量**：`timebase.span_scans` 与 `trigger.holdoff_samples` 都是**源口的拍**
  （抽取前）；入口峰值检测 D:1 之后引擎自己除以 D 换成存储拍，操作者不用知道 D。
  `measure.cursors` 相对时间零点，按**存储拍**计（它是视图坐标）。
- 几何（改了重建环）= `channels + depth_bytes + budget_bytes_per_s`；其余就地。
- `TapConfig` 去掉 `trigger`（旧绑定里的 trigger 字段读时被丢，不报错——它只存在于没
  合过的分支上）；`TapTrigger` 类型删除，`TrigSlope` / `TrigMode` 保留给 ScopeTrigger。
- 一个口绑定要么 `tap` 要么 `scope`，按控件类型二选一。CLI：
  `widget bind <alias> port:<id> --scope-file setup.toml`；壳体 `widget_bind` 对本节点口
  当场解析列 id（同 tap 那条）。

### 4.1 引擎落地时核过的几条（sigflow-core 92868fd）

- 外触发源不在选中列里时追加进环（环列 = 显示列 + 触发源列）；vertical / measure 按
  （列 id, 组）身份配对；率取 sfrate01 > declared_rate_hz；`span_s` 而口无率 → 拒。
- `D = ceil(fs × 存储字节/scan / budget)`，源已是对时字节本来翻倍、不再乘 2。
- 视图帧的 `kind` 加了 2 = 原始 (min, max) 对（对存储放大到每像素不足一拍时）。
- 口在对 / 非对之间切换而环布局不合（decim = 1）时帧被丢并计入 `dropped_scans`
  ——不会静默按错布局写。
- **view 帧的横坐标约定**（hml 第二轮试用报的"电平、触发位置、波形不交于一点"，
  sigflow-core f63e466）：`kind = 1 / 2`（原始样本 / 原始 (min,max) 对）里第 j 个在
  **拍坐标 `x0 + j`** 上，不是"第 j 格的格心"；`kind = 0`（每像素列 min/max）第 i 列
  覆盖 `[x0 + span·i/px, x0 + span·(i+1)/px)`，画在**列心**。两种约定混用（把样本
  当格）会让波形整体比触发点右移半拍——T 标记画在整拍位置 `ref_abs`，一屏只有几
  十拍时半拍就是几十像素（实测 20 拍一屏 / 1177 px 时 28 px）。台阶插值
  （`interp = none`）保持到下一拍（ZOH），跳变画在样本自己那一拍上，与触发点同处。
- **触发点钉住**（hml 试用反馈，sigflow-core 4ae9479）：触发窗的时间零点是
  `ref_abs − 1 + frac`（跨越点在两拍之间）。T 标记钉在 `ref_abs` 那一列，**波形、密度
  累积、光标读数一律整体右挪 `1 − frac` 拍**，各窗的跨越点才叠在同一像素列上（时基越
  小一拍占的像素越多，不挪就左右抖）。`measure.cursors` 仍按相对 `ref_abs` 的整数存
  储拍存，显示与读数按挪过的位置取。
- 密度图放大到样本级：列数下限 1024（不再 = span），每列在两拍之间线性插值取列两端
  的 min/max——相邻样本连成斜段而不是一拍宽的台阶块；每个采集 tick 最多累 512 窗
  （超了抽着累，形状不变）。实测 1 MS/s 正弦 5 µs/div、触发 1 万/s 仍 30 帧、壳体
  12% CPU。
- `scope_state {scope_id}` → `{state}`：只拿运行态（触发/s、丢拍、写指针），控件按秒
  轮询状态行用；view 帧只带 flags，触发率不随帧走。
- view 帧每通道带 `ac_mean f32`（`[ch u32][kind u8][ac_mean f32][n u32][f32 × n]`，DC 为
  0）：AC 耦合时引擎从样本里减掉的均值只有引擎知道，控件用它把电平线画在
  `level − ac_mean`、拖电平时把均值加回去再 `scope_set`，光标处的通道读数也按它
  算（sigflow-core 47c6c41 落地）。
- 单列多组的色相规则住在 `traceColor(col, group, groups, ncols)`：`ncols === 1` 且
  `1 < groups ≤ 8` → 每路一个色相，与单组时第 group 列同色；waveform_chart 与
  oscilloscope 都经它取色（47c6c41）。

## 5. 控件声明

- `plugins/ui/oscilloscope/manifest.toml`：`presentation = "page"`、绑 port、config 只有
  `label`——旋钮不是控件配置。
- `plugins/ui/waveform-chart/manifest.toml`：退回看波形（tap 订阅、选列、stream 历史），
  头里不再有触发 / 测量 / 光标 / 刻度那段。

## 6. 测试源 `plugins/native/sig-gen`（sigflow 仓，原生插件都在这里）

一期：`fs`（改它 = 新采样轴 = 断）、`channels`、`dtype i16|f32`、每通道波形
正弦 / 方波 / 三角 / 脉冲 / 噪声、幅值 / 频率 / 占空 / 相位、注入跳号的 action；**不声明
`negotiable`**（协商二期），超预算时靠它验入口抽取。验收与压测（100 MS/s、8 GB）都以
它为源，不依赖 dex 仿真能否连续实时。

## 6.1 验收记录（sigflow-core feat/scope a4a5c67 + sig-gen 5b71dfa，release，假 HOME/9502，i7 级桌面）

- ✅ 单通道 i16 100 MS/s、8 GB 深度、连续 60 s：入环 100.0 MS/s 稳、丢拍 0；1 MHz 正弦
  上沿触发 10 万次/s（受 pre+post 重臂限）；环文件 6.46 GB（预算减金字塔）在 /dev/shm；
  采集线程 ≈ 0.8 核；20 亿拍全景视图 RPC 5 ms、8 亿拍 2 ms。
- ✅ 1 MS/s × 2 ch f32、10 kHz、normal 触发 + persist：view 20 Hz、density 两通道、meas
  freq 10000.0 / period 100.0 µs；触发在窗中点、frac 1.000（10 kHz @ 1 MS/s 正好采到
  0）；分段 256 条；stop / single / 回翻 / set 不重建 / 改深度重建全过。
- ✅ 4 ch f32 100 MS/s（1.6 GB/s → 8:1 峰值检测存 12.5 MS/s）：sigflow-core 1961b9e 之后
  存 12.5 MS/s 稳、丢拍 0、触发 10 万/s；入环线程 68%、采集线程 49%、处理线程 19%。
  之前只到 20.7 MS/s 的原因**不是生成也不是入环的算力**（sig-gen 纯生成 f32×4 正弦
  1054 M 样本/s ≈ 0.95 ns/样本；RingWriter 8:1 抽取 528 → 向量化后 676 MS/s），而是壳体
  两条结构限制，已改：插件输出缓冲原来固定 1 MiB 不看 `max_frame_bytes`（现在按声明
  开，只增不减）；源节点 tick 原来是"干完再 sleep 1 ms"（现在固定 1 kHz interval）。
  改完 87 MS/s（处理线程上生成 + 抽取 ≈ 1.14 核卡住），再把入口抽取挪出处理线程
  （feed 只 memcpy 进 64 MiB 有界暂存队列，独立入环线程抽列/抽取/写环，采集线程只做
  金字塔/触发/密度/推帧）才到 100。没连消费方时壳体不发布（`has_publisher` 为假直接
  跳过），没有共享内存的白拷贝。

## 7. 归属

- sigflow（本仓）：§1–§5 的类型与声明（已落）、§6 sig-gen。
- sigflow-core：引擎 / RPC / 协议 / 前端 / connect 的 dtype 校验 / tap 的 scale·offset 换算
  与 PEAK_PAIRS 透传 / `waveform_chart` 与 tap 的退回。
- 合并与装包归 hml；两仓 `feat/scope` 都建在 `feat/tap-scope` 上。
