#!/bin/sh
# new-plugin.sh — 生成一个空白 sigflow 插件工程。
#
# 用法：
#   tools/new-plugin.sh --lang rust   --runtime native  --name my-filter  ~/dev/my-filter
#   tools/new-plugin.sh --lang rust   --runtime process --name my-source  ~/dev/my-source
#   tools/new-plugin.sh --lang python                   --name my-py-node ~/dev/my-py-node
#
# 选项：
#   --lang rust|python     必选。python 恒为 process 运行时。
#   --runtime native|process   rust 默认 native。
#   --name <kebab-name>    必选，[a-z0-9-]，作为目录/crate 名。
#   --id <manifest-id>     manifest 身份名（默认 my.plugins.<snake>）。
#   --path-sdk             SDK 依赖用本仓库的本地路径（默认：git 依赖钉住
#                          本仓库当前 tag；无 tag 时 branch = "main"）。
#   <dest-dir>             必选，生成位置（不存在或为空目录）。
#
# 生成后：cd <dest> && ./build.sh && sigflow-cli plugin install pkg
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() { echo "new-plugin.sh: $*" >&2; exit 1; }
usage() { sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

LANG_=""
RUNTIME=""
NAME=""
ID=""
PATH_SDK=0
DEST=""
while [ $# -gt 0 ]; do
    case "$1" in
        --lang)      LANG_="$2"; shift 2 ;;
        --lang=*)    LANG_="${1#--lang=}"; shift ;;
        --runtime)   RUNTIME="$2"; shift 2 ;;
        --runtime=*) RUNTIME="${1#--runtime=}"; shift ;;
        --name)      NAME="$2"; shift 2 ;;
        --name=*)    NAME="${1#--name=}"; shift ;;
        --id)        ID="$2"; shift 2 ;;
        --id=*)      ID="${1#--id=}"; shift ;;
        --path-sdk)  PATH_SDK=1; shift ;;
        -h|--help)   usage 0 ;;
        -*)          die "未知参数 '$1'（--help 看用法）" ;;
        *)           [ -z "$DEST" ] || die "多余的位置参数 '$1'"
                     DEST="$1"; shift ;;
    esac
done

[ -n "$LANG_" ] || { usage 1; }
[ -n "$NAME" ]  || die "缺 --name"
[ -n "$DEST" ]  || die "缺目标目录"
case "$NAME" in
    *[!a-z0-9-]*|-*|*-) die "--name 需为 kebab-case（[a-z0-9-]，不以 - 开头/结尾）" ;;
esac

case "$LANG_" in
    rust)   RUNTIME="${RUNTIME:-native}"
            case "$RUNTIME" in native|process) ;; *) die "--runtime 需为 native|process" ;; esac
            TPL="rust-$RUNTIME" ;;
    python) [ -z "$RUNTIME" ] || [ "$RUNTIME" = process ] || die "python 只有 process 运行时"
            TPL="python-process" ;;
    *)      die "--lang 需为 rust|python" ;;
esac

SNAKE="$(echo "$NAME" | tr '-' '_')"
STRUCT="$(echo "$NAME" | awk -F- '{ for (i = 1; i <= NF; i++) printf "%s%s", toupper(substr($i,1,1)), substr($i,2) }')"
[ -n "$ID" ] || ID="my.plugins.$SNAKE"

# SDK 依赖行（整行替换模板里的 sigflow-plugin-sdk = ... 行）
if [ "$PATH_SDK" = 1 ]; then
    SDK_DEP="sigflow-plugin-sdk = { path = \"$REPO_ROOT/sdk/rust\" }"
else
    TAG="$(git -C "$REPO_ROOT" describe --tags --abbrev=0 2>/dev/null || true)"
    if [ -n "$TAG" ]; then
        SDK_DEP="sigflow-plugin-sdk = { git = \"https://github.com/YHF-1404/sigflow.git\", tag = \"$TAG\" }"
    else
        SDK_DEP="sigflow-plugin-sdk = { git = \"https://github.com/YHF-1404/sigflow.git\", branch = \"main\" }"
    fi
fi

# 目标目录：不存在或为空
if [ -e "$DEST" ]; then
    [ -d "$DEST" ] || die "$DEST 已存在且不是目录"
    [ -z "$(ls -A "$DEST")" ] || die "$DEST 非空"
fi
mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd)"

cp -R "$SCRIPT_DIR/templates/$TPL/." "$DEST/"

# 占位符替换（sed 不用 -i，保持 GNU/BSD 双兼容）
find "$DEST" -type f | while IFS= read -r f; do
    sed -e "s|__PLUGIN_NAME__|$NAME|g" \
        -e "s|__PLUGIN_SNAKE__|$SNAKE|g" \
        -e "s|__PLUGIN_STRUCT__|$STRUCT|g" \
        -e "s|__PLUGIN_ID__|$ID|g" \
        -e "s|^sigflow-plugin-sdk = .*|$SDK_DEP|" \
        "$f" > "$f.tmp" && mv "$f.tmp" "$f"
done
chmod +x "$DEST/build.sh"

# python：vendor SDK 进工程（build.sh 从这里取，SIGFLOW_PY_SDK 可覆盖）
if [ "$TPL" = python-process ]; then
    mkdir -p "$DEST/sigflow_sdk"
    cp "$REPO_ROOT"/sdk/python/sigflow_sdk/*.py "$DEST/sigflow_sdk/"
fi

cat <<EOF
生成完毕：${DEST}（${TPL}，manifest id = ${ID}@0.1.0）

下一步：
  cd ${DEST}
  ./build.sh                        # 构建并 stage 到 pkg/
  sigflow-cli plugin install pkg    # 入插件仓库
  sigflow-cli --node <节点> plugin set $ID@0.1.0 -y
EOF
