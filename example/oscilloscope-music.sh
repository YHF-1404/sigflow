#!/bin/sh
# example/oscilloscope-music.sh —— 示波器音乐：两路音频驱动 X-Y，屏上是画不是波形。
# requires-native: wav-source
#
# 一个节点、一个口、一台整页示波器。放什么由你给：
#
#   Primer —— BUS ERROR Collective（DJ_Level_3 与 Marv1994）
#   Revision 2025 Wild 第一名 + Crowd Favorite（2025-04-20，Saarbrücken）
#   另获 Meteoriks 2026 New Talent
#     Demozoo  https://demozoo.org/productions/371249/
#     pouët    https://www.pouet.net/prod.php?which=103998
#     官方发布在 scene.org（FLAC）
#
# **作品文件不在这个仓里**，也不该在——那是别人的作品。自己去上面的发布页下载。
# 官方发的是 FLAC，这个源只吃 WAV，转一下：
#
#   ffmpeg -i primer.flac primer.wav
#
# 然后：  WAV=~/primer.wav sh example/oscilloscope-music.sh
#
# 换任何一段 stereo 录音都行，只是别人未必是画。
#
# 前提：sigflow-cli 在 PATH，wav-source 已 `sigflow-cli plugin install`。
# POSIX sh；参数走环境变量：
#   WAV       WAV 文件路径（必给）
#   NODE_DIR  节点目录        （默认 /tmp/sigflow-osc-music）
#   SPAN      一屏多少拍      （默认 1024，别往大调，理由见下）
set -eu

WAV="${WAV:-}"
NODE_DIR="${NODE_DIR:-/tmp/sigflow-osc-music}"
SPAN="${SPAN:-1024}"
export SIGFLOW_ROOT_PORT="${SIGFLOW_ROOT_PORT:-9500}"

step() { printf '\n\033[1;36m=== %s\033[0m\n' "$*"; }

[ -n "$WAV" ] || { printf '\033[1;31m要给 WAV=<stereo wav 的路径>\033[0m\n（官方发的是 FLAC：ffmpeg -i primer.flac primer.wav）\n' >&2; exit 1; }
[ -f "$WAV" ] || { printf '\033[1;31m没有这个文件：%s\033[0m\n' "$WAV" >&2; exit 1; }
case "$WAV" in /*) ;; *) WAV="$(pwd)/$WAV" ;; esac   # 节点在别的 cwd 里跑

step "重建 $NODE_DIR"
rm -rf "$NODE_DIR"
sigflow-cli start "$NODE_DIR"
sigflow-cli add music
sigflow-cli --node music plugin set sigflow.io.wav_source@0.1.0 -y
sigflow-cli --node music param set path "$WAV"

# ---- 示波器 setup ------------------------------------------------------------
# clock = native：原样进环，X-Y 画的才是**真轨迹**。重采样（auto / fixed）也建得
# 起来（口的率从第一帧的 sfrate01 注解来），但那会插值——李萨如图上插值出来的过冲
# 是画上没有的线。
#
# **一屏用 span_scans 不用 span_s，这是这份 setup 里最要紧的一行**：
# X-Y 靠"同一拍"成立——视图只在每像素列不足两拍时发原始样本（span < 2 × px），
# 超过就退化成每列的 (min, max)，那时 x[i] 与 y[i] 是各自窗口里的极值、根本不是
# 同一拍，连出来的不是轨迹（sigflow docs/scope-contract.md §13.2）。
# 写 span_scans = 1024 是**跟文件采样率无关**的：任何文件都是 1024 拍，永远在线
# 下面。写成时间就不是了——20 ms 在 48 kHz 上是 960 拍（安全），在 96 kHz 上是
# 1920 拍、在 192 kHz 上直接越线。**这条是"别把一个具体采样率下的数当通则"。**
#
# 两轴 v_div 取同一个值：X-Y 画在正方形里、两轴都是 8 格，同档才是圆不是椭圆。
mkdir -p "$NODE_DIR/scope"
cat > "$NODE_DIR/scope/xy.toml" <<EOT
channels = [{ column = "l" }, { column = "r" }]
clock = { mode = "native" }
depth = { max_points = 8000000 }
timebase = { span_scans = $SPAN, position = 0.5 }

[display]
mode = "xy"
x = { column = "l" }
y = { column = "r" }

[[vertical]]
channel = { column = "l" }
v_div = 0.25
[[vertical]]
channel = { column = "r" }
v_div = 0.25

[acq]
mode = "normal"
EOT

step "整页示波器（X-Y）"
sigflow-cli --node music widget add sigflow.ui.oscilloscope@0.1.0 xy \
    --layout "8,8,384,40" --config "label=示波器音乐（X-Y）"
sigflow-cli --node music widget bind xy port:audio --scope-file "$NODE_DIR/scope/xy.toml"

# **节点在画布上的显示密度**——不设这一行，浏览器里只有一张 node 卡片，整页控件的
# 入口根本不出现，看起来像"控件没装上"。整页控件必须配 `--display face`。
# （`--pos` 是必填的；`--size` 是脸的尺寸，给大一点，X-Y 画在正方形里。）
sigflow-cli layout music --pos 20,20 --size 1260,840 --display face

step "开播"
sigflow-cli --node music processing start

printf '\n  打开 webui：\033[1mhttp://127.0.0.1:%s/\033[0m —— 进 music 节点，点开「示波器音乐」那一页。\n' "$SIGFLOW_ROOT_PORT"
cat <<'TXT'

  看不到东西时，先分清是**看不见页面**还是**看不见波形**——两件事，别混着查：

    0. 浏览器里只有一张 node 卡片、压根没有页面入口 → 节点的显示密度没设成 face。
       脚本里那行 `sigflow-cli layout music … --display face` 就是干这个的；
       手搭图时最容易漏，而漏了之后看起来像"控件没装上"，会往完全错的方向查。
    1. 有入口、点进去没波形，状态行写「在等口出第一帧」→ **还没开播**。这个口不
       声明采样率（文件的率是运行期才知道的），率是第一帧带上来的，所以开播之前
       它确实不知道自己该多快。这不是坏了。
    2. 画太小 → 拧 gain，或者把两轴的 v_div 一起调小。
       **两轴要一起**，只调一个会把圆压成椭圆。
    3. 一团糊、不像画 → 一屏太长了。X-Y 只在一屏拿得到原始样本时才是轨迹；
       把 SPAN 调回 1024。

  能拧的（照抄就能跑）：
    sigflow-cli --node music param set gain 2          画太小就往上拧（超过 1 会削顶）
    sigflow-cli --node music param set speed 0.25      慢放看轨迹怎么走出来的
    sigflow-cli --node music param set loop_play false 放完就停，不循环
    sigflow-cli --node music param set loop_play true  停了之后打开循环 = 接着放
    sigflow-cli --node music action invoke restart     从头播

  没有余辉——真机的拖影是示波器音乐好看的一半，我们还没做（X-Y 平面上要另开一张
  累积网格）。所以现在看到的是单帧线条，比真机干净、也比真机冷清。

TXT
