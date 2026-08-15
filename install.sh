#!/bin/sh
# install.sh — 从 GitHub Releases 安装 sigflow 核心（sigflow-shell + sigflow-cli 预编译包）。
#
#   curl -fsSL https://raw.githubusercontent.com/YHF-1404/sigflow/main/install.sh | sh
#   ./install.sh [--dry-run] [--version vX.Y.Z]
#
# 平台探测：macOS(arm64) → .pkg；Arch Linux(x86_64) → pacman 包；
# Debian/Ubuntu(amd64|arm64) → .deb；Windows(Git Bash/MSYS, x86_64) →
# 免安装 zip，解压到 ~/sigflow（SIGFLOW_WINDOWS_DIR 可改）并写入用户
# PATH。Android 走 Releases 里的 bionic tarball + 手动 adb 安装
# （tarball 内 README 有步骤）。
set -eu

REPO="YHF-1404/sigflow"
DRY=0
WANT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run)   DRY=1; shift ;;
        --version)   WANT="$2"; shift 2 ;;
        --version=*) WANT="${1#--version=}"; shift ;;
        -h|--help)   sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "install.sh: unknown arg '$1'" >&2; exit 2 ;;
    esac
done

die() { echo "install.sh: $*" >&2; exit 1; }
command -v curl >/dev/null || die "need curl"

# ---- 平台 → 资产文件名模式 ----
OS="$(uname -s)"; ARCH="$(uname -m)"
KIND=""
case "$OS" in
    Darwin)
        [ "$ARCH" = arm64 ] || die "只提供 macOS arm64 包（当前 ${ARCH}）"
        PATTERN='-darwin-arm64\.pkg'; KIND=macos ;;
    Linux)
        DISTRO=""
        if [ -f /etc/os-release ]; then
            # shellcheck disable=SC1091
            . /etc/os-release
            case "${ID:-}:${ID_LIKE:-}" in
                arch:*|*:*arch*)              DISTRO=arch ;;
                debian:*|ubuntu:*|*:*debian*) DISTRO=debian ;;
            esac
        fi
        case "$DISTRO" in
            arch)
                [ "$ARCH" = x86_64 ] || die "Arch 包只提供 x86_64（当前 ${ARCH}）"
                PATTERN='-x86_64\.pkg\.tar\.zst'; KIND=arch ;;
            debian)
                case "$ARCH" in
                    x86_64)  PATTERN='_amd64\.deb' ;;
                    aarch64) PATTERN='_arm64\.deb' ;;
                    *) die "Debian 包只提供 amd64/arm64（当前 ${ARCH}）" ;;
                esac
                KIND=debian ;;
            *) die "未识别的发行版——Android 设备请用 Releases 里的 android tarball（内含 README），其他 Linux 可从源码构建" ;;
        esac ;;
    MINGW*|MSYS*|CYGWIN*)
        [ "$ARCH" = x86_64 ] || die "Windows 包只提供 x86_64（当前 ${ARCH}）"
        PATTERN='-windows-x86_64\.zip'; KIND=windows ;;
    *) die "不支持的平台 $OS" ;;
esac

# ---- 解析 Release 资产 URL（GitHub API，逐行 JSON，无需 jq）----
if [ -n "$WANT" ]; then
    API="https://api.github.com/repos/$REPO/releases/tags/$WANT"
else
    API="https://api.github.com/repos/$REPO/releases/latest"
fi
URL="$(curl -fsSL "$API" \
    | grep '"browser_download_url"' \
    | grep -E -e "$PATTERN" \
    | head -1 \
    | sed 's/.*"\(https:[^"]*\)".*/\1/')"
[ -n "$URL" ] || die "Release 里没有匹配 $PATTERN 的资产（${API}）"

echo "→ $URL"
[ "$DRY" = 1 ] && exit 0

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM
FILE="$TMP/$(basename "$URL")"
curl -fL -o "$FILE" "$URL"

case "$KIND" in
    macos)  sudo installer -pkg "$FILE" -target / ;;
    arch)   sudo pacman -U --noconfirm "$FILE" ;;
    debian) sudo dpkg -i "$FILE" ;;
    windows)
        # 免安装 zip：解压到 DEST，二进制拍平（zip 顶层是版本命名目录）。
        DEST="${SIGFLOW_WINDOWS_DIR:-$HOME/sigflow}"
        command -v cygpath >/dev/null || die "需在 Git Bash / MSYS2 里运行"
        mkdir -p "$DEST" "$TMP/x"
        if command -v unzip >/dev/null; then
            unzip -oq "$FILE" -d "$TMP/x"
        else
            powershell.exe -NoProfile -Command \
                "Expand-Archive -Force -LiteralPath '$(cygpath -w "$FILE")' -DestinationPath '$(cygpath -w "$TMP/x")'" \
                || die "解压失败（无 unzip，Expand-Archive 也失败）"
        fi
        INNER="$(find "$TMP/x" -mindepth 1 -maxdepth 1 -type d | head -1)"
        [ -n "$INNER" ] || die "zip 布局异常：顶层不是目录"
        cp -f "$INNER"/* "$DEST/"
        # 用户 PATH（注册表，幂等追加）；失败降级为手动提示。本会话用
        # export 让脚本末尾的自检能找到 sigflow-cli。
        WDEST="$(cygpath -w "$DEST")"
        powershell.exe -NoProfile -Command "
            \$p = [Environment]::GetEnvironmentVariable('Path', 'User');
            if ((';' + \$p + ';') -notlike ('*;' + '$WDEST' + ';*')) {
                [Environment]::SetEnvironmentVariable('Path', \$p + ';' + '$WDEST', 'User');
            }" >/dev/null 2>&1 \
            || echo "install.sh: 未能写入用户 PATH，请手动加入：$WDEST" >&2
        PATH="$DEST:$PATH"; export PATH
        echo "→ 已解压到 $WDEST（已写入用户 PATH；已开的其他终端需重开生效）"
        ;;
esac

hash -r 2>/dev/null || true
command -v sigflow-cli >/dev/null || die "安装完成但 PATH 里找不到 sigflow-cli"
echo "✓ $(sigflow-cli --version 2>/dev/null || echo sigflow-cli) 已就绪"
