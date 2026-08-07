"""Shell-side protocol test for the SDK runtime loop.

Spawns the gain_filter example as a real subprocess and drives it exactly the
way `sigflow-shell`'s ProcessRuntime does (see process_runtime.rs):

    spawn(cmd script --shm-path=...) → ready → init → initialized
    → start → started → process(in_headers/in_latencies) → processed
    (out_headers/health) → stop → stopped → shutdown → shutdown_complete

This locks the Python SDK to the sigflow-plugin-v1 wire contract.
"""

import json
import mmap
import os
import struct
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from test_shm import _create_shm_file  # noqa: E402
from sigflow_sdk._shm import HEADER_SIZE, PORT_ENTRY_SIZE  # noqa: E402

SDK_ROOT = os.path.join(os.path.dirname(__file__), "..")
GAIN_SCRIPT = os.path.join(SDK_ROOT, "examples", "gain_filter.py")


class ShellSide:
    """Minimal stand-in for the shell's ProcessRuntime."""

    def __init__(self, script: str, num_inputs: int, num_outputs: int, capacity: int = 4096):
        fd, self.shm_path = tempfile.mkstemp(suffix=".shm")
        os.close(fd)
        _create_shm_file(self.shm_path, num_inputs, num_outputs, capacity)
        self.num_inputs = num_inputs
        self.capacity = capacity

        env = dict(os.environ, PYTHONPATH=SDK_ROOT)
        self.child = subprocess.Popen(
            [sys.executable, script, f"--shm-path={self.shm_path}"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,
            env=env,
            text=True,
        )

    def send(self, msg: dict) -> None:
        self.child.stdin.write(json.dumps(msg) + "\n")
        self.child.stdin.flush()

    def recv(self, timeout: float = 10.0) -> dict:
        # The child answers promptly; a plain blocking readline suffices here
        # (pytest's own timeout catches a hung child).
        line = self.child.stdout.readline()
        assert line, "child closed stdout unexpectedly"
        return json.loads(line)

    def write_input(self, index: int, data: bytes) -> None:
        with open(self.shm_path, "r+b") as f:
            mm = mmap.mmap(f.fileno(), 0)
            entry = HEADER_SIZE + index * PORT_ENTRY_SIZE
            offset = struct.unpack_from("<Q", mm, entry)[0]
            mm[offset : offset + len(data)] = data
            struct.pack_into("<Q", mm, entry + 16, len(data))
            mm.flush()
            mm.close()

    def read_output(self, index: int) -> bytes:
        with open(self.shm_path, "r+b") as f:
            mm = mmap.mmap(f.fileno(), 0)
            entry = HEADER_SIZE + (self.num_inputs + index) * PORT_ENTRY_SIZE
            offset = struct.unpack_from("<Q", mm, entry)[0]
            size = struct.unpack_from("<Q", mm, entry + 16)[0]
            out = bytes(mm[offset : offset + size])
            mm.close()
            return out

    def close(self) -> None:
        try:
            self.child.kill()
            self.child.wait(timeout=5)
        except Exception:
            pass
        os.unlink(self.shm_path)


def test_full_lifecycle_protocol():
    shell = ShellSide(GAIN_SCRIPT, num_inputs=1, num_outputs=1)
    try:
        # ready + manifest
        ready = shell.recv()
        assert ready["method"] == "ready"
        assert ready["params"]["protocol"] == "sigflow-plugin-v1"
        assert ready["params"]["manifest"]["name"] == "example.dsp.gain"

        # init with ParamValue-encoded defaults → initialized
        shell.send({"method": "init", "params": {"params": {"gain": {"f64": 2.0}}}})
        assert shell.recv()["method"] == "initialized"

        # start → started with ok health
        shell.send({"method": "start"})
        started = shell.recv()
        assert started["method"] == "started"
        assert started["params"]["health"] == {"status": "ok"}

        # process: 4 i16 samples ×2 gain, header passthrough
        samples = struct.pack("<4h", 100, -200, 300, -400)
        shell.write_input(0, samples)
        header = {
            "seq": 7,
            "sample_index": 4096,
            "t0_ns": 123456789,
            "tx_egress_mono_ns": 0,
            "gap_samples": 0,
            "n_samples": 4,
            "flags": 1,
            "annotation_len": 0,
        }
        shell.send(
            {
                "method": "process",
                "params": {"seq": 1, "in_headers": [header], "in_latencies": [5000]},
            }
        )
        processed = shell.recv()
        assert processed["method"] == "processed"
        assert processed["params"]["health"] == {"status": "ok"}
        out_h = processed["params"]["out_headers"][0]
        assert out_h["sample_index"] == 4096
        assert out_h["t0_ns"] == 123456789
        assert out_h["flags"] == 1

        out = struct.unpack("<4h", shell.read_output(0))
        assert out == (200, -400, 600, -800)

        # set_param (fire and forget) then verify it took effect
        shell.send(
            {"method": "set_param", "params": {"id": "gain", "value": {"f64": 3.0}}}
        )
        shell.write_input(0, struct.pack("<1h", 10))
        shell.send(
            {
                "method": "process",
                "params": {
                    "seq": 2,
                    "in_headers": [dict(header, n_samples=1)],
                    "in_latencies": [0],
                },
            }
        )
        assert shell.recv()["method"] == "processed"
        assert struct.unpack("<1h", shell.read_output(0)) == (30,)

        # stop → stopped
        shell.send({"method": "stop"})
        assert shell.recv()["method"] == "stopped"

        # shutdown → shutdown_complete + clean exit
        shell.send({"method": "shutdown"})
        assert shell.recv()["method"] == "shutdown_complete"
        assert shell.child.wait(timeout=5) == 0
    finally:
        shell.close()


def test_process_exception_reports_fault_and_survives():
    """A crashing process() must report fault health, not kill the loop."""
    # A tiny inline plugin whose process() raises on empty input.
    script = os.path.join(tempfile.gettempdir(), "sigflow_test_faulty_plugin.py")
    with open(script, "w") as f:
        f.write(
            '''
import sys, os
sys.path.insert(0, os.environ["SIGFLOW_SDK_ROOT"])
from sigflow_sdk import Frame, FrameOut, Plugin, PluginFault, run

class Faulty(Plugin):
    @staticmethod
    def manifest():
        return {
            "name": "test.faulty", "version": "0.0.1", "category": "compute",
            "runtime": {"type": "process", "command": "python3",
                        "script": "x.py", "protocol": "sigflow-plugin-v1"},
            "ports": [
                {"id": "in", "direction": "consumer", "semantic_type": {"kind": "any"}},
                {"id": "out", "direction": "producer", "semantic_type": {"kind": "any"}},
            ],
            "parameters": [], "actions": [],
        }

    def process(self, inputs):
        frame = inputs["in"]
        if not frame.data:
            raise PluginFault("empty input")
        return {"out": FrameOut(header=frame.header, data=frame.data)}

if __name__ == "__main__":
    run(Faulty)
'''
        )

    fd, shm_path = tempfile.mkstemp(suffix=".shm")
    os.close(fd)
    _create_shm_file(shm_path, 1, 1, 4096)
    env = dict(os.environ, SIGFLOW_SDK_ROOT=SDK_ROOT)
    child = subprocess.Popen(
        [sys.executable, script, f"--shm-path={shm_path}"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        env=env,
        text=True,
    )
    try:
        def send(m):
            child.stdin.write(json.dumps(m) + "\n")
            child.stdin.flush()

        def recv():
            return json.loads(child.stdout.readline())

        assert recv()["method"] == "ready"
        send({"method": "init", "params": {"params": {}}})
        assert recv()["method"] == "initialized"

        # Empty input → PluginFault → fault health, but the loop survives.
        send({"method": "process", "params": {"seq": 1, "in_headers": [{}], "in_latencies": [0]}})
        resp = recv()
        assert resp["params"]["health"] == {"status": "fault", "reason": "empty input"}
        # Unemitted output ⇒ zero header (shell skips publishing it).
        assert resp["params"]["out_headers"][0]["n_samples"] == 0

        # Next tick still works.
        send({"method": "shutdown"})
        assert recv()["method"] == "shutdown_complete"
        assert child.wait(timeout=5) == 0
    finally:
        try:
            child.kill()
        except Exception:
            pass
        os.unlink(shm_path)
        os.unlink(script)
