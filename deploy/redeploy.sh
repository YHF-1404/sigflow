#!/usr/bin/env bash
# deploy/redeploy.sh — sigflow 节点图一键部署引擎（多平台，项目无关）。
#
# 通常不直接调用：项目仓库放一个薄 wrapper（设参数默认值 + 转发清单后
# exec 本脚本），见 dualpen/redeploy.sh 参考实现。直接调用也行——公共
# 仓库自带的图例（example/hello.sh）无需项目上下文即可部署。
#
# 用法（wrapper 与直接调用同款）：
#   ./redeploy.sh <graph>                      # 本地部署（按本机平台）
#   ./redeploy.sh <graph> --host H --passwd P  # ssh 远程部署（默认 root）
#   ./redeploy.sh <graph> --host H --passwd P --user U   # 普通用户+sudo
#   ./redeploy.sh <graph> --host H --adb       # Android 设备（adb 网络连接）
#
# Windows（Git Bash/MSYS2）：支持本地部署（核心装到 ~/sigflow，插件
# build.sh 出 .dll/.exe/run.cmd）；也支持作为 --host 远程**目标**——
# 本机 mingw 交叉编译核心+插件，远端经 Git for Windows 的 bash 安装
# 起流（目标机前提：OpenSSH 服务 + Git for Windows 标准路径安装，
# SIGFLOW_REMOTE_BASH 可指定非标准 bash.exe 路径）。--host/--adb 仍
# 不支持从 Windows 主机**发起**（需要 cross/docker 等 Linux 侧工具链）。
#
# <graph> 在项目 example/ 与公共 example/ 里按名字解析（路径亦可）。
# 图例头部自我声明所需插件（缺省 = 两侧全部有 build.sh 的插件）：
#   # requires-native: trigger-capture data-monitor ...
#   # requires-process: my-py-node
#
# 环境接口（wrapper 负责导出）：
#   SIGFLOW_PROJECT_DIR   项目仓库根（plugins/ example/ deploy/hooks/）；
#                         缺省无——只能部署公共图例
#   SIGFLOW_CORE_DIR      核心源码 checkout；有=从源码打包安装（开发流），
#                         无=用 PATH 里已装核心，再无=本地走 install.sh
#   SIGFLOW_PUBLIC_DIR    公共面 checkout；缺省 = 本脚本所在仓库
#   SIGFLOW_FORWARD_VARS  远程/adb 模式额外转发给图例的 env 名单（空格分隔；
#                         只转发已设非空者——图例自带默认值的变量别在
#                         wrapper 里兜默认，会盖死图例的默认）
#
# 项目钩子（$SIGFLOW_PROJECT_DIR/deploy/hooks/，都可缺省）：
#   precheck.sh        装完核心后在部署目标机上 source（本地与 --phase
#                      remote 两路）；引擎函数 note/die/priv 可用，
#                      $HOOK_OS = arch|debian|macos|windows|linux；自行按
#                      NATIVE_PLUGINS/PROCESS_PLUGINS（本地）或装船目录
#                      （远端）决定要不要干活
#   adb-pre-install.sh 独立 sh，推到 Android 设备上、装包前执行（清场
#                      项目残留进程之类）
#   boot-hook.sh       推到 Android 设备常驻（/data/local/tmp/
#                      sigflow-boot-hook.sh），开机守护 sigflow-boot.sh
#                      每次启动 source 一次：顶层语句 = 一次性开机动作，
#                      可选覆盖 project_tick()（守护每 5s 调，用于常驻
#                      驯服抢设备的进程）与 project_pre_graph()（每次
#                      rebuild 建图前调）。项目私货全落这里，守护脚本
#                      本身保持项目无关
#
# Android 模式：packaging/android.sh 出 bionic tarball（bin + native 插件
# + UI manifest）→ adb push → 设备装插件 → 跑图。python process 插件在
# Android 上恒跳过（SIGFLOW_SKIP_PY=1，图例自行降级；设备通常无 python3）。
# 开机自启：deploy/android/{sigflow.rc,sigflow-boot.sh} 推入 /system。
#
# ssh 远程模式：探测远端架构/发行版 → 本机 cross 交叉编译核心包 + 插件
# → 装船上传 → 远端执行本脚本 --phase remote（装包、装插件、建图起流）。
# 密码喂 ssh/scp 优先用 sshpass、缺则 expect；--user 非 root 时同一
# 密码喂 sudo -S。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

step() { printf '\n\033[1;36m=== %s\033[0m\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die()  { echo "ERROR: $*" >&2; exit 1; }

# ---- 三兄弟解析 -------------------------------------------------------------
# --phase remote 跑在装船布局里（无兄弟仓库），三目录都不解析。
PHASE_PROBE="auto"
case " $* " in *" --phase remote "*|*" --phase=remote "*) PHASE_PROBE=remote ;; esac
if [ "$PHASE_PROBE" = remote ]; then
    PUBLIC_DIR=""; PROJECT_DIR=""; CORE_DIR=""
    REPO_ROOT="$SCRIPT_DIR"   # 装船目录
    cd "$REPO_ROOT"
else
    PUBLIC_DIR="${SIGFLOW_PUBLIC_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)}"
    [ -f "$PUBLIC_DIR/Cargo.toml" ] || die "SIGFLOW_PUBLIC_DIR 不像公共仓库：$PUBLIC_DIR"
    PROJECT_DIR="${SIGFLOW_PROJECT_DIR:-}"
    [ -z "$PROJECT_DIR" ] || [ -d "$PROJECT_DIR" ] || die "SIGFLOW_PROJECT_DIR 不存在：$PROJECT_DIR"
    CORE_DIR="${SIGFLOW_CORE_DIR:-}"
    if [ -z "$CORE_DIR" ] && [ -d "$PUBLIC_DIR/../sigflow-core" ]; then
        CORE_DIR="$(cd "$PUBLIC_DIR/../sigflow-core" && pwd)"
    fi
    [ -z "$CORE_DIR" ] || [ -f "$CORE_DIR/package.sh" ] || die "SIGFLOW_CORE_DIR 不像核心仓库：$CORE_DIR"
fi

# ---- 引擎自身参数（项目参数全在 wrapper 侧） --------------------------------
DATA_ROOT_SET="${DATA_ROOT:+1}"
DATA_ROOT="${DATA_ROOT:-$HOME/sigflow-captures}"
NODE_DIR="${NODE_DIR:-/tmp/sigflow-node}"
export SIGFLOW_ROOT_PORT="${SIGFLOW_ROOT_PORT:-9500}"
export SIGFLOW_BIND_HOST="${SIGFLOW_BIND_HOST:-0.0.0.0}"
export CARGO_NET_GIT_FETCH_WITH_CLI=true
# Windows(Git Bash)：DATA_ROOT 经 env 直达原生进程，MSYS 只转换命令行参数
# 不转换 env——预先转成 C:/... 混合形式（NODE_DIR 走 CLI 参数，自动转换）。
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) DATA_ROOT="$(cygpath -m "$DATA_ROOT")" ;;
esac
export NODE_DIR DATA_ROOT

