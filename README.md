# sigflow

sigflow 信号流图框架的公开面：插件 SDK、共享契约 crate、通用插件与入门图例。
Plugin SDKs, shared contract crates, generic plugins and examples for the
sigflow dataflow framework.

sigflow 把信号处理组织成一张节点图：每个节点挂一个插件（native cdylib 或
子进程），节点间经零拷贝数据面连线，控制面走 WebSocket RPC + 节点编辑器。
核心（壳体/数据面/CLI）以预编译包分发，见 Releases；本仓库是你读代码、
学写插件、跑图例的地方。

## 布局

| 目录 | 内容 |
|---|---|
| `crates/sigflow-types` | 共享契约：manifest / 帧头 / 参数 / RPC / UI 类型（ts-rs 可导 TS） |
| `crates/sigflow-catalog` | 落盘数据格式：sidecar 真相 + 可重建索引 |
| `sdk/rust` | Rust 插件 SDK（`sigflow-plugin-sdk`，native + process 双运行时） |
| `sdk/python` | Python 插件 SDK（`sigflow_sdk`，process 运行时） |
| `plugins/native` | 通用 native 插件 ×15（passthrough / sine-generator / data-monitor / storage / trigger-capture / …） |
| `plugins/process` | 子进程插件参考实现（rust-counter / usb-adc） |
| `plugins/ui` | UI 控件 manifest ×10（button / slider / heatmap / waveform-chart / …） |
| `example/` | 图例脚本（`hello.sh` 无硬件即跑） |
| `deploy/` | 部署引擎 `redeploy.sh`（本地/ssh/adb 三路，项目仓库以薄 wrapper 接入）+ Android 自启资产 |
| `docs/` | 专题文档（触发捕获端到端、VOFA 波形） |

## 快速开始

```sh
git clone https://github.com/YHF-1404/sigflow.git && cd sigflow
cargo build            # 全部 crate + 插件（独立 workspace，无外部依赖）

# 装核心（sigflow-shell/cli 预编译包，平台自动探测）：
curl -fsSL https://raw.githubusercontent.com/YHF-1404/sigflow/main/install.sh | sh

# 构建并安装 hello 图例所需插件：
for p in sine-generator passthrough data-monitor; do
    ( cd plugins/native/$p && ./build.sh )
    sigflow-cli plugin install plugins/native/$p/pkg
done
sigflow-cli plugin install plugins/ui/waveform-chart   # mon 的波形脸控件
sh example/hello.sh    # sine → gain → mon，VOFA UDP + 编辑器波形脸
```

> Windows：以上 `build.sh` / 脚手架脚本在 Git Bash（或 MSYS2）里执行即可，
> 产物自动切换为 `.dll` / `.exe` / `run.cmd`；`install.sh` 暂无 Windows
> 预编译包，核心请从源码构建。

## 起一个项目

业务插件多了就建项目仓库（插件集 + 图例 + 部署入口，兄弟 checkout 布局）：

```sh
tools/new-project.sh --plugin my-filter ../my-project
cd ../my-project && cargo build && ./redeploy.sh starter
```

## 写一个插件

用脚手架生成空白工程（Rust native / Rust process / Python process 三种形态）：

```sh
tools/new-plugin.sh --lang rust --runtime native --name my-filter ~/dev/my-filter
cd ~/dev/my-filter && ./build.sh && sigflow-cli plugin install pkg
```

SDK 依赖默认钉住本仓库当前 tag 的 git 依赖，开箱即编译；对着克隆仓库
开发用 `--path-sdk`。每个插件是一个自包含目录：

```
my-plugin/
├── Cargo.toml       # 依赖 sigflow-plugin-sdk（git 依赖本仓库）
├── manifest.toml    # 身份(name@version)、端口、参数、动作、UI 声明
├── build.sh         # cargo build --release + staged 到 pkg/
└── src/lib.rs       # Plugin trait 实现（native）或 main.rs（process）
```

从 `plugins/native/passthrough`（native 最小参考）与
`plugins/process/rust-counter`（process 最小参考）读起；Python 插件见
`sdk/python/examples/`。构建后 `sigflow-cli plugin install <dir>/pkg`
入仓库，`sigflow-cli --node X plugin set <name>@<ver>` 挂上节点。

## License

MIT，见 [LICENSE](LICENSE)。
