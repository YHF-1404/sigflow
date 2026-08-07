"""Plugin base class for sigflow plugins."""

from abc import ABC, abstractmethod
from typing import Any

from sigflow_sdk.frame import Frame, FrameOut


class PluginFault(Exception):
    """Raise from ``start()`` / ``process()`` to report a *logical* fault.

    The runtime converts it into ``{"status": "fault", "reason": ...}`` in the
    reply's health field (the shell's supervisor then applies its restart
    policy).  Any other exception is reported the same way with the exception
    text as the reason — ``PluginFault`` just lets the plugin author control
    the wording.
    """


class Plugin(ABC):
    """Base class that all sigflow Python plugins must subclass.

    Plugin authors implement ``manifest()``, ``process()``, and optionally
    override the lifecycle hooks.  Lifecycle order (mirrors the shell's
    node lifecycle):

        setup(params)  — once, after the init handshake
        start()        — processing is about to begin
        process(...)   — once per shell tick with pending input
        stop()         — processing halted (may start() again later)
        teardown()     — once, before the subprocess exits
    """

    @staticmethod
    @abstractmethod
    def manifest() -> dict:
        """Return the plugin manifest as a dict.

        The manifest declares the plugin's name, version, ports, parameters,
        actions, and runtime configuration.  It must agree with the packaged
        ``manifest.toml`` — the simplest way is to parse that file (e.g. with
        ``tomllib``) instead of duplicating it in code.
        """
        ...

    @abstractmethod
    def process(self, inputs: dict[str, Frame]) -> dict[str, FrameOut] | None:
        """Process one tick.

        Args:
            inputs: Mapping of input port id to that port's latest ``Frame``.
                A port with no data this tick maps to an empty frame
                (``header.n_samples == 0``, ``data == b""``).

        Returns:
            Mapping of output port id to the ``FrameOut`` to publish.  Ports
            omitted (or a ``None`` return) publish nothing this tick.

        Raising ``PluginFault`` (or any exception) reports a fault to the
        shell instead of output.
        """
        ...

    def start(self) -> None:
        """Called when the node's processing starts.

        Must return promptly (the shell waits ~2 s for the ack); put heavy
        one-time work in ``setup()`` instead.  Raise to report a fault.
        """
        pass

    def stop(self) -> None:
        """Called when the node's processing stops."""
        pass

    def on_param_change(self, name: str, value: Any) -> None:
        """Called when a parameter value changes.

        Override to react to hot parameter updates.  The default
        implementation does nothing.
        """
        pass

    def on_action(self, name: str) -> None:
        """Called when an action is invoked by the shell.

        Override to handle actions.  The default implementation does nothing.
        """
        pass

    def setup(self, params: dict) -> None:
        """Called once after the init handshake with initial parameter values.

        Override to perform one-time initialisation (heavy precomputation is
        fine here — the shell's init read has no tight deadline).  The default
        implementation does nothing.
        """
        pass

    def teardown(self) -> None:
        """Called before the plugin shuts down.

        Override to release resources.  The default implementation does
        nothing.
        """
        pass