# 远程模式转发到远端的 env 名单 = 引擎基础 + wrapper 的 SIGFLOW_FORWARD_VARS
FORWARD_VARS=(NODE_DIR GRAPH_NAME SIGFLOW_ROOT_PORT SIGFLOW_BIND_HOST)
if [ -n "${SIGFLOW_FORWARD_VARS:-}" ]; then
    # 词分割整个值（不是 read -a：那只吃第一行，多行清单会静默丢光——
    # 排查代价极高：远端安静地用图例默认值跑，看不出旋钮没生效）
    # shellcheck disable=SC2206
    _extra=($SIGFLOW_FORWARD_VARS)
    FORWARD_VARS+=("${_extra[@]}")
fi
for _v in "${FORWARD_VARS[@]}"; do export "${_v?}" 2>/dev/null || true; done

# ---- 参数解析 ---------------------------------------------------------------
RHOST=""
RUSER="root"
# 尊重 env 里已有的 PASSWD：--phase remote 时密码经 env 传入（喂 sudo -S），
# 无条件清空会把远端阶段打回交互 sudo（root 部署不踩、--user 必踩）。
PASSWD="${PASSWD:-}"
PHASE="auto"
USE_ADB=0
GRAPH=""
while [ $# -gt 0 ]; do
    case "$1" in
        --adb)    USE_ADB=1; shift ;;
        --host)   RHOST="$2"; shift 2 ;;
        --host=*) RHOST="${1#--host=}"; shift ;;
        --passwd)   PASSWD="$2"; shift 2 ;;
        --passwd=*) PASSWD="${1#--passwd=}"; shift ;;
        --user)   RUSER="$2"; shift 2 ;;
        --user=*) RUSER="${1#--user=}"; shift ;;
        --phase)   PHASE="$2"; shift 2 ;;
        --phase=*) PHASE="${1#--phase=}"; shift ;;
        --graph)   GRAPH="$2"; shift 2 ;;
        --graph=*) GRAPH="${1#--graph=}"; shift ;;
        -h|--help)
            sed -n '2,46p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        -*) die "未知参数 '$1'（见 --help）" ;;
        *)  [ -z "$GRAPH" ] || die "多余的位置参数 '$1'（图例只取一个）"
            GRAPH="$1"; shift ;;
    esac
done

# ---- 图例解析：名字/路径 → 绝对路径 + 插件需求清单 --------------------------
list_examples() {
    local d f
    local dirs=()
    [ -z "$PROJECT_DIR" ] || dirs+=("$PROJECT_DIR/example")
    dirs+=("$PUBLIC_DIR/example")
    for d in "${dirs[@]}"; do for f in "$d"/*.sh; do
        [ -f "$f" ] || continue
        printf '    %-18s %s\n' "$(basename "$f" .sh)" \
            "$(sed -n '2s/^# *[^ ]* *— *//p' "$f")"
    done; done
}

resolve_graph() {
    [ -n "$GRAPH" ] || die "缺少图例参数。可用图例：
$(list_examples)"
    local cand
    local cands=("$GRAPH" "$PWD/$GRAPH")
    [ -z "$PROJECT_DIR" ] || cands+=("$PROJECT_DIR/example/$GRAPH" "$PROJECT_DIR/example/$GRAPH.sh")
    cands+=("$PUBLIC_DIR/example/$GRAPH" "$PUBLIC_DIR/example/$GRAPH.sh")
    for cand in "${cands[@]}"; do
        if [ -f "$cand" ]; then
            GRAPH_SCRIPT="$(cd "$(dirname "$cand")" && pwd)/$(basename "$cand")"
            return
        fi
    done
    die "找不到图例 '$GRAPH'。可用图例：
$(list_examples)"
}
[ "$PHASE" = remote ] || resolve_graph
[ "$PHASE" != remote ] || GRAPH_SCRIPT="$REPO_ROOT/graph.sh"

# 插件目录解析：通用插件在公共仓库，项目插件在项目仓库。项目侧优先——
# 项目里与公共插件同名的目录是有意覆盖（new-project 生成的图例引用的
# 是项目插件的 id，公共优先会把它遮蔽成"未安装"）。输出绝对路径。
plugin_dir() {  # $1 = native|process  $2 = 插件名 → 输出绝对路径
    if [ -n "$PROJECT_DIR" ] && [ -d "$PROJECT_DIR/plugins/$1/$2" ]; then
        echo "$PROJECT_DIR/plugins/$1/$2"
    else
        echo "$PUBLIC_DIR/plugins/$1/$2"
    fi
}

# 插件需求：图例头部的 requires-native / requires-process 声明；两者都未
# 声明则回退到全部插件（公共+项目 plugins/ 里有 build.sh 的目录）。
# 只在本机侧（--phase auto）解析：--phase remote 跑在装船布局
# （plugins-native/ 等）里，装船清单本身即需求清单。
NATIVE_PLUGINS=()
PROCESS_PLUGINS=()
if [ "$PHASE" = auto ]; then
    read -r -a NATIVE_PLUGINS <<< "$(sed -n 's/^# *requires-native: *//p' "$GRAPH_SCRIPT" | head -1)"
    read -r -a PROCESS_PLUGINS <<< "$(sed -n 's/^# *requires-process: *//p' "$GRAPH_SCRIPT" | head -1)"
    if [ "${#NATIVE_PLUGINS[@]}" -eq 0 ] && [ "${#PROCESS_PLUGINS[@]}" -eq 0 ]; then
        _proots=("$PUBLIC_DIR")
        [ -z "$PROJECT_DIR" ] || _proots+=("$PROJECT_DIR")
        for _r in "${_proots[@]}"; do
            for _d in "$_r"/plugins/native/*/; do
                [ -f "$_d/build.sh" ] || continue
                NATIVE_PLUGINS+=("$(basename "$_d")")
            done
            for _d in "$_r"/plugins/process/*/; do
                [ -f "$_d/build.sh" ] || continue
                PROCESS_PLUGINS+=("$(basename "$_d")")
            done
        done
    fi
    for _p in "${NATIVE_PLUGINS[@]:+${NATIVE_PLUGINS[@]}}"; do
        [ -d "$(plugin_dir native "$_p")" ] || die "图例声明的 native 插件不存在：${_p}（公共与项目 plugins/native 均无）"
    done
    for _p in "${PROCESS_PLUGINS[@]:+${PROCESS_PLUGINS[@]}}"; do
        [ -d "$(plugin_dir process "$_p")" ] || die "图例声明的 process 插件不存在：${_p}（公共与项目 plugins/process 均无）"
    done
fi

# 项目钩子目录：本地=项目仓库 deploy/hooks，remote 阶段=装船 hooks/
if [ "$PHASE" = remote ]; then
    HOOKS_DIR="$REPO_ROOT/hooks"
else
    HOOKS_DIR="${PROJECT_DIR:+$PROJECT_DIR/deploy/hooks}"
fi

