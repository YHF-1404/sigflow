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
- **`ColumnRef` 的相等是"写法相等"，不是"指同一列"**：单组口上 `{ column = "v" }`
  （每组）与 `{ column = "v", group = 0 }`（第 0 组）指着同一路，`PartialEq` 却判不
  等。拿它当 map 键 / 做集合比较的代码要**先落到具体通道**（交织下标，或 (列 id, 组)
  一对）再比——sigflow-core 的触发源下拉曾整个显示为空，就是 CLI 写 `0/v`、控件按形
  状省成 `v`，两边按写法取键对不上（c079449 修）。类型侧 `selected_in` 的方向也记在
  它的文档注释里：`sel` 里不带组序的覆盖该列所有组，反过来不成立。
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
- **余辉跟着通道的插值方式走**（hml 第三轮试用，c079449）：密度累积在壳体、矢量线在
  前端，原来密度恒按两拍连线，选了台阶 / sinc 后同一条波在余辉里和矢量线上形状不
  一样。现在 `VerticalSetting.interp` 一并传进密度累积（引擎内部字段，线协议不动）：
  样本级放大时列是两个边界值之间的一段，边界按 interp 算——`none` 取所在拍（跳变列
  自带竖线，与 ZOH 矢量线同处）、`linear` 两拍连线、`sinc` 8 抽头 Lanczos（权重查
  表，256 档亚拍位置，与前端同样归一化、同样夹边界）。`interp` 变了和 `v_div`/
  `offset` 一样清计数。
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

一期：`fs`（改它 = 新采样轴 = 断）、`channels`、`dtype i16|f32|both`、每通道波形
正弦 / 方波 / 三角 / 脉冲 / 噪声、幅值 / 频率 / 占空 / 相位、注入跳号的 action；**不声明
`negotiable`**（协商二期），超预算时靠它验入口抽取。验收与压测（100 MS/s、8 GB）都以
它为源，不依赖 dex 仿真能否连续实时。

`dtype = "both"` 是**多源示波器的验收源**：两个口同时发、各自一个率（f32 走 `fs_hz`、
i16 走 `fs2_hz`），一个节点上就凑得出两个不同率的源进同一台示波器（跨节点是 12-b）。
两条流锚在同一时刻、同一相位，相位每拍推进 `freq/fs`，所以在**绝对时间**上是同一根
信号——重采样到同一根时钟之后同一拍上两路该读到同一个值，差多少就是对齐误差
（§8.4 量到 3.9e-5 = i16 的量化底）。多流化的代价：同机同日 f32×4 聚合从 977 M
样本/s 掉到 904 M（≈ 7%），仍是 100 MS/s 需求的两倍以上。

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

- sigflow（本仓）：§1–§5 的类型与声明（已落）、§6 sig-gen（含 `dtype = "both"` 两口
  同发的多源验收源）、§8 固定时钟与多源的类型与真图验收。
- sigflow-core：引擎 / RPC / 协议 / 前端 / connect 的 dtype 校验 / tap 的 scale·offset 换算
  与 PEAK_PAIRS 透传 / `waveform_chart` 与 tap 的退回。
- 合并与装包归 hml；两仓 `feat/scope` 都建在 `feat/tap-scope` 上。

## 8. 固定时钟与多源（2026-09-01）

出处：hml 试用后要求"示波器有自己的采样率、任何率/任何位宽的源都重采样到这根时钟
上、所有口的所有通道能进同一台示波器，8 MS/s 够用"——**由 sigflow-core-03 转述，hml
本人没在我这会话说过**。引擎侧设计（重采样核、混合写环、跨节点 attach、代价）在
sigflow-core `docs/scope.md` §12；本节是类型层的落地。

### 8.1 落了什么（`ui.rs`）

- `ScopeClock { mode: native | fixed, fs_hz: Option<f64> }`，`ScopeConfig.clock`。
  `native` = 现状（跟着唯一的源走、原生 dtype、整数抽取 + 峰值对，100 MS/s 抓 1 拍
  毛刺那条路留着）；`fixed` = 示波器自己的时钟，各源重采样上来，每 scan 每列一个值。
  `fs_hz` 省略 = auto。**进 `same_geometry`**。
- `ScopeSource { name, node, port, channels }`，`ScopeConfig.sources`。`node` 省略 = 挂
  控件的那个节点；`channels` 空 = 整口。老的 `ScopeConfig.channels` 留作**单源简写**
  （源 = 绑定的那个口），两者互斥。
- `ChanRef { source: Option<String>, #[serde(flatten)] column: ColumnRef }`：
  `trigger.source`、`vertical[].channel`、`measure.channels` 的类型从 `ColumnRef` 换成
  它。摊平序列化 → **老 setup 一字不改照旧能读**（`source = { column = "z" }`），
  TOML 写回也验过（`多源_setup_能写回_toml_再读回来`）。文本形式 `源:列`，
  如 `foc/rotor:0/theta`。
- 解析入口 `ScopeConfig::resolve(&ChanRef) -> (源槽位, &ColumnRef)`，配套
  `source_count` / `source_names` / `channels_of(slot)`。

