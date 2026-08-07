#!/bin/sh
# new-project.sh — 生成一个 sigflow 项目仓库骨架（dualpen 形态）。
#
# 用法：
#   tools/new-project.sh [--name <kebab>] [--plugin <kebab>] <dest-dir>
#
#   --name     项目名（默认 = dest 目录名，[a-z0-9-]）
#   --plugin   顺手播一个 native 插件（进 workspace + [patch] 项目形态）
#              并生成 example/starter.sh（sine → 插件 → mon，生成即可部署）
#
# 生成内容：Cargo.toml（workspace + 兄弟 [patch]）、Cross.toml、
# redeploy.sh 部署 wrapper、deploy/hooks/、plugins/、example/、git init。
# 兄弟目录约定：本仓库（sigflow-public）与新项目并排；核心源码
# （../sigflow-core）可选。
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() { echo "new-project.sh: $*" >&2; exit 1; }

NAME=""
PLUGIN=""
DEST=""
while [ $# -gt 0 ]; do
    case "$1" in
        --name)     NAME="$2"; shift 2 ;;
        --name=*)   NAME="${1#--name=}"; shift ;;
        --plugin)   PLUGIN="$2"; shift 2 ;;
        --plugin=*) PLUGIN="${1#--plugin=}"; shift ;;
        -h|--help)  sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*)         die "未知参数 '$1'（--help 看用法）" ;;
        *)          [ -z "$DEST" ] || die "多余的位置参数 '$1'"
                    DEST="$1"; shift ;;
    esac
done
[ -n "$DEST" ] || die "缺目标目录（--help 看用法）"
[ -n "$NAME" ] || NAME="$(basename "$DEST")"
case "$NAME" in
    *[!a-z0-9-]*|-*|*-) die "项目名需为 kebab-case（[a-z0-9-]）：${NAME}" ;;
esac

if [ -e "$DEST" ]; then
    [ -d "$DEST" ] || die "${DEST} 已存在且不是目录"
    [ -z "$(ls -A "$DEST")" ] || die "${DEST} 非空"
fi
mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd)"

cp -R "$SCRIPT_DIR/templates/project/." "$DEST/"
mkdir -p "$DEST/plugins/native" "$DEST/plugins/process" "$DEST/example" "$DEST/deploy/hooks"

# 占位符替换（sed 不用 -i，GNU/BSD 双兼容）
find "$DEST" -type f | while IFS= read -r f; do
    sed "s|__PROJECT_NAME__|$NAME|g" "$f" > "$f.tmp" && mv "$f.tmp" "$f"
done
chmod +x "$DEST/redeploy.sh"

# ---- 可选：播一个 native 插件 + starter 图例 --------------------------------
if [ -n "$PLUGIN" ]; then
    "$SCRIPT_DIR/new-plugin.sh" --lang rust --runtime native \
        --name "$PLUGIN" "$DEST/plugins/native/$PLUGIN" >/dev/null
    PSNAKE="$(echo "$PLUGIN" | tr '-' '_')"
    PID="my.plugins.$PSNAKE"
    # SDK 依赖改项目形态：git branch=main，经根 [patch] 走兄弟 checkout
    sed 's|^sigflow-plugin-sdk = .*|sigflow-plugin-sdk = { git = "https://github.com/YHF-1404/sigflow.git", branch = "main" }|' \
        "$DEST/plugins/native/$PLUGIN/Cargo.toml" > "$DEST/.t" && mv "$DEST/.t" "$DEST/plugins/native/$PLUGIN/Cargo.toml"
    # 进 workspace（awk：BSD sed 替换串不认 \n）
    awk -v m="    \"plugins/native/$PLUGIN\"," \
        '/^members = \[/ { print; print m; next } { print }' \
        "$DEST/Cargo.toml" > "$DEST/.t" && mv "$DEST/.t" "$DEST/Cargo.toml"
    # starter 图例
    sed -e "s|__PROJECT_NAME__|$NAME|g" -e "s|__PLUGIN_NAME__|$PLUGIN|g" \
        -e "s|__PLUGIN_ID__|$PID|g" \
        "$SCRIPT_DIR/templates/starter-graph.sh" > "$DEST/example/starter.sh"
    chmod +x "$DEST/example/starter.sh"
fi

( cd "$DEST" && git init -q -b main && git add -A \
    && git commit -q -m "init: ${NAME} —— sigflow 项目骨架（new-project.sh 生成）" )

cat <<EOF
生成完毕：${DEST}（项目 ${NAME}${PLUGIN:+，含插件 ${PLUGIN}}）

下一步：
  cd ${DEST}
  cargo build                       # 经 [patch] 读兄弟 ../sigflow-public
${PLUGIN:+  ./redeploy.sh starter             # 部署起步图例（sine → ${PLUGIN} → mon）
}  ./redeploy.sh hello               # 或公共入门图例
  # 项目参数与转发清单改 redeploy.sh；设备预检/清场钩子见 deploy/hooks/
  # 记得给仓库配 remote 备份
EOF
