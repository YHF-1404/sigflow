#!/bin/sh
# example/oscilloscope-music.sh —— 示波器音乐：两路音频驱动 X-Y，屏上是画不是波形。
# requires-native: wav-source
# forwards: WAV SPAN
#
# 一个节点、一个口、一台整页示波器。默认放的是这个：
#
#   Primer —— BUS ERROR Collective（DJ_Level_3 与 Marv1994）
#   Revision 2025 Wild 第一名 + Crowd Favorite（2025-04-20，Saarbrücken）
#   另获 Meteoriks 2026 New Talent
#     Demozoo  https://demozoo.org/productions/371249/
#     pouët    https://www.pouet.net/prod.php?which=103998
#     官方发布在 scene.org
#
# **作品文件不在这个仓里**，也不该在——那是别人的作品，而本仓是公开的 MIT 仓。
# 自己去上面的发布页下载，转成 WAV 放到脚本默认找的位置：
#
#   ffmpeg -i primer.flac example/oscilloscope-music-res/primer-final.wav
#
# 那个目录的 .gitignore 挡着音频文件，怎么拿见它的 README.md。
# 要换别的：WAV=<你的.wav> sh example/oscilloscope-music.sh
# 换任何一段 stereo 录音都行，只是别人未必是画。
#
# 远程部署（deploy/redeploy.sh --host）：船上只有这个脚本，素材得先自己放到
# 目标机上；WAV 写的是**目标机上**的路径，引擎按上面 `forwards:` 那行把它转发
# 过去。Windows 目标写 Git Bash 形式（/c/Users/…），不是 WSL 的 /mnt/c/…：
#   WAV=/c/Users/me/primer-final.wav ./deploy/redeploy.sh oscilloscope-music --host H --passwd P --user U
#
# 前提：sigflow-cli 在 PATH，wav-source 已 `sigflow-cli plugin install`；脸上的播放条
# 和音量滑块要两个控件包（没装就跳过，示波器照常）：
#   sigflow-cli plugin install plugins/ui/transport
#   sigflow-cli plugin install plugins/ui/slider
# 声音走系统默认输出设备（cpal）。没声卡的机器（远程部署的船上常见）画面照常，
# 播放条上写"设备打不开"——那是 transport 口 audio 列的出口，不是坏了。
# POSIX sh；参数走环境变量：
#   WAV       WAV 文件路径    （默认 example/oscilloscope-music-res/primer-final.wav）
#   NODE_DIR  节点目录        （默认 /tmp/sigflow-osc-music）
#   SPAN      一屏多少拍      （默认 1024，别往大调，理由见下）
set -eu

# 默认找 res 目录里那份（**不在仓里**，自己放）；脚本可能从任何 cwd 跑，按自身位置定位。
_here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
WAV="${WAV:-$_here/oscilloscope-music-res/primer-final.wav}"
NODE_DIR="${NODE_DIR:-/tmp/sigflow-osc-music}"
SPAN="${SPAN:-1024}"
export SIGFLOW_ROOT_PORT="${SIGFLOW_ROOT_PORT:-9500}"

step() { printf '\n\033[1;36m=== %s\033[0m\n' "$*"; }

[ -n "$WAV" ] || { printf '\033[1;31m要给 WAV=<stereo wav 的路径>\033[0m\n' >&2; exit 1; }
if [ ! -f "$WAV" ]; then
    # 本地 checkout 指 res 目录里的 README；部署船上只有这个脚本（素材目录
    # 不上船），就指回仓库里那份——指一个这台机器上不存在的文件等于没指。
    _res_doc="$_here/oscilloscope-music-res/README.md"
    [ -f "$_res_doc" ] || _res_doc="仓库里的 example/oscilloscope-music-res/README.md（这里没有 checkout，只有脚本自己）"
    printf '\033[1;31m没有这个文件：%s\033[0m\n' "$WAV" >&2
    printf '素材不进仓（那是别人的作品），得自己放到这台机器上；怎么拿见 %s\n' "$_res_doc" >&2
    printf '指定位置：WAV=<这台机器上的 stereo wav> sh %s\n' "$0" >&2
    printf '远程部署时在发起端写：WAV=<目标机上的路径> ./deploy/redeploy.sh oscilloscope-music --host …（Windows 目标用 /c/… 写法）\n' >&2
    exit 1