### 8.2 我改了提案的哪几条，为什么

1. **源引用按名字，不按 `sources` 下标**（提案是 `source: Option<u32>`）。下标是位置：
   中间插一个源、删一个源，所有引用会**悄悄**指到另一路去——那正是"口加一列就错位"
   这笔账（§4 `ColumnRef` 那条）。名字错了 validate 当场报。缺省名 = `port`，给了
   `node` 就是 `node/port`；重名要求写 `name`，名字里不许有 `:`。
2. **两个以上源时省略 `source` 是错，不是"取第 0 个"**。猜错了就是量错一路，而这一路
   在屏幕上和量对了长得一模一样。只有一个源时可以省。
3. **`fs_hz` 的上限归壳体**（`SIGFLOW_SCOPE_MAX_FS`，缺省 8e6），类型层只查有限且
   > 0——和 `depth_bytes` 同一分工，类型层不该知道某台机器的环境变量。
   `mode = native` 时给 `fs_hz` 是错（写的人以为它有用）。
4. **`fixed` 下触发源必须在选列里**。`native` 的外触发成立是因为写环时读整行；`fixed`
   下环里只有选中的列，没进环就不在共同网格上，触发无处可跑。validate 拦。
5. **拍的度量跟着时钟走**（提案没说，会当场差一个 D）：`span_scans`、
   `holdoff_samples`、`CursorPair` 在 `native` 下是**源口的拍**（引擎自己 /D），在
   `fixed` 下是**示波器时钟的拍**（没有 D，不换算）。
6. **`fixed` 环一期恒 f32**。§12.1 提的"可选 i16 换深度"没进类型层：i16 存重采样后的
   物理量需要一个量化标度，标度从哪来（列 `bound`？每通道 full-scale？）是个决定，
   不是个字段。定了再加 `clock.dtype`，二期。

### 8.3 校验规则（`ScopeConfig::validate`）

`channels` 与 `sources` 二选一；每个 source 的 `port` 非空、`node`/`name` 给了就非空、
解析名唯一且不含 `:`；两个以上源要 `clock.mode = fixed`；`native` 不接受 `fs_hz`，
`fs_hz` 给了要有限且 > 0；`trigger.source`/`vertical[].channel`/`measure.channels` 都要
能 `resolve` 到一个源，且（除 `native` 的外触发外）在该源的选列里。

列 id 存不存在、节点/口在不在图上、`fs_hz` 与 `depth_bytes` 超不超本机上限、无率口在
`fixed` 下拒绝入环、停摆源填 NaN 标 missing——都到壳体才知道，归引擎。

### 8.4 多源真图验收（2026-09-01，sigflow-core 6d38b5c + sig-gen 两口同发，release，假 HOME/9502）

一个 sig-gen 节点两个口（f32 1 MS/s + i16 250 kS/s，绝对时间上同一根 1 kHz 正弦、
幅度 0.9）进同一台固定时钟 500 kS/s 的示波器：

- ✅ **对齐**：同一拍上两路最大差 **3.9e-5**（rms 2.0e-5）——就是 i16 的量化底
  （1/32768 = 3.1e-5），不是对齐误差。频率量到 1000.0 Hz。2 s 写指针 999983 拍
  = 正好跟上实时，丢拍 0。
- ✅ **停摆**：把 i16 口停掉（`dtype` both → f32），1.5 s 写指针照走 752035 拍
  （= 实时），停摆那一路 1000/1000 拍是 NaN、**0 拍是 0**；恢复后那一路回来，
  两路差仍是 3.9e-5。
- ✅ **大抽取比**（1 MS/s → 10 kS/s = 100:1）：带内 1 kHz 出来 0.896，带外 12 kHz
  （新奈奎斯特的 2.4 倍）残量 **5e-6**。
- ✅ **口的归属**：口名打错、源里写别的节点的口，都当场报并指路。
- ❌ **发现的问题**：抽取比一大就丢拍——ratio 2 丢 0、ratio 10 丢 4%、ratio 100 丢
  **65%**，且环跟不上实时。原因不在滤波器的算力，在 `Resampler::retune()`
  **每帧都建一张多相表再扔掉**（`kernel()` 先 `Poly::new()` 再比抽头数）。实测建
  一张表：32 抽头 0.47 ms、160 抽头 2.3 ms、800 抽头 11.2 ms、1024 抽头 14.4 ms
  ——按 1 kHz 的帧率就是 0.47 / 2.3 / 11 / 14 个核，全花在建了就扔上。已提
  sigflow-core：先算形状（抽头数 + 截止 + 预抽取比），变了才建表。

引擎级测试盖不到这条：那里的 `t0_ns` 是完美等间隔的，拟合出的率不抖、每帧走的是
同一个形状；真图上帧到得有抖动，形状每帧都要重算。**验收要在真图上做**，这是第三
次（前两次是 1 MiB 输出缓冲、sleep 1 ms 的源 tick）。

