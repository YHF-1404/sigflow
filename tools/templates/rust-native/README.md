# __PLUGIN_NAME__

sigflow native 插件（cdylib，壳体 dlopen 装载）。

```sh
./build.sh                                  # cargo build --release + stage 到 pkg/
sigflow-cli plugin install pkg              # 入插件仓库（~/.sigflow/plugins）
sigflow-cli --node <节点> plugin set __PLUGIN_ID__@0.1.0 -y
```

- 契约（端口/参数/动作）改 `manifest.toml`，处理逻辑改 `src/lib.rs`。
- SDK 文档与更多参考实现：https://github.com/YHF-1404/sigflow
