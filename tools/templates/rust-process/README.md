# __PLUGIN_NAME__

sigflow 子进程（Process runtime）插件——独立二进制，SHM 数据面 + JSON 控制通道。

```sh
./build.sh                                  # cargo build --release + stage 到 pkg/
sigflow-cli plugin install pkg
sigflow-cli --node <节点> plugin set __PLUGIN_ID__@0.1.0 -y
```
