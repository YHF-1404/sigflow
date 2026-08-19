# 参数文档：schema 驱动的结构化配置

`ParameterDescriptor` 描述**一个旋钮**——类型、范围、单位、`visible_when`。
插件自己那张扁平参数表用它就够了。它表达不了的是真实机器配置的形状：

- **重复**——四个轴，每轴五组工况；
- **库**——一个电机*型号*的值被所有装了它的轴共享，只在一处编辑；
- **引用**——轴上的一个*槽位*指向库里的某一条，必要时本地覆盖一两个字段；
- **继承**——一条库条目扩展另一条（`hold` 继承 `base`，只改三个字段）。

`sigflow-types` 的 `schema` / `doc` 两个模块补的正是这一层，别的一概不补：
字段仍然是 `ParameterDescriptor`。

对应的控件是 `sigflow.ui.param_table`，它不认识任何领域概念——树的形状、
表单的字段、单位换算、跨字段规则全部来自 schema。电机参数、称重标定、
工艺配方用的是同一个控件。

---

## 三个概念

### 字段组 `ParamGroup`

一张表单的字段集合，可带小标题 `sections`。字段就是 `ParameterDescriptor`，
新增了几个可选项（对插件自己的参数同样可用）：

| 字段 | 作用 |
|---|---|
| `section` | 归到哪个小标题下 |
| `note` | 为什么是这个值：台架实测、它防住了什么、和谁耦合 |
| `recommended` | 合法范围里**实测站得住**的那一段，超出只警告不拦 |
| `display` | 显示单位换算（`shown = stored × scale`），仿射且自足 |
| `guard` | `confirm`（改前确认）/ `unlock`（先解锁才能改） |
| `readback` | 值不落文档，由设备回读——派生系数、CRC、状态字 |

`note` 这一栏是有意加的。「1200（穿越 ~200Hz）明显容易振荡，600 稳」这类
知识现在只活在配置文件的注释里、只有作者读得到；它在编辑那一刻的价值高于
任何表单打磨，而代价只是一个字符串。

`guard` 与 `editable` 不是一回事。过流跳闸线完全可编辑而且必须可编辑，
它只是不该在扫增益的时候被顺手拧——而机器安全字段和调参字段挨在一起，
恰恰就是它被顺手拧的方式。

### 库 `LibraryDecl`

一族同形状的具名条目：电机型号、减速器、工况组。库条目是**共享**的——
改 `dm3510` 会改到每一个引用它的轴。这是它存在的意义，也是这类编辑器最大
的事故来源，所以控件顶栏显示引用计数，并提供"另存为新条目"。

`inheritable = true` 的库允许条目之间 `extends`。

### 树 `NodeDecl`

三种节点，也正好是树 UI 必须提供的三种行为：

| 节点 | 用户可以 |
|---|---|
| `group` | 改字段 |
| `slot` | 换引用（`allow_override` 时还可本地覆盖） |
| `collection` | 增 / 删 / 排序 |

轴*有*一个电机，是 schema 说的，那个槽位不能增删；有几个轴是集合，由用户
决定。这条区分是"我在 UI 里拖了一下，结果改了机器定义"不会发生的原因。

`slot` 上的 `nameplate = true` 表示只读铭牌：记录装的是**哪一条**，不产生
下发、表单不可编辑。驱动板就是这样一个槽位——PWM/死区/电流采样系数固化在
从站固件里，刻意不开下发通道（写错采样系数会让过流保护同步失效）。

---

## 文档格式

```toml
schema = "sigflow.example.servo_axis"
schema_version = 1

[libraries.motor.dm3510]          # <库 id>.<条目 id>
pole_pairs = 1
rated_current_ma = 590
# current_kp_q15 留空 = 用带宽推导（不是 0！见下节）

[libraries.profile.base]
vel_kp_ua_per_kcps = 600

[libraries.profile.hold]
extends = "base"                  # 仅 inheritable 库
max_torque_permille = 400

[nodes.bus]                       # 与 DocSchema::root 对应
cycle_us = 1000

[[nodes.axis]]                    # 实例集合
  [nodes.axis.identity]           # 实例内的 group 节点
  name = "s0"
  index = 0
  [nodes.axis.motor]              # slot 节点
  ref = "dm3510"
  [nodes.axis.motor.overrides]
  rated_current_ma = 540          # 仅本轴
  [[nodes.axis.profiles]]         # 库引用集合
  ref = "base"
```

保留键只有三个：槽位上的 `ref` / `overrides`，库条目上的 `extends`。其余
都是字段。**schema 不认识的键原样保留**——新版本写出的文档必须能被旧版本
读进来、改一改再存回去而不丢东西（`node.toml` 的 `archived_params` 是同一
个思路）。

