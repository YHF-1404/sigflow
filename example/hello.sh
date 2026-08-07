#!/bin/sh
# example/hello.sh — 无硬件入门图例：正弦源 → 直通增益 → 波形监视。
# requires-native: sine-generator passthrough data-monitor
#
# 三个节点、两条边，任何机器可跑：sine 产 440Hz 正弦流，gain 直通
# （passthrough 插件，带 gain 参数），mon 把流推去 VOFA+（UDP JustFloat）
# 并在节点编辑器里装了波形脸。用来验证安装、学习建图动词。
#
# 前提：sigflow-cli 在 PATH、上面三个插件已 `sigflow-cli plugin install`。
# POSIX sh；全部参数走环境变量：
#   NODE_DIR   节点目录            （默认 /tmp/sigflow-hello）
#   VOFA_HOST RAW_PORT  VOFA+ UDP  （默认 127.0.0.1:1347）
#   SINE_HZ    正弦频率            （默认 440）
set -eu

NODE_DIR="${NODE_DIR:-/tmp/sigflow-hello}"
VOFA_HOST="${VOFA_HOST:-127.0.0.1}"
RAW_PORT="${RAW_PORT:-1347}"
SINE_HZ="${SINE_HZ:-440}"
export SIGFLOW_ROOT_PORT="${SIGFLOW_ROOT_PORT:-9500}"
export SIGFLOW_BIND_HOST="${SIGFLOW_BIND_HOST:-0.0.0.0}"

step() { printf '\n\033[1;36m=== %s\033[0m\n' "$*"; }

step "重建 $NODE_DIR 并搭图"
rm -rf "$NODE_DIR"
sigflow-cli start "$NODE_DIR"

sigflow-cli add sine     # 正弦信号源
sigflow-cli add gain     # 直通/增益
sigflow-cli add mon      # 监视：VOFA UDP + 编辑器波形脸

sigflow-cli --node sine plugin set sigflow.core.sine_generator@0.1.0 -y
sigflow-cli --node gain plugin set sigflow.core.passthrough@0.1.0    -y
sigflow-cli --node mon  plugin set sigflow.core.data_monitor@0.1.0   -y

step "连线"
sigflow-cli connect sine signal_out gain in
sigflow-cli connect gain out        mon  signal_in
sigflow-cli connection-list

step "参数与脸"
sigflow-cli --node sine param set frequency_hz "$SINE_HZ"
sigflow-cli --node gain param set gain 1.0
sigflow-cli --node mon  param set vofa_enabled true
sigflow-cli --node mon  param set vofa_addr "$VOFA_HOST:$RAW_PORT"
# 波形脸（可选）：需要已安装 sigflow.ui.waveform_chart（plugins/ui/waveform-chart）
if sigflow-cli --node mon widget add sigflow.ui.waveform_chart@0.1.0 wave \
    --layout 8,8,384,244 --config "labels=sine" 2>/dev/null; then
    sigflow-cli --node mon widget bind wave port:signal_in
    sigflow-cli layout mon --pos 340,80 --size 400,320 --display face
else
    printf '    （未装 sigflow.ui.waveform_chart——跳过波形脸；编辑器里仍可开 tap 看流）\n'
fi

step "processing start（消费者先于源）"
for n in mon gain sine; do
    sigflow-cli --node "$n" processing start
done

printf '\n\033[1;32mhello 图就绪。\033[0m\n'
printf '  正弦流(1ch) → VOFA+ UDP JustFloat  %s:%s\n' "$VOFA_HOST" "$RAW_PORT"
printf '  节点编辑器  → ws://<本机IP>:%s/ws（mon 脸上看波形）\n' "$SIGFLOW_ROOT_PORT"
printf '  节点日志    → tail -f %s/children/mon/node.log\n' "$NODE_DIR"
printf '  收摊        → sigflow-cli stop\n'
