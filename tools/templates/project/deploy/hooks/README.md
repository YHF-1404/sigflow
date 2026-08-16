# deploy/hooks/ — 项目部署钩子（都可缺省）

- `precheck.sh` — 装完核心后在部署目标机上被引擎 **source**（本地与
  --phase remote 两路）。可用引擎函数 `note/die/priv`、`$HOOK_OS`
  （arch|debian|macos|windows|linux；windows 本地部署无 sudo，钩子里
  别用 `priv`）；按 `NATIVE_PLUGINS/PROCESS_PLUGINS`
  （本地）或装船目录 `plugins-*/`（远端）自行决定要不要干活。
  参考：dualpen 的 SDC pyusb/udev 预检。
- `adb-pre-install.sh` — 独立 POSIX sh，推到 Android 设备上、装包前
  执行（清场项目残留进程、设备调优等）。失败不阻断部署。