run_precheck_hook() {  # $1 = os kind；source 执行，钩子可用 note/die/priv
    [ -n "$HOOKS_DIR" ] && [ -f "$HOOKS_DIR/precheck.sh" ] || return 0
    HOOK_OS="$1"
    # shellcheck disable=SC1091
    . "$HOOKS_DIR/precheck.sh"
}

# ---- ssh/scp 包装（--passwd 时经 expect 喂密码；否则走密钥） ---------------
SSH_OPTS="-o StrictHostKeyChecking=accept-new -o ConnectTimeout=10"
# expect 里的 $env() 只能看到导出的变量
export SSH_OPTS RUSER RHOST PASSWD

# RCMD 作为单个 argv 传给 ssh（{*} 只展开选项串，命令串不经 Tcl 重解析——
# 里面的引号/$ 原样到达远端 shell）。
ssh_do() {  # ssh_do <单条远端命令字符串>
    if [ -n "$PASSWD" ] && command -v sshpass >/dev/null; then
        # shellcheck disable=SC2086
        SSHPASS="$PASSWD" sshpass -e ssh $SSH_OPTS "$RUSER@$RHOST" "$1"
    elif [ -n "$PASSWD" ]; then
        RCMD="$1" expect <<'EXP'
set timeout -1
spawn ssh {*}[split $env(SSH_OPTS)] $env(RUSER)@$env(RHOST) $env(RCMD)
expect {
    -re "(?i)password:" { send -- "$env(PASSWD)\r"; exp_continue }
    eof
}
catch wait result
exit [lindex $result 3]
EXP
    else
        # shellcheck disable=SC2086
        ssh $SSH_OPTS "$RUSER@$RHOST" "$1"
    fi
}

scp_do() {  # scp_do <本地文件> <远端路径>
    if [ -n "$PASSWD" ] && command -v sshpass >/dev/null; then
        # shellcheck disable=SC2086
        SSHPASS="$PASSWD" sshpass -e scp $SSH_OPTS "$1" "$RUSER@$RHOST:$2"
    elif [ -n "$PASSWD" ]; then
        SRC="$1" DST="$2" expect <<'EXP'
set timeout -1
spawn scp {*}[split $env(SSH_OPTS)] $env(SRC) $env(RUSER)@$env(RHOST):$env(DST)
expect {
    -re "(?i)password:" { send -- "$env(PASSWD)\r"; exp_continue }
    eof
}
catch wait result
exit [lindex $result 3]
EXP
    else
        # shellcheck disable=SC2086
        scp $SSH_OPTS "$1" "$RUSER@$RHOST:$2"
    fi
}

# ---- 平台检测 ---------------------------------------------------------------
detect_os() {  # 输出 arch|debian|macos|windows|linux（在待部署机上调用）
    case "$(uname -s)" in
        Darwin) echo macos; return ;;
        MINGW*|MSYS*|CYGWIN*) echo windows; return ;;
        Linux)  ;;
        *) echo linux; return ;;
    esac
    if [ -f /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        case "${ID:-}:${ID_LIKE:-}" in
            arch:*|*:*arch*)     echo arch; return ;;
            debian:*|ubuntu:*|*:*debian*) echo debian; return ;;
        esac
    fi
    command -v pacman >/dev/null && { echo arch; return; }
    command -v dpkg   >/dev/null && { echo debian; return; }
    echo linux
}

# 特权执行：root 直接跑；非 root 有密码喂 sudo -S，无密码走交互 sudo
priv() {
    if [ "$(id -u)" = 0 ]; then "$@"
    elif [ -n "${PASSWD:-}" ]; then printf '%s\n' "$PASSWD" | sudo -S -p '' "$@"
    else sudo "$@"
    fi
}

# ============================================================================
# 建图：单一真源在选定的图例脚本（本地/ssh/adb 三路共用）
# ============================================================================
build_graph_and_start() {
    SIGFLOW_SKIP_PY="${SIGFLOW_SKIP_PY:-0}" sh "$GRAPH_SCRIPT"
}

# ============================================================================
# 通用子步骤（在部署目标机上执行）
# ============================================================================
stop_running() {
    step "停止运行中的 sigflow"
    sigflow-cli stop >/dev/null 2>&1 || true
    case "$(uname -s)" in
        MINGW*|MSYS*|CYGWIN*)
            # Windows 无 SIGTERM/pkill；stop 已走 shutdown RPC，这里只兜底强杀
            # （//F//IM：双斜杠防 Git Bash 把 /F 当路径转换）
            sleep 0.5
            taskkill //F //IM sigflow-shell.exe >/dev/null 2>&1 || true ;;
        *)
            pkill -f sigflow-shell 2>/dev/null || true
            sleep 0.5
            pkill -9 -f sigflow-shell 2>/dev/null || true ;;
    esac
    rm -rf /tmp/iceoryx2 2>/dev/null || true
    rm -f /dev/shm/iox2_* 2>/dev/null || true
    note "done（含 iceoryx2 残留清理）"
}

# dist/ 里最新的产物（按 mtime）。不用 `ls -t … | head -1`：dist 攒到几十个包
# 后 ls 的输出超过一次 pipe 写（4 KiB），head 读到第一行就退，ls 的下一次写
# 吃 SIGPIPE——pipefail 把 141 当失败，而赋值位置上 `x="$(…)"` 的失败没有任何
# 输出，set -e 于是在"打完包"之后不留一个字地退出。是不是撞上取决于 head 退得
# 比 ls 第二次写快不快，机器一忙（刚编译完）就撞，闲着就不撞：那个"有时"。
newest() {  # newest <glob…> → 最新的一个；一个都没有则失败（调用方 die）
    local f best=""
    for f in "$@"; do
        [ -e "$f" ] || continue
        if [ -z "$best" ] || [ "$f" -nt "$best" ]; then best="$f"; fi
    done
    [ -n "$best" ] && printf '%s\n' "$best"
}

