#!/system/bin/sh
# sigflow 开机自启守护——init service "sigflow" 的本体（见同目录 sigflow.rc）。
# 职责：重建易失运行环境 → 建图 → 常驻盯根壳体，9500 监听消失即整树重建。
# mksh/toybox 兼容；日志落 /data/local/tmp/sigflow-boot.log（/tmp 此时可能未挂）。
#
# 与 redeploy.sh 的互斥：部署开始时设备上 touch $PAUSE，本脚本 10 分钟内
# 不抢跑（部署自己会杀壳体再建图；无互斥会在停机窗口里撞车双建）。
# 部署成功后 redeploy 删 $PAUSE；部署夭折则 pause 过期自动恢复守护。
#
# 项目私货一律走 $HOOK（redeploy 从项目 deploy/hooks/boot-hook.sh 推来），
# 本脚本只留与项目无关的骨架——契约见下方"项目开机钩子"段。
#
# 已知边界：只监督"根壳体死"（整树重建）。子节点被 OOM SIGKILL 而根还活着
# 的情形（cap 事件）不在本层——那是壳体内部监督的独立课题。

LOG=/data/local/tmp/sigflow-boot.log
PAUSE=/data/local/tmp/sigflow-boot.pause
GRAPH=/data/local/tmp/graph.sh
ENVFILE=/data/local/tmp/graph.env   # redeploy 落的部署期参数覆盖（export V='..' 行）
HOOK=/data/local/tmp/sigflow-boot-hook.sh   # 项目开机钩子（可缺省）
PORT_HEX=251C                       # 9500 = 根壳体 RPC/ws 端口

exec >> "$LOG" 2>&1

export PATH=/data/local/tmp/sigflow/bin:$PATH
export HOME=/data/local/tmp/sigflow

echo "[boot] up $(date)"

# 部署期参数（图例 env 覆盖）——rebuild 里也会再 source 一次
[ -f "$ENVFILE" ] && . "$ENVFILE"

# ---- 项目开机钩子 ----
# 契约：钩子被 source 一次（顶层语句 = 一次性开机动作，如等设备节点就绪、
# 关 USB 电源管理），可选覆盖下面两个函数：
#   project_tick()      守护启动时与此后每 $TICK_S 秒各调一次——用于"某进程
#                       一冒头就摁下去"这类需要常驻盯的驯服动作
#   project_pre_graph() 每次 rebuild 建图前调一次——用于清场抢设备的残留进程
# 钩子里不要 exit（会带走守护）；出错不阻断（本脚本无 set -e）。
project_tick() { :; }
project_pre_graph() { :; }
TICK_S=5
if [ -f "$HOOK" ]; then
    echo "[boot] hook: $HOOK"
    . "$HOOK"
fi
project_tick

# ---- 易失环境（每次开机重建；与 redeploy.sh 设备段保持一致）----
# iceoryx2 数据面要 /dev/shm；/tmp 供图与日志。项目相关的易失设置（USB
# 电源管理之类）归钩子。
mkdir -p /dev/shm 2>/dev/null
mountpoint -q /dev/shm || mount -t tmpfs -o size=256m tmpfs /dev/shm
mountpoint -q /tmp 2>/dev/null || mount -t tmpfs tmpfs /tmp 2>/dev/null || true

alive() {  # 根壳体 RPC 端口在监听（本地口 :9500、远端全零、状态 0A=LISTEN）
    grep -qE ":$PORT_HEX 0+:0000 0A " /proc/net/tcp /proc/net/tcp6 2>/dev/null
}

paused() {  # 部署互斥窗；陈旧 pause（>10min）视为部署夭折，忽略
    [ -f "$PAUSE" ] || return 1
    now=$(date +%s)
    mt=$(stat -c %Y "$PAUSE" 2>/dev/null || echo 0)
    [ $((now - mt)) -lt 600 ]
}

rebuild() {
    echo "[boot] rebuild $(date)"
    pkill -9 sigflow-shell 2>/dev/null
    project_pre_graph
    sleep 1
    # 注册表随 $HOME 持久，重启后 PID 复用会让陈旧条目装活（--node 解析
    # 连死端口，真机踩过）——壳全杀光了，条目必然全是垃圾，直接清
    rm -rf /tmp/iceoryx2 /dev/shm/iox2_* "$HOME/.sigflow/registry"
    [ -f "$ENVFILE" ] && . "$ENVFILE"
    # 与部署路径同一套：android 不装 python 插件，引用 python 节点的图例
    # 走降级骨架
    if SIGFLOW_SKIP_PY=1 SIGFLOW_NICE=-10 sh "$GRAPH" > /tmp/sigflow-graph.log 2>&1 < /dev/null; then
        echo "[boot] graph up"
    else
        echo "[boot] graph.sh rc=$?（详见 /tmp/sigflow-graph.log），30s 后重试"
        # 半建的树会留着 9500 监听，让 alive() 误判活着、重试永不生效
        # （开机负载下子节点 ws 慢就绪建图失败，真机踩过）——清场再等
        pkill -9 sigflow-shell 2>/dev/null
        sleep 30
    fi
}

while true; do
    if ! alive && ! paused; then
        rebuild
    fi
    project_tick
    sleep "$TICK_S"
done