完整可跑的例子见 `crates/sigflow-types/tests/fixtures/`：一份 schema、
一份双轴文档，`tests/param_doc.rs` 就是照着它们跑的。

---

## 取值解析：链与来源

```
实例覆盖  →  库条目自身  →  沿 extends 上溯  →  schema default  →  未设置
Override      Entry           Inherited          Default          Unset
```

`resolve_slot_field` 返回值和**来源**，`Override` 时还附带 `inherited`
（去掉覆盖会变回什么）——"恢复继承"这个动作在按下之前就得能说清后果。

展平只发生在下发前的最后一步。文档和 UI 保留未展平形态，因为展平恰好毁掉
操作员最需要的那条信息：这个 600 是本轴自己的决定，还是从 `base` 继承来的。

### 未设置不是 0

固件把"电流环 Kp 写 0"读作"用 R、L 和 PWM 周期推导"。这让第三态成为承重
结构：把未设置的字段渲染成 `0` 再保存，就写下了一个没有人做过的决定。因此
`ResolvedField::value` 是 `Option`，`Provenance::Unset` 与"值为零"永远分开。

---

## 校验

`validate()` 一次返回全部问题，按文档位置排序——在改配置的人要的是清单，
不是第一条错误。`Error` 拦下发，`Warn` 不拦。

字段级：类型、`range`、`recommended`（警告）、悬空引用、不允许的覆盖、
继承环、集合数量上下限。

跨字段的用 `Rule`，是一组**封闭的规则种类**而不是表达式语言：这个领域真实
存在的约束形状很有限（从邻件借来的上界、必须连续的序号），而表达式语言会
长成一门带自己的解析器、自己的错误信息和自己的 bug 的小语言。

| 种类 | 用途 |
|---|---|
| `le` / `ge` | 工况限速 ≤ 减速器最大输入转速；速度环限幅 ≤ 电机额定电流 |
| `between` | 三端都是路径（常数边界写在字段自己的 `range` 里） |
| `unique` | 轴名不重复 |
| `dense_index` | 从站序号必须是 `0..n`——设备按序号选组，缺号会选到别的 |
| `subset` | 引用必须落在允许集合内 |
| `member_of` | 默认工况必须是本轴真的下发了的那几组之一 |

路径以 `/` 分隔、相对 `scope`；`*` 段在集合上展开。`scope = "axis"` 的规则
对每个轴各求一次，所以 s0 的限值放宽不会连累 s1。

---

## 控件的三个区

```
┌ 树 ─────────────┬ 顶栏 ─────────────────────────────────────────────┐
│ ▾ 装置          │ 电机  [dm3510 ▾] ⚠被 4 处引用   [新建][另存为]    │
│ ▾ 轴            │ 编辑目标: (•)改型号库  ( )仅本轴覆盖               │
│   ▾ s0          │ 草稿 → 已保存 → 设备影子 → 已生效 → 已存盘        │
│      电机→dm3510├───────────────────────────────────────────────────┤
│      减速器     │ 表单 / 对比表                                      │
│      编码器     │  极对数      1        u32   [库 dm3510]            │
│      驱动板(铭牌)│  额定电流   540 mA          [本轴覆盖 ←590] ↺     │
│    ▾ 工况       │  电流环 Kp   —                [未设置 = 用推导]    │
│ ▾ 库            │  过流跳闸线 770 mA    🔒     保护不是调参          │
└─────────────────┴───────────────────────────────────────────────────┘
```

顶栏右侧是生命周期，不是一个保存按钮：

```
草稿 ──校验──▶ 已保存 ──下发──▶ 设备影子 ──提交──▶ 已生效 ──存盘──▶ 掉电保持
  ▲                                                        │
  └────────────────── 回读比对不符（已分歧）◀──────────────┘
```

`DocState` / `DocSyncStatus` 是契约类型而不是 UI 细节：这些状态不是编辑器
发明的，而是"带影子副本 + 显式提交"的设备本来就有的。在它上面画一个存盘
按钮，会盖掉这类系统最贵的那种故障——值写进去了、也被确认了、就是没生效。
`DocSyncStatus::targets` 按目标分别报状态，一个轴被拒不会塌缩成一盏红灯。

`fingerprint()` 覆盖文档里所有**生效值**，回答"自上次成功下发以来文档改过
没有"。设备自己那个 CRC 是按它的二进制布局算的，属于下发插件的职责，两者
不是一个东西。

---

## 现在能跑到哪一步

编辑闭环已通：读得出、改得动、存得回，且文件的注释不会掉。设备下发还没接。