# ============================================================================
# 核心获取阶梯（本地）：源码打包 → PATH 已装 → install.sh(Releases)
# ============================================================================
install_core_local() {  # $1 = os kind
    if [ -n "$CORE_DIR" ]; then
        command -v cargo >/dev/null || die "需要 cargo（https://rustup.rs）"
        step "打包并安装 sigflow（源码：${CORE_DIR}）"
        case "$1" in
            arch)
                priv pacman -R --noconfirm sigflow 2>/dev/null || note "（未安装，跳过卸载）"
                ( cd "$CORE_DIR" && ./package.sh --arch )
                local pkg; pkg="$(newest "$CORE_DIR"/dist/sigflow-*.pkg.tar.zst)" || die "dist/ 里没有 arch 包"
                priv pacman -U --noconfirm "$pkg" ;;
            debian)
                priv dpkg -r sigflow 2>/dev/null || note "（未安装，跳过卸载）"
                case "$(uname -m)" in
                    aarch64) ( cd "$CORE_DIR" && ./packaging/deb.sh --arch=arm64 ) ;;
                    *)       ( cd "$CORE_DIR" && ./packaging/deb.sh --arch=amd64 ) ;;
                esac
                local deb; deb="$(newest "$CORE_DIR"/dist/sigflow_*.deb)" || die "dist/ 里没有 deb 包"
                priv dpkg -i "$deb" ;;
            macos)
                ( cd "$CORE_DIR" && ./package.sh --macos )
                local mpkg; mpkg="$(newest "$CORE_DIR"/dist/sigflow-*.pkg)" || die "dist/ 里没有 macOS 包"
                priv installer -pkg "$mpkg" -target / ;;
            windows)
                # 无包管理器：原生 cargo 构建两个 exe，装到 ~/sigflow
                # （与 install.sh 同一目录约定，SIGFLOW_WINDOWS_DIR 可改）。
                ( cd "$CORE_DIR" && cargo build --release -p sigflow-shell -p sigflow-cli )
                local wdest="${SIGFLOW_WINDOWS_DIR:-$HOME/sigflow}"
                mkdir -p "$wdest"
                cp -f "$CORE_DIR"/target/release/sigflow-shell.exe \
                      "$CORE_DIR"/target/release/sigflow-cli.exe "$wdest/"
                PATH="$wdest:$PATH"; export PATH ;;
            *)
                note "未知发行版：cargo build + install 到 /usr/local/bin"
                ( cd "$CORE_DIR" && cargo build --release -p sigflow-shell -p sigflow-cli )
                priv install -m755 "$CORE_DIR"/target/release/sigflow-shell \
                    "$CORE_DIR"/target/release/sigflow-cli /usr/local/bin/ ;;
        esac
    elif command -v sigflow-cli >/dev/null; then
        step "核心：无源码 checkout，用已安装的 $(command -v sigflow-cli)"
    else
        step "核心：无源码也未安装——install.sh（GitHub Releases）"
        sh "$PUBLIC_DIR/install.sh"
        # Windows：install.sh 写的是注册表用户 PATH，本进程看不见——补上
        [ "$1" != windows ] || { PATH="${SIGFLOW_WINDOWS_DIR:-$HOME/sigflow}:$PATH"; export PATH; }
    fi
    hash -r
    command -v sigflow-cli >/dev/null || die "安装后找不到 sigflow-cli"
    note "sigflow-cli => $(command -v sigflow-cli)"
}

