#!/system/bin/sh
# sigflow 开机自启守护——init service "sigflow" 的本体（见同目录 sigflow.rc）。
# 职责：重建易失运行环境 → 建图 → 常驻盯根壳体，9500 监听消失即整树重建。
# mksh/toybox 兼容；日志落 /data/local/tmp/sigflow-boot.log（/tmp 此时可能未挂）。
#
# 与 redeploy.sh 的互斥：部署开始时设备上 touch $PAUSE，本脚本 10 分钟内
# 不抢跑（部署自己会杀壳体再建图；无互斥会在停机窗口里撞车双建）。
# 部署成功后 redeploy 删 $PAUSE；部署夭折则 pause 过期自动恢复守护。
#
# 已知边界：只监督"根壳体死"（整树重建）。子节点被 OOM SIGKILL 而根还活着
# 的情形（cap 事件）不在本层——那是壳体内部监督的独立课题。

LOG=/data/local/tmp/sigflow-boot.log
PAUSE=/data/local/tmp/sigflow-boot.pause
GRAPH=/data/local/tmp/graph.sh
ENVFILE=/data/local/tmp/graph.env   # redeploy 落的部署期参数覆盖（export V='..' 行）
PORT_HEX=251C                       # 9500 = 根壳体 RPC/ws 端口

exec >> "$LOG" 2>&1

export PATH=/data/local/tmp/sigflow/bin:$PATH
export HOME=/data/local/tmp/sigflow

echo "[boot] up $(date)"

# 部署期参数（含 THS_TAKEOVER）——rebuild 里也会再 source 一次
[ -f "$ENVFILE" ] && . "$ENVFILE"

# ---- ths 数据面接管驯服（THS_TAKEOVER=true 时生效）----
# cvte 系 HAL 客户端（udi 的 hal_SPM、libcvtetouchcomm）在"连过 ths 后
# ths 消失"时 read_thread 无退避紧循环刷 E 日志（实测 21k 行/s），几分钟
# 灌爆 logd 拖死整机；而 ths 缺席时新生的 udi 惰性不初始化 SPM HAL=无害。
# 故接管模式下：ths 一冒头（开机 vendor rc 自启/人为 start）就停掉，并
# kill 一次 udi 让 AMS 重生成惰性态。回退老路 = THS_TAKEOVER=false 重部署。
tame_ths() {
    [ "$THS_TAKEOVER" = "true" ] || return 0
    ps -A | grep touchhandleserver | grep -qv grep || return 0
    echo "[boot] takeover: stop ths + purge udi $(date)"
    stop touchhandleserver
    upid=$(ps -A | grep 'udi.service.core' | grep -v pc | grep -v grep | awk '{print $2}')
    [ -n "$upid" ] && kill $upid 2>/dev/null
}
tame_ths

# ---- 易失环境（每次开机重建；与 redeploy.sh 设备段保持一致）----
mkdir -p /dev/shm 2>/dev/null
mountpoint -q /dev/shm || mount -t tmpfs -o size=256m tmpfs /dev/shm
mountpoint -q /tmp 2>/dev/null || mount -t tmpfs tmpfs /tmp 2>/dev/null || true
# 关 USB 运行时电源管理（hub 链 auto 挂起会卡下游 FS 流量数百 ms）
for pc in /sys/bus/usb/devices/*/power/control; do
    echo on > "$pc" 2>/dev/null || true
done

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
    # 清场残留 tcp_bridge（独占 /dev/aitouch_raw 会把 touch_source 饿死）
    pkill tcp_bridge 2>/dev/null
    sleep 1
    # 注册表随 $HOME 持久，重启后 PID 复用会让陈旧条目装活（--node 解析
    # 连死端口，真机踩过）——壳全杀光了，条目必然全是垃圾，直接清
    rm -rf /tmp/iceoryx2 /dev/shm/iox2_* "$HOME/.sigflow/registry"
    [ -f "$ENVFILE" ] && . "$ENVFILE"
    if SDC_SKIP_PY=1 SIGFLOW_NICE=-10 sh "$GRAPH" > /tmp/sigflow-graph.log 2>&1 < /dev/null; then
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
    tame_ths
    sleep 5
done
