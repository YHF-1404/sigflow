#!/bin/sh
# install.sh — 从 GitHub Releases 安装 sigflow 核心（sigflow-shell + sigflow-cli 预编译包）。
#
#   curl -fsSL https://raw.githubusercontent.com/YHF-1404/sigflow/main/install.sh | sh
#   ./install.sh [--dry-run] [--version vX.Y.Z]
#
# 平台探测：macOS(arm64) → .pkg；Arch Linux(x86_64) → pacman 包；
# Debian/Ubuntu(amd64|arm64) → .deb。Android 走 Releases 里的 bionic
# tarball + 手动 adb 安装（tarball 内 README 有步骤）。
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
        [ "$ARCH" = arm64 ] || die "只提供 macOS arm64 包（当前 $ARCH）"
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
                [ "$ARCH" = x86_64 ] || die "Arch 包只提供 x86_64（当前 $ARCH）"
                PATTERN='-x86_64\.pkg\.tar\.zst'; KIND=arch ;;
            debian)
                case "$ARCH" in
                    x86_64)  PATTERN='_amd64\.deb' ;;
                    aarch64) PATTERN='_arm64\.deb' ;;
                    *) die "Debian 包只提供 amd64/arm64（当前 $ARCH）" ;;
                esac
                KIND=debian ;;
            *) die "未识别的发行版——Android 设备请用 Releases 里的 android tarball（内含 README），其他 Linux 可从源码构建" ;;
        esac ;;
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
    | grep -E "$PATTERN" \
    | head -1 \
    | sed 's/.*"\(https:[^"]*\)".*/\1/')"
[ -n "$URL" ] || die "Release 里没有匹配 $PATTERN 的资产（$API）"

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
esac

hash -r 2>/dev/null || true
command -v sigflow-cli >/dev/null || die "安装完成但 PATH 里找不到 sigflow-cli"
echo "✓ $(sigflow-cli --version 2>/dev/null || echo sigflow-cli) 已就绪"
