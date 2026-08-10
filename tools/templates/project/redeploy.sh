#!/usr/bin/env bash
# redeploy.sh — __PROJECT_NAME__ 部署入口：项目参数默认值 + 转发清单，
# 机制在公共引擎（../sigflow/deploy/redeploy.sh）。
#   ./redeploy.sh <graph>                 # 本地部署
#   ./redeploy.sh <graph> --host H --adb  # Android 设备
# 项目私货钩子放 deploy/hooks/（precheck.sh / adb-pre-install.sh，见
# 引擎头注释）。核心源码 checkout（../sigflow-core）由引擎自动探测；
# 没有时自动降级为 PATH 已装核心或 install.sh。
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
export SIGFLOW_PROJECT_DIR="$HERE"
export SIGFLOW_PUBLIC_DIR="${SIGFLOW_PUBLIC_DIR:-$(cd "$HERE/.." && pwd)/sigflow}"

# ---- 项目参数默认值（图例 env）---------------------------------------------
export NODE_DIR="${NODE_DIR:-/tmp/sigflow-__PROJECT_NAME__}"
export GRAPH_NAME="${GRAPH_NAME:-__PROJECT_NAME__}"
# 在这里加你的项目参数。注意：图例脚本自带 ${VAR:-默认} 的变量别在此再
# 兜默认——会盖死图例自己的默认值；只列进下面的转发清单即可。

# 远程/adb 模式转发到目标机的项目 env 名单（只转发已设非空者）
export SIGFLOW_FORWARD_VARS=""

exec "$SIGFLOW_PUBLIC_DIR/deploy/redeploy.sh" "$@"