**复测（sigflow-core 9bc27e4，改成"先算形状、变了才建表" + 抽取比按 5% 等比阶梯向上
量化 + 2.5% 迟滞）**：三档比（1 MS/s → 500 k / 100 k / 10 kS/s）丢拍全 0、写指针全部
正好跟上实时；多源对齐仍是 3.9e-5、停摆仍是 1000/1000 拍 NaN。

**顺带量出固定时钟的通带形状**（1 MS/s → 10 kS/s，幅度按 rms×√2 量——用 max 会被采样
格点拖低，2.5 kHz 在 10 kS/s 上一周期才 4 个点）：

| 频率（× 新奈奎斯特） | 0.05 | 0.2 | 0.5 | 0.8 | 0.9 | 2.4 |
|---|---|---|---|---|---|---|
| 出来的幅度 / 真值 | 100.0% | 100.0% | 100.0% | 93.8% | 73.1% | 0.0% |

正常的抗混叠滤波器形状，但**对示波器是个要说清的事**：固定时钟下能照实读幅度的带宽
只到 **0.8 × 新奈奎斯特**（≈ `fs/2.5`），再往上是滤波器的滚降不是信号变小。要量某个
频率的峰峰值，时钟至少取它的 2.5 倍；抓毛刺仍然只能走 `clock = native`。

### 8.5 示波器的寿命：会话的 vs 节点的（2026-09-01，hml 拍板）

现状是**一台示波器跟着开它的那个 WebSocket 连接走**：连接一断壳体就替它
`scope_remove`（跟 tap 一套——8 GB 的环不能靠 GC 收）。代价是**刷新浏览器 = 重建环 =
攒下来的深度全丢**，而真机上就是一边跑一边翻历史。hml 定的是**分成两种寿命**：

- **会话的**：`scope_create` 临时开的，跟连接走，断线即收。现状不变。
- **节点的**：`bind.scope` 里**声明**的那台。声明就是拥有——节点起来它就起来、一直
  攒；浏览器刷新重新接上**同一个环**，深度还在；**节点停、或绑定删了**才释放。

三条随之而来的契约：

1. **身份是 (挂控件的节点, 控件 alias)**，不是 `scope_create` 时发的 `scope_id`
   （[`UiWidget::alias`] 在节点内唯一）。重连按这个键认领；`scope_id` 仍可以当运行期
   句柄，但它不是身份——**别用计时器猜谁拥有它**，那正是"声明胜过推断"要避免的。
2. **节点起来就建，不等浏览器**。等第一个观众才建环，"一直攒"就是假的。
3. **深度在节点启动时就占住**，装不下要**在启动时报错并说明**（不许悄悄缩小深度——
   那是把不正常换成正常）。一个节点上声明了几台示波器就是几份深度：foc-sil 那种五张
   脸 + 两页示波器的图，总量要先算得过来。

改几何（`clock` / `channels` / `sources` / `depth_bytes` / `budget_bytes_per_s`）照旧重建
环、攒的丢掉——那是几何变了，跟寿命是两回事。

**认领本身不许重建**（2026-09-01 真图上抓到过一次，sigflow-core 859b865）：浏览器按
alias 认领时把声明的 setup 原样送回来，几何理应相同。但"相同"是拿**解析出来的
`ScopeSpec`** 比的，里面带着 `srcs[].fs_native`——那是**观测**（口出过帧才知道的率），
不是声明。节点起来时环建在第一帧上，那一刻口的率还没记下来（`fs_native = None`），
于是第一个连上来的浏览器一认领就"几何变了"→ 重建环 → **节点攒的全丢**，而这正是
常驻要避免的事。规矩：**几何只由声明决定**；观测（率、组数一类）参与比较之前必须先
落成真正影响环布局的量（native 下是 `(decim, pairs)`，fixed 下根本不影响），或者在
观测到来时就地更新存着的 spec。

**建不起来分两类，按原因分，不按时间分**：`clock`/`sources` 写错、深度合计装不下——
这类永远不会自己好，**立刻喊**；"口还没出第一帧所以不知道帧宽/组数"是常态，**永远
不该喊**，`scope_list` 的 `failed` 里也该说"在等口出第一帧"而不是把按一组算出来的
`vertical[1]: channel 1/v is not among the displayed channels` 摆出来——那句话读起来
就是"你的配置写错了"，会让人去改一个没错的配置。用计时器把两类拖成一类，代价是真错
的那类晚三秒才响。

引擎住在**产源口的那个壳体**（12-a 下所有源同壳体）。绑定的 `node` 指子孙时，声明落在
宿主的 node.toml 里、引擎在子孙的壳体里，谁在启动时去建是壳体侧的事，但"节点起来就
建"这条不变。

### 8.6 给引擎的两条提醒

- `same_geometry` 比的是**写法相等**（`clock`、`channels`、`sources` 逐字段比）：把
  `v` 改写成 `0/v`、把源名从缺省改成显式 `name`，都会多重建一次环。保守，不算错，
  但别在回写绑定时顺手规范化写法。
- 前端的通道键要用 `resolve` 落出来的 `(源槽位, 列 id, 组)`，别拿 `ChanRef` 当键——
  它现在有两层写法相等（省不省源、省不省组），上一轮"触发源下拉显示为空"就是这个。
