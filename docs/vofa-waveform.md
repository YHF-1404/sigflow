# 实时波形查看（usb_adc → data_monitor → VOFA+）

把 USB ADC 的采集流接到 `data_monitor`，由它经 **UDP / VOFA+ JustFloat** 协议
把样本推给 [VOFA+](https://www.vofa.plus/)，即可看到实时波形。最小图只有两个
节点：

```
usb_adc.adc_out ──→ data_monitor.signal_in ──(UDP JustFloat)──→ VOFA+
```

> 硬件在 archlinux 机器上（USB 设备 + udev 规则）。下文命令在那台机器执行。
>
> **一键部署**：本仓库根目录有 `./redeploy-arch.sh`，自动完成「停止/卸载旧版 →
> 清插件 → 编译打包 → pacman 安装 → 建图 → 指派 → 连线 → 配 VOFA（原始流 + 触发
> 捕获两路）」。手动分步见下文。

---

## CLI 约定（同 trigger-capture-e2e.md）

- 可执行文件是 **`sigflow-cli`**，下文命令一律以 `sigflow-cli` 开头。
- **全局 `--node <ROUTE>`** 指定作用节点（如 `--node adc`）。注意是长格式 `--node`。
- 拓扑命令（`add`/`start`/`ls`）的节点作位置参数；关系命令（`connect`）的端口对
  作位置参数，发给当前会话节点（根容器）。

---

## 1. 编译 + 安装插件

```fish
cargo build -p sigflow-cli -p sigflow-shell
(cd plugins/process/usb-adc;        and ./build.sh)  # process 插件
(cd plugins/native/data-monitor;    and ./build.sh)  # native 插件（含 VOFA 支持）
(cd plugins/native/trigger-capture; and ./build.sh)  # native 插件（触发捕获）
sigflow-cli plugin install plugins/process/usb-adc/pkg
sigflow-cli plugin install plugins/native/data-monitor/pkg
sigflow-cli plugin install plugins/native/trigger-capture/pkg
sigflow-cli plugin list                              # 确认三个插件都已注册
```

## 2. 起根节点 + 子节点

```fish
sigflow-cli start /tmp/sigflow-vofa  # 起根节点并 attach（路径不存在会新建）
sigflow-cli add adc
sigflow-cli add mon
sigflow-cli ls
```

## 3. 指派插件

```fish
sigflow-cli --node adc plugin set sigflow.io.usb_adc@0.1.0        -y
sigflow-cli --node mon plugin set sigflow.core.data_monitor@0.1.0 -y
```

## 4. 连线

```fish
sigflow-cli connect adc adc_out mon signal_in
sigflow-cli connection-list              # 确认一条
```

## 5. 配置 VOFA 输出

```fish
sigflow-cli --node mon param set vofa_enabled true
# 默认发到 127.0.0.1:1347。VOFA 跑在别的机器就改成那台的 IP：
# sigflow-cli --node mon param set vofa_addr 192.168.1.50:1347
# 通道数默认自动从帧头推断；要写死（5 通道）可：
# sigflow-cli --node mon param set channels 5
```

## 6. VOFA+ 侧设置

1. 打开 VOFA+ → 选 **UDP** 连接方式。
2. 数据格式选 **JustFloat**。
3. 端口填 `1347`（与 `vofa_addr` 的端口一致），让 VOFA **监听**该 UDP 端口。
4. sigflow 作为发送方把数据打过去——确保两机网络互通、防火墙放行 **UDP 1347**。
   （同机回环最简单：VOFA 与 sigflow 都在本机，地址用默认 `127.0.0.1:1347`。）

## 7. 启动

```fish
for n in adc mon
    sigflow-cli --node $n processing start
end
```

设备约 0.5–0.75 s 后开始出数据，VOFA+ 里随即出现 5 条通道波形。

---

## 加上触发捕获（可选）

只想“看连续波形”到第 7 步就够了。要做“笔敲击 → 抓对应窗口”，再加一个
`trigger_capture` 节点，把笔的触发口接进去（笔的物理触发已接设备 PB4/PB5，
`usb_adc` 自带 `trigger_pen0/pen1` 两路 marker 输出，详见
`trigger-capture-e2e.md` 的真机版）：

```fish
sigflow-cli add cap
sigflow-cli --node cap plugin set sigflow.io.trigger_capture@0.1.0 -y
sigflow-cli connect adc adc_out      cap signal      # 被捕获的信号
sigflow-cli connect adc trigger_pen0 cap trigger     # 笔 0 的触发 marker
# 捕获结果也送给另一个开了 VOFA 的 monitor 看（用另一个 VOFA 端口/实例）：
sigflow-cli add capmon
sigflow-cli --node capmon plugin set sigflow.core.data_monitor@0.1.0 -y
sigflow-cli --node capmon param set vofa_enabled true
sigflow-cli --node capmon param set vofa_addr 127.0.0.1:1348
sigflow-cli connect cap capture capmon signal_in
```

---

## 排查

- **VOFA 没波形**：
  - 确认 `vofa_enabled=true` 且 `adc`/`mon` 都 `processing start`
    （`sigflow-cli --node mon status` 看 `State: running`）。
  - 确认 VOFA 是 **UDP + JustFloat**、监听端口与 `vofa_addr` 一致。
  - 跨机：防火墙放行 UDP，`vofa_addr` 填 VOFA 所在机 IP（不是 127.0.0.1）。
  - 看 `mon` 节点日志 `<root>/children/mon/node.log`：绑定/解析/发送出错会打印一次。
- **波形通道数不对/串道**：通道数自动推断依赖帧头 `n_samples`；异常时显式
  `sigflow-cli --node mon param set channels <N>` 写死（N = usb_adc 的通道数，默认 5）。
- **波形卡顿/丢样**：UDP 非阻塞，套接字缓冲满会丢样本（不会卡处理）。同机回环
  一般无碍；跨机走千兆且别让链路拥塞。
- **数值像电压但量程不对**：`usb_adc` 的 `vref` 参数决定 raw→电压换算，按硬件设。
- **连线 stop/start 后消失**：已修（连线现在自动落盘、重启自动重建运行时连线）。
  若仍异常，确认跑的是新版 `sigflow-shell`（`./redeploy-arch.sh` 会装新版）。
- **`create_publisher ... IsMarkedForDestruction`**：上次运行被强杀，iceoryx2 的
  service 卡在“待销毁”状态，挡住了同名 service 重建。停掉所有 sigflow 后清残留：
  `pkill -9 -f sigflow-shell; rm -rf /tmp/iceoryx2 /dev/shm/iox2_*`，再重跑。
  `./redeploy-arch.sh` 第 1 步已自动做这件事。

---

## 涉及的提交

| 提交 | 内容 |
|---|---|
| `84c3656` | data-monitor VOFA+ UDP（JustFloat）实时波形输出 |
| `b2ca531` | usb-adc 笔 IO 触发 host 侧（trigger_pen0/pen1 marker） |
| `80322c3` | 连线持久化 + 重启后自动重建运行时连线 |
