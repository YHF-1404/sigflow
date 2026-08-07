#!/usr/bin/env python3
"""__PLUGIN_NAME__ — sigflow Python 子进程插件骨架：透传 + 参数示例。

契约在旁边的 manifest.toml（单一真源，运行时用 tomllib 读入上报）。
壳体按 manifest 的 runtime 配置经 run.sh 拉起本脚本。
"""

import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, _HERE)  # 同目录的 vendored sigflow_sdk/

from sigflow_sdk import Frame, FrameOut, Plugin, run


class __PLUGIN_STRUCT__(Plugin):
    def __init__(self):
        self.gain = 1.0

    # ---- manifest：读旁边的 manifest.toml，不在代码里重复声明 ----
    @staticmethod
    def manifest() -> dict:
        import tomllib
        with open(os.path.join(_HERE, "manifest.toml"), "rb") as fh:
            return tomllib.load(fh)

    def setup(self, params: dict) -> None:
        self.gain = float(params.get("gain", 1.0))

    def process(self, inputs):
        frame = inputs.get("in")
        if frame is None or not frame.data:
            return {}
        # 骨架：原样透传（帧头带走 t0/sample_index 时间语义）。
        # 在这里替换成你的处理逻辑。
        return {"out": FrameOut(header=frame.header, data=frame.data)}

    def on_param_change(self, name, value) -> None:
        if name == "gain":
            self.gain = float(value)


if __name__ == "__main__":
    run(__PLUGIN_STRUCT__)
