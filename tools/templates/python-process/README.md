# __PLUGIN_NAME__

sigflow Python 子进程插件——run.sh 包装拉起，SHM 数据面 + JSON 控制通道。

```sh
./build.sh                                  # 依赖检查 + vendor SDK + stage 到 pkg/
sigflow-cli plugin install pkg
sigflow-cli --node <节点> plugin set __PLUGIN_ID__@0.1.0 -y
```

- 契约改 `manifest.toml`，逻辑改 `plugin.py`；需要 numpy 等依赖时在
  build.sh 的检查行照样子加一条。
- `sigflow_sdk/` 是生成工程时 vendor 进来的副本；要更新就从
  https://github.com/YHF-1404/sigflow 的 sdk/python 重新拷贝，或
  `SIGFLOW_PY_SDK=` 指向一个 checkout。
