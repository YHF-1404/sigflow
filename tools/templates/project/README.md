# __PROJECT_NAME__

sigflow 项目：业务插件 + 图例 + 部署入口。由
[sigflow](https://github.com/YHF-1404/sigflow) 的 `tools/new-project.sh`
生成。

## 兄弟目录约定

对 sigflow 公共面（SDK / types）的依赖是 git 依赖 + 根 Cargo.toml
`[patch]` 重定向到兄弟 checkout——本地开发不需要先 push：

```
<workdir>/
├── sigflow/           # 公共面 checkout（github: YHF-1404/sigflow）
├── sigflow-core/      # （可选）核心源码——没有时部署自动用已装核心
└── __PROJECT_NAME__/  # 本仓库
```

## 用法

```sh
cargo build                                  # 全部插件
../sigflow/tools/new-plugin.sh \
    --lang rust --name my-node plugins/native/my-node   # 加插件后记得
                                             # 进 Cargo.toml members +
                                             # SDK 依赖改 branch="main"
./redeploy.sh <graph>                        # 部署 example/ 里的图例
```
