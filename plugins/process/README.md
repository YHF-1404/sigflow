# Process plugins

This directory hosts subprocess-style plugins. They are loaded via
`ProcessRuntime` rather than dlopen: the shell spawns `command [script]
--shm-path=...`, exchanges JSON-line control messages over stdin/stdout
(protocol `sigflow-plugin-v1`), and moves bulk sample data through shared
memory.

- `rust-counter` — minimal Rust binary plugin (via `sdk/rust`).
- `usb-adc` — USB ISO ADC acquisition source (Rust binary).
- `knock-localizer` — knock localization from two vibration channels
  (Python, via `sdk/python`; the reference Python plugin).
- `sdc-usb` — SDC gateway acquisition source: local multichannel ADC stream +
  remote pen scope windows + USB time sync, one frozen node→host clock map
  for both output ports (Python + pyusb).

A process plugin's `build.sh` stages `pkg/` (manifest + executable/scripts)
for `sigflow-cli plugin install`.
