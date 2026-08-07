"""IPC runtime loop for sigflow Python plugins.

This module implements the subprocess side of the sigflow plugin IPC
protocol (``sigflow-plugin-v1``), mirroring the Rust reference in
``sdk/rust/src/process.rs``:

    plugin → shell: ``ready`` (with manifest), ``initialized``, ``started``,
                    ``stopped``, ``processed``, ``shutdown_complete``
    shell → plugin: ``init``, ``set_param``, ``start``, ``process``, ``stop``,
                    ``invoke_action``, ``shutdown``

Bulk sample data travels through shared memory; per-frame ``FrameHeader``s
travel in the ``process`` / ``processed`` control messages.  The shell waits
at most ~2 s for each ack (``started`` / ``processed``), so ``start()`` and
``process()`` must return promptly; heavy one-time work belongs in
``setup()``.

stdout is the control channel — plugins must log to **stderr** only.

Plugin authors should never import this module directly; instead, call
``sigflow_sdk.run(MyPluginClass)`` from their ``__main__`` block.
"""

import json
import sys
import traceback
from typing import Type

from sigflow_sdk._shm import SharedMemory
from sigflow_sdk.frame import Frame, FrameHeader, FrameOut
from sigflow_sdk.plugin import Plugin, PluginFault


def _read_msg() -> dict:
    """Read one JSON-line message from stdin."""
    line = sys.stdin.readline()
    if not line:
        raise EOFError("shell closed stdin")
    return json.loads(line)


def _send_msg(msg: dict) -> None:
    """Write one JSON-line message to stdout and flush."""
    sys.stdout.write(json.dumps(msg, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def _extract_param_value(value) -> object:
    """Extract a plain Python value from the sigflow ParamValue JSON encoding.

    The Rust ``ParamValue`` enum serialises as ``{"f64": 1.0}`` etc.
    This helper unwraps that to a bare ``1.0``.  If the value is already
    a plain scalar (from a simplified sender), it is returned as-is.
    """
    if isinstance(value, dict) and len(value) == 1:
        return next(iter(value.values()))
    return value


_HEALTH_OK = {"status": "ok"}


def _fault(exc: BaseException) -> dict:
    """Health JSON for a plugin-raised exception (matches Rust ProcessOutcome)."""
    if isinstance(exc, PluginFault):
        reason = str(exc)
    else:
        reason = f"{type(exc).__name__}: {exc}"
        traceback.print_exc(file=sys.stderr)
    return {"status": "fault", "reason": reason}


def run(plugin_class: Type[Plugin]) -> None:
    """Instantiate *plugin_class* and enter the IPC event loop.

    Call this from the plugin script's ``if __name__ == "__main__"`` guard.
    """
    # ---- Parse --shm-path from argv ----
    shm_path: str | None = None
    for arg in sys.argv[1:]:
        if arg.startswith("--shm-path="):
            shm_path = arg.split("=", 1)[1]
            break

    if shm_path is None:
        print("error: --shm-path=<path> is required", file=sys.stderr)
        sys.exit(1)

    # ---- Open shared memory ----
    shm = SharedMemory(shm_path)

    # ---- Instantiate plugin and obtain manifest ----
    plugin = plugin_class()
    manifest = plugin_class.manifest()

    # Port-id lists in manifest order, split by direction.  The shell's SHM
    # buffers and header arrays use exactly this ordering (consumers first
    # 0..n_in, then producers 0..n_out).
    input_ports: list[str] = []
    output_ports: list[str] = []
    for port in manifest.get("ports", []):
        if port["direction"] == "consumer":
            input_ports.append(port["id"])
        else:
            output_ports.append(port["id"])

    # ---- Send ready ----
    _send_msg(
        {
            "method": "ready",
            "params": {
                "protocol": "sigflow-plugin-v1",
                "manifest": manifest,
            },
        }
    )

    # ---- Receive init ----
    init_msg = _read_msg()
    if init_msg.get("method") != "init":
        print(f"error: expected 'init', got: {init_msg}", file=sys.stderr)
        sys.exit(1)

    init_params = init_msg.get("params", {})

    # Apply initial parameter values.  A setup() failure is fatal: the plugin
    # would be permanently broken, so exit before acking (the shell reports a
    # load error at plugin-set time).
    raw_params = init_params.get("params", {})
    plain_params = {k: _extract_param_value(v) for k, v in raw_params.items()}
    try:
        plugin.setup(plain_params)
    except BaseException:
        traceback.print_exc(file=sys.stderr)
        print("error: setup() failed", file=sys.stderr)
        sys.exit(1)

    # ---- Send initialized ----
    _send_msg({"method": "initialized"})

    # ---- Main loop ----
    try:
        while True:
            msg = _read_msg()
            method = msg.get("method")
            params = msg.get("params", {})

            if method == "process":
                # Per-input headers + shell-measured ingress latencies travel
                # in the control message (parallel to input_ports); bulk bytes
                # are already in SHM.
                raw_headers = params.get("in_headers") or []
                raw_latencies = params.get("in_latencies") or []
                inputs: dict[str, Frame] = {}
                for i, port_id in enumerate(input_ports):
                    header = FrameHeader.from_dict(
                        raw_headers[i] if i < len(raw_headers) else None
                    )
                    latency = (
                        int(raw_latencies[i]) if i < len(raw_latencies) else 0
                    )
                    inputs[port_id] = Frame(
                        header=header, data=shm.read_input(i), latency_ns=latency
                    )

                health = _HEALTH_OK
                outputs: dict[str, FrameOut] = {}
                try:
                    outputs = plugin.process(inputs) or {}
                except BaseException as exc:
                    health = _fault(exc)

                # Write outputs to SHM; unset ports report a zero header +
                # zero bytes, which the shell recognises as idle and skips.
                out_headers = []
                for i, port_id in enumerate(output_ports):
                    fo = outputs.get(port_id)
                    if fo is None:
                        shm.write_output(i, b"")
                        out_headers.append(FrameHeader().to_dict())
                    else:
                        shm.write_output(i, fo.data)
                        out_headers.append(fo.header.to_dict())
                shm.flush()

                _send_msg(
                    {
                        "method": "processed",
                        "params": {
                            "seq": params.get("seq", 0),
                            "out_headers": out_headers,
                            "health": health,
                        },
                    }
                )

            elif method == "start":
                health = _HEALTH_OK
                try:
                    plugin.start()
                except BaseException as exc:
                    health = _fault(exc)
                _send_msg({"method": "started", "params": {"health": health}})

            elif method == "stop":
                try:
                    plugin.stop()
                except BaseException:
                    traceback.print_exc(file=sys.stderr)
                _send_msg({"method": "stopped"})

            elif method == "set_param":
                name = params.get("id", "")
                value = _extract_param_value(params.get("value"))
                try:
                    plugin.on_param_change(name, value)
                except BaseException:
                    traceback.print_exc(file=sys.stderr)

            elif method == "invoke_action":
                name = params.get("name", "")
                try:
                    plugin.on_action(name)
                except BaseException:
                    traceback.print_exc(file=sys.stderr)

            elif method == "shutdown":
                plugin.teardown()
                shm.close()
                _send_msg({"method": "shutdown_complete"})
                sys.exit(0)

            else:
                # Unknown message -- ignore (forward compatibility).
                pass

    except EOFError:
        # Shell closed the control channel.
        plugin.teardown()
        shm.close()
        sys.exit(0)
    except KeyboardInterrupt:
        plugin.teardown()
        shm.close()
        sys.exit(0)