fi
# 节点在别的 cwd 里跑，给成绝对路径。Windows 本地给 C:/… 也已经是绝对的——
# 只认 /* 会把它拼成 "$(pwd)/C:/…"，而且 -f 在前面已经过了，坏在口里才露。
case "$WAV" in /*|[A-Za-z]:*) ;; *) WAV="$(pwd)/$WAV" ;; esac

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
# **官方那份 Primer 正好是 192 kHz**，所以它就是那个反例本身：
#   span_scans = 1024  →  5.33 ms，X-Y 成立
#   span_s     = 21 ms →  4096 拍，2 倍越线，X-Y 当场不画
# 同一份 setup 拿 48 kHz 的素材试，两种写法都过——**只有换了率才分得出对错**。
#
# 一个跟着来的限制，看余辉时会撞上：**采样率越高，余辉能盖住的比例越低**。余辉累的
# 是推出去的帧，要盖满得让一屏 ≈ 刷新周期；而 X-Y 要求一屏短到还能发原始样本。两个
# 要求朝相反方向拉，采样率越高拉得越开。
#
# **具体到多少，看屏上**——状态行写着"余辉覆盖 N%"，「覆盖调到最大」的 tooltip 写着
# "最高能到 N%"。那是运行时按**这一帧的** px 和**这个源的** fs 算的；这里写死一个数
# 就是抄件，改个余量、换个画布宽度就漂（我写过一版 32%，实际 29%）。
#
# 量级感：音频那一档（48 kHz）够得到满，192 kHz 大约三成。官方那份 Primer 是
# 192 kHz，**所以它的拖影本来就比 48 kHz 的素材淡，这不是坏了**。
#
# **别为了盖满去手调大一屏**——越过配对线之后 X-Y 直接不画，拿不到轨迹比拿到一条淡
# 的轨迹坏得多。（那个按钮自己不会越线，它取两者的较小值。）
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

# ---- 脸上的播放条 + 音量 -------------------------------------------------------
# 示波器整页开在浮窗里，脸上的这几个一直看得见。播放条绑 **transport 口**（不是
# 参数）：进度从口读——位置 / 时长 / 播放态 / 出声态，20 Hz 一拍；原生插件的参数
# 只能被写、不能自己改，"现在放到哪了"只有口能带出来。拖完写 seek_s、▶/⏸ 写
# playing、⏮ 按 restart，都是 wav_source 的 id，控件的缺省就是它们。
step "播放条 + 音量"
if sigflow-cli --node music widget add sigflow.ui.transport@0.1.0 bar \
    --layout "8,56,384,52" 2>/dev/null; then
    sigflow-cli --node music widget bind bar port:transport
else
    printf '  （没装 sigflow.ui.transport，脸上没有播放条：sigflow-cli plugin install plugins/ui/transport）\n'
fi
if sigflow-cli --node music widget add sigflow.ui.slider@0.1.0 vol \
    --layout "8,116,384,44" --config "label=音量" 2>/dev/null; then
    sigflow-cli --node music widget bind vol param:volume
else
    printf '  （没装 sigflow.ui.slider，脸上没有音量滑块：sigflow-cli plugin install plugins/ui/slider）\n'
fi

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
    2. 图缩成一个点 → **先看是不是刚开播**。Primer 开头几秒近乎静音（实测峰值
       0.004，而全曲中位 0.37、最高 0.86），图就该是个点；等几秒它自己长开。
       真的一直小才去拧 gain，或者把两轴的 v_div 一起调小——**两轴要一起**，
       只调一个会把圆压成椭圆。
       （默认 v_div = 0.25 是按这份素材配的：峰值 0.86 时正好撑满不溢出。）
    3. 一团糊、不像画 → 一屏太长了。X-Y 只在一屏拿得到原始样本时才是轨迹；
       把 SPAN 调回 1024。

  能拧的（照抄就能跑；脸上的播放条 / 音量滑块拧的是同一批）：
    sigflow-cli --node music param set gain 2          画太小就往上拧（超过 1 会削顶）
    sigflow-cli --node music param set speed 0.25      慢放看轨迹怎么走出来的（音调跟着降）
    sigflow-cli --node music param set loop_play false 放完就停，不循环
    sigflow-cli --node music param set loop_play true  停了之后打开循环 = 接着放
    sigflow-cli --node music action invoke restart     从头播
    sigflow-cli --node music param set playing false   暂停（true 继续；放完之后 true = 从头播）
    sigflow-cli --node music param set seek_s 30       跳到第 30 秒（写入即跳；实时位置看 transport 口）
    sigflow-cli --node music param set volume 0.5      音量（只管声音，不动画面；gain 才动画面）
    sigflow-cli --node music param set audio false     不出声

  没声音：先看播放条右上角的出声态——"设备打不开"是 transport 口 audio 列报的，
  多半是这台机器没有默认输出设备（远程部署的船上常见）；画面照常，这不是坏了。
  节点日志里有设备名和率（"出声 → xxx（设备 48000 Hz，文件 192000 Hz）"）。

  没有余辉——真机的拖影是示波器音乐好看的一半，我们还没做（X-Y 平面上要另开一张
  累积网格）。所以现在看到的是单帧线条，比真机干净、也比真机冷清。

TXT
