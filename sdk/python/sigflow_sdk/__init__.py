"""sigflow Python plugin SDK.

Provides the base class and runtime for writing sigflow plugins in Python.
Plugins communicate with the sigflow shell via JSON-line control messages
over stdin/stdout and shared-memory data buffers (protocol
``sigflow-plugin-v1``; Rust reference: ``sdk/rust``).

stdout is the control channel — plugin logging must go to stderr.

Usage:
    from sigflow_sdk import Frame, FrameOut, Plugin, run

    class MyPlugin(Plugin):
        ...

    if __name__ == "__main__":
        run(MyPlugin)
"""

from sigflow_sdk.frame import (
    FLAG_ANNOTATED,
    FLAG_DISCONTINUITY,
    FLAG_TRIG_INDEXED,
    Frame,
    FrameHeader,
    FrameOut,
    parse_annotation,
)
from sigflow_sdk.plugin import Plugin, PluginFault
from sigflow_sdk._runtime import run

__all__ = [
    "FLAG_ANNOTATED",
    "FLAG_DISCONTINUITY",
    "FLAG_TRIG_INDEXED",
    "Frame",
    "FrameHeader",
    "FrameOut",
    "Plugin",
    "PluginFault",
    "parse_annotation",
    "run",
]
