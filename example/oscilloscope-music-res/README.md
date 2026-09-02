# 示波器音乐的素材放这儿

`example/oscilloscope-music.sh` 默认从这个目录读
`primer-final.wav`。**素材本身不在仓里**（同目录 `.gitignore` 挡着音频文件），
因为那是别人的作品，而本仓是公开的 MIT 仓——一个作品文件躺在里面，默认读法就是
"它也是 MIT"，而我们没有资格替作者做这个声明。

## 默认那一份：Primer

> **BUS ERROR Collective** — DJ_Level_3 与 Marv1994
> 2025-04-20 发布于 Revision 2025（Saarbrücken），Wild 单元第一名 + Crowd Favorite
> 另获 Meteoriks 2026 New Talent
>
> - Demozoo：<https://demozoo.org/productions/371249/>
> - pouët：<https://www.pouet.net/prod.php?which=103998>
> - 官方发布：scene.org

去发布页下载，转成 WAV 放成 `primer-final.wav`：

```sh
ffmpeg -i primer.flac example/oscilloscope-music-res/primer-final.wav
```

（源插件只吃 WAV——WAV 解析零依赖，FLAC 不是。只收 **stereo**：X-Y 要两路不同的
信号，单声道画出来是一条对角线。）

## 换成自己的素材

```sh
WAV=~/我的.wav sh example/oscilloscope-music.sh
```

脚本不依赖这一份。**任何 stereo 录音都放得出来，只是别人未必是画。**

## 一件跟采样率有关的，选素材时值得知道

官方那份 Primer 是 **192 kHz**。X-Y 要求一屏 `< 2 × px` 拍（默认 `span_scans = 1024`
永远安全），但**余辉**要盖满得让一屏 ≈ 刷新周期——两者同时成立要
`fs < 2 · px · refresh`（px = 1024、30 Hz 时约 61 kHz）。所以：

| 素材 | 覆盖率上限 |
|---|---|
| 48 kHz | 100% |
| 96 kHz | 64% |
| 192 kHz | **32%** |

**高采样率的素材拖影本来就淡，而且不能靠调大一屏去救**——越过配对线之后 X-Y 直接
不画，拿不到轨迹比拿到一条淡的轨迹坏得多。想要拖影浓一点，用 48 kHz 的版本。