| 环节 | 状态 |
|---|---|
| `DocSchema` / 解析 / 校验 / 指纹（Rust） | ✅ `sigflow-types` |
| `doc_get` RPC（schema + 文档 + findings + 指纹，一次往返） | ✅ shell |
| `doc_apply` RPC（编辑列表 → 指纹 CAS → 原子写回 → `doc_changed`） | ✅ shell |
| `doc_validate` RPC（草稿干跑，不写盘） | ✅ shell |
| `widget bind <alias> doc:<id>` | ✅ CLI + shell |
| `presentation = "page"` 整页宿主 | ✅ webui |
| 树 / 表单渲染 + 编辑，来源徽标、旁注、越界提示 | ✅ |
| 全文搜索（字段 / 轴 / 型号，跨整份文档） | ✅ |
| `guard` 生效（confirm 一次 / unlock 整轮） | ✅ |
| 集合增删（加一个轴、删一个工况槽位） | ✅ |
| 状态边车（`status_file`）→ `doc_get` 带回 `sync` / `readback` | ✅ |
| 页面：下发/存盘按钮、设备生命周期、按目标分状态 | ✅ |
| `cia402-param` 插件（展平 → SDO → 提交 → CRC 比对） | ⬜ 下一轮 |
| 对比表（行=字段，列=兄弟实例）+ 跨列复制 | ✅ 可编辑 |
| 前端/Rust 解析器对拍（共享 fixture） | ✅ |
| 下发生命周期接真设备（`DocSyncStatus`） | ⬜ 消费插件的活 |

### 改动怎么写回去

发的是**编辑列表**（`DocEdit`），不是改完的整份文档。三个理由都在这里成立：

- **注释活下来。** 这些文件是手写的，注释里是「600 稳、1200 起振」这类结论。
  把解析后的树重新序列化会把它们全抹掉；`toml_edit` 的定点修改不会。
  连「另存为新型号」复制过去的条目，键上方那行注释也跟着走。
- **未知字段永远不出壳体**，任何前端 bug 都丢不掉新版本 schema 加的字段。
- **文件里别处的并发改动不会被覆盖**——整份 PUT 天然做不到这点。

写入前用**指纹做 CAS**：文档的生效值在编辑器读过之后动过，就拒绝保存并让人
重读，而不是默默盖掉。一次 `doc_apply` 里的多条编辑按顺序**互相可见**——
「另存为新型号」是两条编辑一个动作（建条目 + 把槽位指过去），后一条必须看得
见前一条。

值层面的问题（越界、规则不满足）**只报不拦**：保存是把工作留住的动作，改到
一半存不下去才是丢工作的方式。拦的是结构上不可能的——路径不存在、字段不存在、
类型不对、往不接受覆盖的槽位写覆盖。错误挡的是**下发**，那是另一道闸。

### 新增的项按邻居的样子写

文件是手写的、缩进有讲究的。新加一项时它自己是空的，没有兄弟可抄——但**它旁边
那一项有**。所以规则是「新项照着既有项写」：表头缩进、键缩进、嵌套集合的缩进，
全部取自集合的第 0 项。

验收标准很硬：加一个轴再删掉，文件与原文件**逐字节相同**。

### 设备那半截：状态边车

「已保存」之后的三个状态不是编辑器算得出来的——它们是一台带影子副本、要显式
提交的设备**本来就有**的状态，只有跟它说话的那个插件知道。所以插件把结果写进
一个边车文件，`doc_get` 读回来：

```jsonc
// <文档同目录>/machine.status.json，由 [[documents]] 的 status_file 声明
{
  "doc_fingerprint": "490889f334210592",   // 描述的是文档的哪一版
  "at": "2026-08-19T16:20:00Z",
  "sync": {
    "state": "applied",
    "expected_checksum": "…", "device_checksum": "…",
    "device_origin": "本次会话已下发（0x2540:2 = 3）",
    "targets": {                            // 一个轴被拒不塌缩成一盏红灯
      "s0": { "state": "applied" },
      "s1": { "state": "diverged",
              "error": "0x06090030 字段越界：0x2500:4 速度环输出限幅 590 > 额定 540" }
    }
  },
  "readback": {                             // schema 里 readback = true 的字段
    "axis/0/status": { "config_state": "本次会话已下发", "active_profile": 1 }
  }
}
```

**为什么是文件而不是插件里的一个调用**：process 插件是独立进程，没有符号可以
dlsym，开一条 ABI 入口只能服务一半的插件；而且「这台机器到底在跑什么」这个
问题，值得在无头设备上能用 `cat` 读出来——编辑器早就关了它还在。

`doc_fingerprint` 与当前文档不符时 `doc_get` 报 `status_stale`，页面亮「已分歧」：
设备上跑的是这份文件的**另一个版本**，此时给个绿灯就是撒谎。

