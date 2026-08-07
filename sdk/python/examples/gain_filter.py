#!/usr/bin/env python3
"""Example sigflow plugin: a simple gain filter.

Reads i16 PCM audio samples from the "in" port, multiplies each sample
by a configurable gain factor, clamps to the i16 range, and writes the
result to the "out" port.  The input frame's header is passed through so
downstream nodes keep the original timing (t0_ns / sample_index).

The shell spawns this automatically based on the manifest's runtime config:

    python3 gain_filter.py --shm-path=/tmp/sigflow_shm_xxx
"""

import struct

from sigflow_sdk import Frame, FrameOut, Plugin, run


class GainFilter(Plugin):
    def __init__(self):
        self.gain: float = 1.0

    @staticmethod
    def manifest() -> dict:
        return {
            "name": "example.dsp.gain",
            "version": "0.1.0",
            "category": "compute",
            "runtime": {
                "type": "process",
                "command": "python3",
                "script": "gain_filter.py",
                "protocol": "sigflow-plugin-v1",
            },
            "ports": [
                {
                    "id": "in",
                    "direction": "consumer",
                    "semantic_type": {"kind": "any"},
                },
                {
                    "id": "out",
                    "direction": "producer",
                    "semantic_type": {"kind": "any"},
                },
            ],
            "parameters": [
                {
                    "id": "gain",
                    "param_type": "f64",
                    "range": {"f64_range": {"min": 0.0, "max": 10.0}},
                    "default": {"f64": 1.0},
                    "hot_change": True,
                },
            ],
            "actions": [],
        }

    def setup(self, params: dict) -> None:
        self.gain = params.get("gain", 1.0)

    def process(self, inputs: dict[str, Frame]) -> dict[str, FrameOut]:
        frame = inputs.get("in")
        if frame is None or not frame.data:
            return {}

        data = frame.samples()
        num_samples = len(data) // 2
        samples = struct.unpack(f"<{num_samples}h", data[: num_samples * 2])
        gained = [
            max(-32768, min(32767, int(s * self.gain))) for s in samples
        ]
        output = struct.pack(f"<{num_samples}h", *gained)

        # Pass the input header through: same t0/sample_index, new payload.
        return {"out": FrameOut(header=frame.header, data=output)}

    def on_param_change(self, name: str, value) -> None:
        if name == "gain":
            self.gain = float(value)


if __name__ == "__main__":
    run(GainFilter)