# ============================================================================
# 本地部署
# ============================================================================
deploy_local() {
    local os; os="$(detect_os)"
    step "本地部署（平台：$os / $(uname -m)，图例：$(basename "$GRAPH_SCRIPT")）"

    stop_running

    step "编译插件"
    PYTHON_BIN="${PYTHON:-$(command -v python3)}"
    local p
    for p in "${NATIVE_PLUGINS[@]:+${NATIVE_PLUGINS[@]}}"; do
        note "build $(plugin_dir native "$p")"
        ( cd "$(plugin_dir native "$p")" && ./build.sh )
    done
    for p in "${PROCESS_PLUGINS[@]:+${PROCESS_PLUGINS[@]}}"; do
        note "build $(plugin_dir process "$p")"
        ( cd "$(plugin_dir process "$p")" && PYTHON="$PYTHON_BIN" ./build.sh )
    done

    install_core_local "$os"
    run_precheck_hook "$os"

    step "安装插件到仓库"
    rm -rf "$HOME/.sigflow/plugins"
    for p in "${NATIVE_PLUGINS[@]:+${NATIVE_PLUGINS[@]}}"; do sigflow-cli plugin install "$(plugin_dir native "$p")/pkg"; done
    for p in "${PROCESS_PLUGINS[@]:+${PROCESS_PLUGINS[@]}}"; do sigflow-cli plugin install "$(plugin_dir process "$p")/pkg"; done
    local d
    for d in "$PUBLIC_DIR"/plugins/ui/*/; do sigflow-cli plugin install "$d"; done
    sigflow-cli plugin list

    build_graph_and_start
}

# ============================================================================
# 远程部署（本机侧：探测 → 交叉编译 → 装船 → 远端执行 --phase remote）
# ============================================================================
deploy_remote() {
    [ -n "$RHOST" ] || die "内部错误：RHOST 为空"
    [ -n "$CORE_DIR" ] || die "远程模式需要核心源码 checkout（SIGFLOW_CORE_DIR）——
    或在远端手动装核心（install.sh / Releases）后到远端本地部署"
    command -v cargo  >/dev/null || die "需要 cargo"
    { command -v sshpass >/dev/null || command -v expect >/dev/null || [ -z "$PASSWD" ]; } \
        || die "--passwd 模式需要 sshpass 或 expect"

    step "探测远端 $RUSER@$RHOST"
    # expect 会把 spawn 回显/密码提示混进 stdout——远端输出用标记括起来再提取。
    # 远端命令由登录 shell 解析（可能是 fish：不认 \${VAR:-} 与 VAR=v cmd
    # 前缀赋值）——显式包 sh -c，不依赖登录 shell 方言。
    local probe raw rc=0
    raw="$(ssh_do "sh -c 'echo __P0__; uname -m; . /etc/os-release 2>/dev/null; echo \${ID:-unknown}:\${ID_LIKE:-}; echo __P1__'")" || rc=$?
    [ "$rc" != 255 ] || die "ssh 连接失败（root 密码登录被拒时用 --user <用户名>，脚本会走 sudo）"
    probe="$(echo "$raw" | tr -d '\r' | sed -n '/^__P0__$/,/^__P1__$/p' | sed '1d;$d')"
    local RBASH=""
    if [ -z "$probe" ]; then
        # 连上了但没有 sh——大概率是 Windows（OpenSSH 默认 shell 是 cmd，
        # 报"系统找不到指定的路径"）。改探 Git Bash 的标准安装路径；不能
        # 用 PATH 上的 bash.exe——System32 那个是 WSL 入口，环境完全不同。
        # 探测输出沿用同款标记格式，成功即拿到 arch 并烙上 windows 记号。
        RBASH="${SIGFLOW_REMOTE_BASH:-C:\\Program Files\\Git\\bin\\bash.exe}"
        raw="$(ssh_do "\"$RBASH\" -c \"echo __P0__; uname -m; echo windows:gitbash; echo __P1__\"")" || true
        probe="$(echo "$raw" | tr -d '\r' | sed -n '/^__P0__$/,/^__P1__$/p' | sed '1d;$d')"
        [ -n "$probe" ] || die "远端既无 POSIX sh 也没探到 Git Bash（$RBASH）——
    Windows 目标需装 Git for Windows（非标准路径用
    SIGFLOW_REMOTE_BASH=<bash.exe 全路径> 指定）；或在目标机上
    Git Bash 里本地部署（install.sh + redeploy.sh）"
    fi
    local rarch rdistro
    rarch="$(echo "$probe" | sed -n 1p)"
    rdistro="$(echo "$probe" | sed -n 2p)"
    local target pkgkind
    case "$rdistro" in
        windows:*)                  pkgkind=windows ;;
        arch:*|*arch*)              pkgkind=arch ;;
        debian:*|ubuntu:*|*debian*) pkgkind=debian ;;
        *) pkgkind=linux ;;
    esac
    case "$pkgkind:$rarch" in
        windows:x86_64) target=x86_64-pc-windows-gnu ;;
        windows:*)      die "Windows 远端只支持 x86_64（当前 ${rarch}）" ;;
        *:x86_64)       target=x86_64-unknown-linux-gnu ;;
        *:aarch64)      target=aarch64-unknown-linux-gnu ;;
        *) die "远端架构 '$rarch' 暂不支持" ;;
    esac
    note "远端：$rarch / $rdistro → target=$target pkg=$pkgkind"

    # 工具链检查按目标分流：windows 用本机 mingw + cargo（无需 docker），
    # 其余走 cross 容器。
    local mingw_lnk=""
    if [ "$pkgkind" = windows ]; then
        command -v x86_64-w64-mingw32-gcc >/dev/null \
            || die "windows 目标需要 mingw-w64（pacman -S mingw-w64-gcc / apt install gcc-mingw-w64-x86-64）"
        rustup target list --installed 2>/dev/null | grep -qx "$target" \
            || die "缺 rust 目标 ${target}——先 rustup target add ${target}"
        # cargo 认的 linker env 名：triple 大写、'-'→'_'
        mingw_lnk="CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_LINKER=x86_64-w64-mingw32-gcc"
    else
        command -v cross >/dev/null || die "远程模式需要 cross（cargo install cross）+ docker"
    fi

    # 独立 target 目录：cross 各 docker 镜像的 host 构建脚本二进制互不兼容，
    # 共用 target/ 会交叉污染缓存（packaging/common.sh 同款隔离）。
    export CARGO_TARGET_DIR="$CORE_DIR/target/xdeploy"

    step "交叉编译 sigflow 包（${target}）"
    case "$pkgkind" in
        arch)   ( cd "$CORE_DIR" && ./packaging/arch.sh --target="$target" ) ;;
        debian) case "$rarch" in
                    aarch64) ( cd "$CORE_DIR" && ./packaging/deb.sh --arch=arm64 ) ;;
                    *)       ( cd "$CORE_DIR" && ./packaging/deb.sh --arch=amd64 ) ;;
                esac ;;
        windows) ( cd "$CORE_DIR" && env "$mingw_lnk" \
                    cargo build --release --target "$target" -p sigflow-shell -p sigflow-cli ) ;;
        *)      ( cd "$CORE_DIR" && cross build --release --target "$target" -p sigflow-shell -p sigflow-cli ) ;;
    esac

    step "交叉编译 native 插件（公共侧 + 项目侧 workspace）"
    if [ "$pkgkind" = windows ]; then
        # mingw 直出，路径依赖原生解析，无需 cross 容器与 SIGFLOW_SIBLING
        ( cd "$PUBLIC_DIR" && CARGO_TARGET_DIR="$PUBLIC_DIR/target/xdeploy" \
            env "$mingw_lnk" cargo build --release --target "$target" --workspace )
        if [ -n "$PROJECT_DIR" ] && [ -f "$PROJECT_DIR/Cargo.toml" ]; then
            ( cd "$PROJECT_DIR" && CARGO_TARGET_DIR="$PROJECT_DIR/target/xdeploy" \
                env "$mingw_lnk" cargo build --release --target "$target" --workspace )
        fi
    else
        ( cd "$PUBLIC_DIR" && CARGO_TARGET_DIR="$PUBLIC_DIR/target/xdeploy" \
            cross build --release --target "$target" --workspace )
        if [ -n "$PROJECT_DIR" ] && [ -f "$PROJECT_DIR/Cargo.toml" ]; then
            # SIGFLOW_SIBLING：项目仓库的 Cross.toml 把 sigflow 挂进容
            # 器，其 [patch] 的 ../sigflow 路径依赖才解析得到
            ( cd "$PROJECT_DIR" && SIGFLOW_SIBLING="$PUBLIC_DIR" \
                CARGO_TARGET_DIR="$PROJECT_DIR/target/xdeploy" \
                cross build --release --target "$target" --workspace )
        fi
    fi

    step "装船"
    local stage; stage="$(mktemp -d)"
    # trap 在函数返回后触发时 local 已出作用域——把值烙进 trap 串
    # shellcheck disable=SC2064
    trap "rm -rf '$stage'" EXIT INT TERM
    mkdir -p "$stage/deploy/pkg" "$stage/deploy/plugins-native" \
             "$stage/deploy/plugins-process" "$stage/deploy/plugins-ui"

    case "$pkgkind" in
        arch)   cp "$(newest "$CORE_DIR"/dist/sigflow-*.pkg.tar.zst)" "$stage/deploy/pkg/" ;;
        debian) cp "$(newest "$CORE_DIR"/dist/sigflow_*.deb)" "$stage/deploy/pkg/" ;;
        windows) mkdir -p "$stage/deploy/pkg/bin"
                cp "$CARGO_TARGET_DIR/$target/release/sigflow-shell.exe" \
                   "$CARGO_TARGET_DIR/$target/release/sigflow-cli.exe" "$stage/deploy/pkg/bin/" ;;
        *)      mkdir -p "$stage/deploy/pkg/bin"
                cp "$CARGO_TARGET_DIR/$target/release/sigflow-shell" \
                   "$CARGO_TARGET_DIR/$target/release/sigflow-cli" "$stage/deploy/pkg/bin/" ;;
    esac

    # native 插件：交叉编译产物 + 重写 library 字段的 manifest（公共侧
    # 产物在 public/target/xdeploy，项目侧在项目 target/xdeploy）
    local p crate lib pdir tdir
    for p in "${NATIVE_PLUGINS[@]:+${NATIVE_PLUGINS[@]}}"; do
        pdir="$(plugin_dir native "$p")"
        case "$pdir" in
            "$PUBLIC_DIR/"*) tdir="$PUBLIC_DIR/target/xdeploy" ;;
            *)               tdir="$PROJECT_DIR/target/xdeploy" ;;
        esac
        crate="$(sed -n 's/^name = "\(.*\)"/\1/p' "$pdir/Cargo.toml" | head -1)"
        if [ "$pkgkind" = windows ]; then
            lib="$(echo "$crate" | tr '-' '_').dll"   # windows 无 lib 前缀
        else
            lib="lib$(echo "$crate" | tr '-' '_').so"
        fi
        [ -f "$tdir/$target/release/$lib" ] || die "缺 ${lib}（cross workspace 构建产物）"
        mkdir -p "$stage/deploy/plugins-native/$p"
        cp "$tdir/$target/release/$lib" "$stage/deploy/plugins-native/$p/"
        sed "s|^library = \".*\"|library = \"$lib\"|" "$pdir/manifest.toml" \
            > "$stage/deploy/plugins-native/$p/manifest.toml"
        # manifest 声明的包内资源（[[documents]].schema_file 等）也得上船：
        # 壳体是从安装目录读它们的，只装 manifest + 动态库的话，插件到了目标
        # 机就少一半——而且少的那半只有点开编辑器才发现。
        for asset in $(sed -n 's/^[[:space:]]*schema_file[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' \
                       "$pdir/manifest.toml"); do
            case "$asset" in
                */*|"") die "插件 $p 的 schema_file 必须是包内的纯文件名：$asset" ;;
            esac
            [ -f "$pdir/$asset" ] || die "插件 $p 的 manifest 声明了 $asset，但包里没有"
            cp "$pdir/$asset" "$stage/deploy/plugins-native/$p/"
        done
    done
    # python 插件：源码 + SDK 上船，build.sh 在远端跑（烙远端解释器 + 依赖检查）
    for p in "${PROCESS_PLUGINS[@]:+${PROCESS_PLUGINS[@]}}"; do
        rsync -a --exclude pkg --exclude __pycache__ "$(plugin_dir process "$p")" "$stage/deploy/plugins-process/"
    done
    # 船上目录名保持 sigflow-plugin-sdk-python——phase_remote 的 /tmp 软链
    # 与各插件 build.sh 的回退查找路径都认这个名字
    rsync -a --exclude __pycache__ "$PUBLIC_DIR/sdk/python/" "$stage/deploy/sigflow-plugin-sdk-python/"
    # ui 插件：纯 manifest
    for d in "$PUBLIC_DIR"/plugins/ui/*/; do
        rsync -a "$d" "$stage/deploy/plugins-ui/$(basename "$d")/"
    done
    # 项目钩子随船（phase_remote 在装船布局 hooks/ 里找）
    if [ -n "$HOOKS_DIR" ] && [ -d "$HOOKS_DIR" ]; then
        rsync -a "$HOOKS_DIR/" "$stage/deploy/hooks/"
    fi
    cp "$0" "$stage/deploy/redeploy.sh"
    cp "$GRAPH_SCRIPT" "$stage/deploy/graph.sh"

    local envfwd="" v
    for v in "${FORWARD_VARS[@]}"; do
        # 未设的变量不转发（图例脚本自带默认）
        [ -n "${!v:-}" ] && envfwd+="$v='${!v}' "
    done
    [ -n "${DATA_ROOT_SET:-}" ] && envfwd+="DATA_ROOT='$DATA_ROOT' "
    [ -n "${PYTHON:-}" ] && envfwd+="PYTHON='$PYTHON' "

    # windows：phase-remote 启动器随船（env 前缀在此烘焙，绕开 ssh→cmd→
    # bash→powershell 的多层引号转义）。经 Start-Process 分离启动，rc 落
    # 文件供本机轮询。
    if [ "$pkgkind" = windows ]; then
        cat > "$stage/deploy/win-launch.sh" <<EOF
#!/bin/sh
# Generated by deploy_remote — do not edit.
cd "\$(dirname "\$0")"
$envfwd bash redeploy.sh --phase remote --graph graph.sh > deploy.log 2>&1 < /dev/null
echo \$? > deploy.rc
EOF
    fi

    COPYFILE_DISABLE=1 tar -C "$stage" -czf "$stage/deploy.tar.gz" deploy

    if [ "$pkgkind" = windows ]; then
        # Windows OpenSSH 的默认 shell 是 cmd：所有远端命令都显式经 Git
        # Bash 转手。scp 的相对路径落 %USERPROFILE%，恰与 Git Bash 的
        # $HOME 同一目录（/tmp 两侧含义不同，不能用）；\$HOME 由远端
        # bash 展开。注意 cmd 在双引号内不吃 &&，但仍会展开 %VAR%——
        # 转发值里别带百分号。Windows 无 sudo，PASSWD 不转发。
        local wdir="sigflow-deploy-$RUSER"
        note "$(du -h "$stage/deploy.tar.gz" | cut -f1) → $RUSER@$RHOST:~/$wdir（Git Bash）"
        # 旧实例的 shell 以装船目录为 cwd，Windows 下被占用的目录删不
        # 掉——预清理前先停旧进程（幂等重部署本就要停）
        # 连带清 iceoryx2 共享内存残骸（强杀的 shell 留下 shm_state 尸体，
        # 同名服务下次 open 报 ServiceInCorruptedState）
        ssh_do "\"$RBASH\" -lc \"taskkill //F //IM sigflow-shell.exe > /dev/null 2>&1; sleep 1; rm -rf \$HOME/$wdir \$HOME/$wdir.tar.gz /c/Temp/iceoryx2 /c/Temp/iox2_*\""
        scp_do "$stage/deploy.tar.gz" "$wdir.tar.gz"
        ssh_do "\"$RBASH\" -lc \"mkdir -p \$HOME/$wdir && tar -C \$HOME/$wdir --strip-components 1 -xzf \$HOME/$wdir.tar.gz\""

        step "远端执行部署（--phase remote，Git Bash，分离+轮询）"
        # 不能同步等：起流的 sigflow-shell 从 msys→原生边界继承 ssh 会话
        # 的管道句柄（Windows CreateProcess 继承父表全部可继承句柄，Rust
        # std 不设句柄白名单），通道等不到 EOF，ssh 永挂。也不能 bash 后
        # 台跑——Windows sshd 会话关闭时连带杀后台进程。唯一活路：经
        # PowerShell Start-Process（ShellExecute）分离启动 win-launch.sh，
        # 脱离 ssh 会话进程树；rc 落文件，本机轮询取回。
        ssh_do "\"$RBASH\" -lc \"cd \$HOME/$wdir && rm -f deploy.rc deploy.log && export BASHEXE='$RBASH' && export WDIR=\$(cygpath -w \$PWD) && powershell.exe -NoProfile -Command 'Start-Process -WindowStyle Hidden -FilePath \$env:BASHEXE -ArgumentList win-launch.sh -WorkingDirectory \$env:WDIR' && echo LAUNCHED\""
        local i rrc=""
        for i in $(seq 1 150); do
            sleep 2
            rrc="$(ssh_do "\"$RBASH\" -lc \"cat \$HOME/$wdir/deploy.rc 2>/dev/null\"" 2>/dev/null | tr -d '\r' | tail -1)" || true
            [ -z "$rrc" ] || break
        done
        ssh_do "\"$RBASH\" -lc \"tail -n 40 \$HOME/$wdir/deploy.log\"" || true
        [ "$rrc" = 0 ] || die "远端部署失败（rc=${rrc:-轮询超时}；完整日志：目标机 ~/$wdir/deploy.log）"
    else
        # 装船目录按远端用户隔离：root 与普通用户交替部署时,/tmp 的 sticky
        # 位会让后者删不掉前者的目录。
        local rdir="/tmp/sigflow-deploy-$RUSER"
        note "$(du -h "$stage/deploy.tar.gz" | cut -f1) → $RUSER@$RHOST:$rdir"
        ssh_do "rm -rf $rdir $rdir.tar.gz"
        scp_do "$stage/deploy.tar.gz" "$rdir.tar.gz"
        ssh_do "mkdir -p $rdir && tar -C $rdir --strip-components 1 -xzf $rdir.tar.gz"

        step "远端执行部署（--phase remote）"
        # VAR=v cmd 前缀赋值是 sh/bash 语法，fish 登录 shell 不认——包 sh -c
        ssh_do "sh -c \"cd $rdir && $envfwd PASSWD='$PASSWD' bash redeploy.sh --phase remote --graph graph.sh\""
    fi

    printf '\n\033[1;32m远程部署完成：%s（%s/%s，图例：%s）\033[0m\n' \
        "$RHOST" "$rarch" "$pkgkind" "$(basename "$GRAPH_SCRIPT")"
    printf '编辑器：http://%s:%s/\n' "$RHOST" "$SIGFLOW_ROOT_PORT"
}

# ============================================================================
# Android 部署（--host + --adb：adb 网络连接 → bionic tarball → 降级图）
# ============================================================================
deploy_adb() {
    [ -n "$RHOST" ] || die "--adb 需要 --host <设备IP[:端口]>"
    [ -n "$CORE_DIR" ] || die "Android 模式需要核心源码 checkout（SIGFLOW_CORE_DIR）——
    或手动取 Releases 的 android tarball 按其内附 README 安装"
    command -v adb   >/dev/null || die "需要 adb（brew install android-platform-tools）"
    command -v cross >/dev/null || die "Android 模式需要 cross（cargo install cross）+ docker"
    local target="$RHOST"
    case "$target" in *:*) ;; *) target="$target:5555" ;; esac

    step "adb 连接 ${target}（先清空设备列表）"
    adb disconnect >/dev/null 2>&1 || true
    adb connect "$target" | grep -q "connected" || die "adb connect $target 失败"
    adb devices -l | sed 's/^/    /'

    note "adb root（adbd 重启后重连）"
    adb -s "$target" root >/dev/null 2>&1 || true
    local i ok=""
    for i in 1 2 3 4 5 6 7 8 9 10; do
        adb connect "$target" >/dev/null 2>&1 || true
        if [ "$(adb -s "$target" shell id -u 2>/dev/null | tr -d '\r')" = 0 ]; then ok=1; break; fi
        sleep 1
    done
    [ -n "$ok" ] || die "adb root 后未能以 root 重连 $target"
    note "root OK"

    note "adb remount"
    local remounted=""
    if adb -s "$target" remount 2>&1 | grep -qi "remount succeeded\|already"; then
        remounted=1
        note "remount OK（sigflow-shell/cli 将装入 /system/bin）"
    else
        note "⚠ remount 失败（verity？）——二进制留在 /data/local/tmp/sigflow/bin"
    fi

    local abi
    abi="$(adb -s "$target" shell getprop ro.product.cpu.abi | tr -d '\r')"
    case "$abi" in
        arm64*) ;;
        *) die "设备 ABI '$abi' 暂不支持（需要 arm64-v8a）" ;;
    esac
    note "设备：${abi}，python 插件走降级图（SIGFLOW_SKIP_PY=1）"

    step "构建 bionic tarball（packaging/android.sh）"
    local stage; stage="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$stage'" EXIT INT TERM
    SIGFLOW_PUBLIC_DIR="$PUBLIC_DIR" SIGFLOW_PROJECT_DIR="${PROJECT_DIR:-/nonexistent}" \
        "$CORE_DIR/packaging/android.sh" --out="$stage"
    local tarball
    tarball="$(newest "$stage"/sigflow-*-android-arm64-bionic.tar.gz)" || die "android.sh 没在 $stage 出 tarball"

    step "推送并安装到设备"
    adb -s "$target" push "$tarball" /data/local/tmp/sigflow-deploy.tar.gz
    adb -s "$target" push "$GRAPH_SCRIPT" /data/local/tmp/graph.sh >/dev/null

    # 开机自启（init service）：rc + 守护脚本进 /system（overlayfs scratch
    # 持久，改 rc 重启后生效）。守护常驻盯 9500，根壳体死→整树重建。
    if [ -n "$remounted" ]; then
        adb -s "$target" push "$PUBLIC_DIR/deploy/android/sigflow.rc" /system/etc/init/sigflow.rc >/dev/null
        adb -s "$target" push "$PUBLIC_DIR/deploy/android/sigflow-boot.sh" /system/bin/sigflow-boot.sh >/dev/null
        adb -s "$target" shell 'chmod 644 /system/etc/init/sigflow.rc; chmod 755 /system/bin/sigflow-boot.sh; restorecon /system/etc/init/sigflow.rc /system/bin/sigflow-boot.sh'
        note "自启文件已推送（sigflow.rc + sigflow-boot.sh）"
    else
        note "⚠ /system 未 remount——跳过自启文件推送"
    fi

    # 项目开机钩子：守护脚本每次启动 source（见头部契约）。落 /data/local/tmp
    # 而非 /system——不依赖 remount，且 adb-pre-install.sh 也能 source 它复用
    # 同一套清场动作。先于预清场钩子推送，保证后者能读到。
    if [ -n "$HOOKS_DIR" ] && [ -f "$HOOKS_DIR/boot-hook.sh" ]; then
        adb -s "$target" push "$HOOKS_DIR/boot-hook.sh" /data/local/tmp/sigflow-boot-hook.sh >/dev/null
        note "项目开机钩子已推送（boot-hook.sh）"
    fi

    # 项目 adb 预清场钩子（装包前在设备上执行）
    local hook_line=""
    if [ -n "$HOOKS_DIR" ] && [ -f "$HOOKS_DIR/adb-pre-install.sh" ]; then
        adb -s "$target" push "$HOOKS_DIR/adb-pre-install.sh" /data/local/tmp/sigflow-hook-pre.sh >/dev/null
        hook_line="sh /data/local/tmp/sigflow-hook-pre.sh || true"
        note "项目预清场钩子已推送（adb-pre-install.sh）"
    fi

    # 设备端安装脚本（mksh/toybox 兼容；一次 shell 跑完，输出落日志文件）
    local envfwd="" v
    for v in "${FORWARD_VARS[@]}"; do
        # 未设的变量不转发（图例脚本自带默认）
        [ -n "${!v:-}" ] && envfwd+="$v='${!v}' "
    done
    [ -n "${DATA_ROOT_SET:-}" ] && envfwd+="DATA_ROOT='$DATA_ROOT' "

    # 部署期参数覆盖固化给开机自启（sigflow-boot.sh 建图前 source）
    local envfile="$stage/graph.env"
    : > "$envfile"
    for v in "${FORWARD_VARS[@]}"; do
        [ -n "${!v:-}" ] && printf "export %s='%s'\n" "$v" "${!v}" >> "$envfile"
    done
    [ -n "${DATA_ROOT_SET:-}" ] && printf "export DATA_ROOT='%s'\n" "$DATA_ROOT" >> "$envfile"
    adb -s "$target" push "$envfile" /data/local/tmp/graph.env >/dev/null

    adb -s "$target" shell "sh -c '
set -e
# 部署互斥：守护脚本见 pause 不抢跑（10min 过期兜底）；成功后建图段删除
touch /data/local/tmp/sigflow-boot.pause
pkill sigflow-shell 2>/dev/null || true
$hook_line
sleep 1
# 注册表随 HOME 持久，重启后 PID 复用会让陈旧条目装活——全杀光后直接清
rm -rf /tmp/iceoryx2 /dev/shm/iox2_* ${NODE_DIR} /data/local/tmp/sigflow/.sigflow/registry
mkdir -p /dev/shm && mountpoint -q /dev/shm || mount -t tmpfs -o size=256m tmpfs /dev/shm
mountpoint -q /tmp 2>/dev/null || mount -t tmpfs tmpfs /tmp 2>/dev/null || true
rm -rf /data/local/tmp/sigflow
cd /data/local/tmp && tar -xzf sigflow-deploy.tar.gz && rm sigflow-deploy.tar.gz
chmod 755 /data/local/tmp/sigflow/bin/*
if [ -n \"$remounted\" ]; then
    cp /data/local/tmp/sigflow/bin/sigflow-shell /data/local/tmp/sigflow/bin/sigflow-cli /system/bin/ && chmod 755 /system/bin/sigflow-shell /system/bin/sigflow-cli || true
fi
export PATH=/data/local/tmp/sigflow/bin:\$PATH
export HOME=/data/local/tmp/sigflow
rm -rf \$HOME/.sigflow/plugins
for d in /data/local/tmp/sigflow/plugins/*/; do sigflow-cli plugin install \"\$d\" >/dev/null; done
echo \"plugins installed: \$(ls \$HOME/.sigflow/plugins | wc -l)\"
' " || die "设备端安装失败"

    # SIGFLOW_SKIP_PY=1：android 路径不 stage python 插件，引用 python 节点
    # 的图例走降级骨架；全 native 图例不读该变量，整图照常。
    step "设备上建图起流（$(basename "$GRAPH_SCRIPT")）"
    # && 串联：graph.sh 失败时整条命令非零退出（否则 tail 的 0 吞掉建图失败）
    adb -s "$target" shell "sh -c 'export PATH=/data/local/tmp/sigflow/bin:\$PATH HOME=/data/local/tmp/sigflow; $envfwd SIGFLOW_SKIP_PY=1 SIGFLOW_NICE=-10 sh /data/local/tmp/graph.sh > /tmp/sigflow-graph.log 2>&1 < /dev/null && rm -f /data/local/tmp/sigflow-boot.pause && tail -n 30 /tmp/sigflow-graph.log'" \
        || { adb -s "$target" shell 'tail -n 40 /tmp/sigflow-graph.log'; die "建图失败（完整日志在设备 /tmp/sigflow-graph.log）"; }

    printf '\n\033[1;32mAndroid 部署完成：%s（%s，图例：%s）\033[0m\n' \
        "$RHOST" "$abi" "$(basename "$GRAPH_SCRIPT")"
    printf '编辑器：http://%s:%s/\n' "${RHOST%%:*}" "$SIGFLOW_ROOT_PORT"
    printf '设备图日志：adb -s %s shell cat /tmp/sigflow-graph.log\n' "$target"
}