下发/存盘是插件的 **action**，按钮绑在控件 config 上（`download_action` /
`store_action`）——控件不假设它们叫什么。action 跨 ABI 没有返回值，所以结果同样
从边车回来：页面触发后轮询边车的 `at`，14 秒没动静就报出来而不是转圈等下去。

这是编辑器唯一替不了你做的决定，而且做错了不出声——四个轴悄悄共用了一个只该
给一个轴的值。所以它是一个**整轮可见的开关**（顶栏：`改 dm3510` / `仅本处`），
外加**单行临时改写**（每行一个 `→dm3510` / `→本处` 小按钮）：一次决定，配一个
不必推翻这次决定的例外口子。

机制上它只是「这条编辑写到哪个路径」：

```
改型号库  →  path = libraries/motor/dm3510    （所有引用它的轴一起变）
仅本处    →  path = axis/0/motor              （落进这一处的 overrides）
```

槽位的 `allow_override = false` 或 `nameplate = true` 时开关直接消失，只能改库
或者「另存为…」——复制当前条目为新型号并把这个槽位指过去，这才是「这一台不
一样」的正确答案。

### 挂上去

```sh
# 插件的 manifest.toml 里已有 [[documents]]，且 path_param 指向的参数已设
sigflow-cli --node ecat param set config_path /path/to/machine.toml
sigflow-cli plugin install plugins/ui/param-table
sigflow-cli --node ecat widget add sigflow.ui.param_table@0.1.0 params --layout 8,8,260,40
sigflow-cli --node ecat widget bind params doc:params
sigflow-cli layout ecat --display face
```

节点脸上出现一个入口，点开就是整页编辑器。

### 前端的那份解析器

`webui/src/lib/param-doc.ts` 是 `doc.rs` 的镜像——编辑器每敲一个键都要重算
生效值，不可能每次往返壳体。**有后果的事情一律以 Rust 为准**：findings 和
将来真正写出去的值都来自 `doc_get` / `doc_validate` / `doc_apply`。

一条规则实现两遍会无声地漂。两边都钉在同一份 fixture 上：
`webui/src/lib/param-doc.conformance.json` 覆盖全部六种来源，由
`param-doc.conformance.test.ts`（`npm test`）和 `config_doc.rs` 里的
`the_client_resolver_and_this_one_agree` 各读一遍。改了一边没跟上另一边，
两个测试里必有一个红。

### 跨列复制

对比表的列可编辑，且能整列复制（「把 s0 抄给 s1」）。复制**沿用顶栏那个编辑
目标开关**，不自己发明规则——包括因此变成空操作的情形（两列引用同一条库条目，
「改型号库」等于把同一个值写回原处）。它会如实报数：`3 处改动，2 处目标与来源
是同一个位置（切到「仅本处」才有意义），1 处受保护或只读，跳过`。

## 当前边界

- **单位换算只支持仿射**（`shown = stored × scale`）。counts↔rpm 需要编码器
  每转计数，是跨字段换算，这里刻意不支持，也不假装支持——先写进 `note`。
- **规则不做表达式**，只有上表那六种；不够用时加种类，不加语言。
- **文档不带历史**。命名快照/回滚是文件层的事，仓库里的文档天然由 git 管。
- **受保护字段在对比表里不可编辑**。`guard` 的确认步骤在表单里；一张全是格子的
  表不是慎重改一个跳闸线的地方。
- **下发链路本身还没有实现**。契约和页面这一半通了，真正把值送上 EtherCAT 的
  插件是下一轮。
- **schema 里没有下发顺序**。哪个字段走哪个 SDO、先写谁后写谁、提交怎么做，
  全部属于消费文档的那个插件；schema 只说"值是什么、值合不合法"。
- **相对路径按壳体进程的工作目录解析**（与 `storage.data_root` 一致）。部署
  脚本应当写绝对路径——`redeploy.sh` 会 `rm -rf` 节点目录，文档不该住在那里。

---

## 插件怎么接

```toml
# 插件的 manifest.toml
[[documents]]
id = "params"
schema_file = "servo-axis.schema.toml"   # 随包发布，相对包根
path_param = "config_path"               # 文档路径由这个参数给出
label = "伺服轴参数"

[[parameters]]
id = "config_path"
param_type = "string"
default = { string = "" }                # 空 = 还没有文档，合法状态
label = "参数文档路径"
```

schema 单独成文件而不是内联进 manifest：一份真实机器 schema 好几百行，写成
嵌套的 TOML 数组表既读不动也评审不动。

控件绑上去就是 `widget bind` 到 `doc:params`。
