#!/bin/sh
# starter.sh — __PROJECT_NAME__ 起步图例：正弦源 → __PLUGIN_NAME__ → 波形监视。
# requires-native: sine-generator __PLUGIN_NAME__ data-monitor
#
# 生成自 new-project.sh：验证工程接线用，把 proc 节点换成你的真实拓扑。
# 前提：sigflow-cli 在 PATH、requires 里的插件已入仓库（./redeploy.sh
# 会自动完成这些）。POSIX sh；参数走环境变量。
set -eu

NODE_DIR="${NODE_DIR:-/tmp/sigflow-__PROJECT_NAME__}"
VOFA_HOST="${VOFA_HOST:-127.0.0.1}"
RAW_PORT="${RAW_PORT:-1347}"
SINE_HZ="${SINE_HZ:-440}"
export SIGFLOW_ROOT_PORT="${SIGFLOW_ROOT_PORT:-9500}"
export SIGFLOW_BIND_HOST="${SIGFLOW_BIND_HOST:-0.0.0.0}"

step() { printf '\n\033[1;36m=== %s\033[0m\n' "$*"; }

step "重建 $NODE_DIR 并搭图"
rm -rf "$NODE_DIR"
sigflow-cli start "$NODE_DIR"

sigflow-cli add src      # 正弦信号源
sigflow-cli add proc     # 你的插件
sigflow-cli add mon      # 监视：VOFA UDP + 编辑器波形脸

sigflow-cli --node src  plugin set sigflow.core.sine_generator@0.1.0 -y
sigflow-cli --node proc plugin set __PLUGIN_ID__@0.1.0               -y
sigflow-cli --node mon  plugin set sigflow.core.data_monitor@0.1.0   -y

step "连线"
sigflow-cli connect src  signal_out proc in
sigflow-cli connect proc out        mon  signal_in
sigflow-cli connection-list

step "参数与脸"
sigflow-cli --node src param set frequency_hz "$SINE_HZ"
sigflow-cli --node mon param set vofa_enabled true
sigflow-cli --node mon param set vofa_addr "$VOFA_HOST:$RAW_PORT"
if sigflow-cli --node mon widget add sigflow.ui.waveform_chart@0.1.0 wave \
    --layout 8,8,384,244 --config "labels=sig" 2>/dev/null; then
    sigflow-cli --node mon widget bind wave port:signal_in
    sigflow-cli layout mon --pos 340,80 --size 400,320 --display face
fi

step "processing start（消费者先于源）"
for n in mon proc src; do
    sigflow-cli --node "$n" processing start
done

printf '\n\033[1;32m__PROJECT_NAME__ starter 图就绪。\033[0m\n'
printf '  信号流(1ch) → VOFA+ UDP JustFloat  %s:%s\n' "$VOFA_HOST" "$RAW_PORT"
printf '  节点编辑器  → http://<本机IP>:%s/（webui 内嵌在壳体）\n' "$SIGFLOW_ROOT_PORT"
printf '  收摊        → sigflow-cli stop\n'