# ============================================================================
# 远端阶段（在远端 /tmp/sigflow-deploy-* 下执行；root 或 sudo）
# ============================================================================
phase_remote() {
    local os; os="$(detect_os)"
    step "远端安装（平台：$os / $(uname -m)，用户：$(id -un)，图例：$(basename "$GRAPH_SCRIPT")）"

    stop_running

    step "安装 sigflow 包"
    case "$os" in
        arch)
            priv pacman -R --noconfirm sigflow 2>/dev/null || note "（未安装，跳过卸载）"
            priv pacman -U --noconfirm pkg/sigflow-*.pkg.tar.zst ;;
        debian)
            priv dpkg -r sigflow 2>/dev/null || note "（未安装，跳过卸载）"
            priv dpkg -i pkg/sigflow_*.deb ;;
        windows)
            # 无包管理器：exe 拷到 ~/sigflow（与 install.sh/本地部署同一
            # 目录约定），本进程 PATH 直接补上
            mkdir -p "$HOME/sigflow"
            cp -f pkg/bin/sigflow-shell.exe pkg/bin/sigflow-cli.exe "$HOME/sigflow/"
            PATH="$HOME/sigflow:$PATH"; export PATH ;;
        *)
            priv install -m755 pkg/bin/sigflow-shell pkg/bin/sigflow-cli /usr/local/bin/ ;;
    esac
    hash -r
    command -v sigflow-cli >/dev/null || die "安装后找不到 sigflow-cli"
    note "sigflow-cli => $(command -v sigflow-cli)"

    run_precheck_hook "$os"

    step "stage python 插件（远端解释器）"
    PYTHON_BIN="${PYTHON:-$(command -v python3 || command -v python || true)}"
    # SDK 定位：显式 env 两种装船布局（/tmp/... 与 ~/sigflow-deploy-*）
    # 通吃；/tmp 软链是老 build.sh ../../../ 回退链的兼容，windows 布局
    # 用不上且 Git Bash 无真软链，跳过。
    # 值是 **包目录**（含 sigflow_sdk/*.py）——插件 build.sh 直接
    # `cp "$SIGFLOW_PY_SDK"/*.py`，新旧脚手架同此约定。装船布局把
    # sdk/python/ 整个搬进 sigflow-plugin-sdk-python/，故要下潜一层。
    export SIGFLOW_PY_SDK="$REPO_ROOT/sigflow-plugin-sdk-python/sigflow_sdk"
    [ "$os" = windows ] || ln -sfn "$REPO_ROOT/sigflow-plugin-sdk-python" /tmp/sigflow-plugin-sdk-python \
        || die "清不掉旧的 /tmp/sigflow-plugin-sdk-python（他人属主？先用对应用户删除）"
    local p
    for p in plugins-process/*/; do
        [ -d "$p" ] || continue
        note "build $p"
        ( cd "$p" && PYTHON="$PYTHON_BIN" ./build.sh )
    done

    step "plugin install"
    rm -rf "$HOME/.sigflow/plugins"
    local d
    for d in plugins-native/*/;  do [ -d "$d" ] || continue; sigflow-cli plugin install "$d"; done
    for d in plugins-process/*/; do [ -d "$d" ] || continue; sigflow-cli plugin install "$d/pkg"; done
    for d in plugins-ui/*/;      do [ -d "$d" ] || continue; sigflow-cli plugin install "$d"; done
    sigflow-cli plugin list

    build_graph_and_start
}

# ============================================================================
# 入口
# ============================================================================
case "$PHASE" in
    remote) phase_remote ;;
    auto)
        if [ "$USE_ADB" = 1 ]; then deploy_adb
        elif [ -n "$RHOST" ]; then deploy_remote
        else deploy_local; fi ;;
    *) die "未知 --phase '$PHASE'" ;;
esac
