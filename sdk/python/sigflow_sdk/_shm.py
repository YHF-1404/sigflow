"""Shared-memory helper for the sigflow plugin IPC data channel.

The SHM layout is defined by the sigflow shell:

    Header (64 bytes):
        magic     : u32 LE  (0x46474953 == "SIGF")
        version   : u32 LE  (1)
        num_inputs: u16 LE
        num_outputs: u16 LE
        reserved  : 52 bytes (zero)

    Port Table (one entry per port, inputs first then outputs):
        offset    : u64 LE  (byte offset into the file for this port's buffer)
        capacity  : u64 LE  (max bytes for this port)
        frame_size: u64 LE  (actual bytes written for the current frame)

    Buffers:
        Contiguous byte regions, one per port, each ``capacity`` bytes long.
"""

import mmap
import struct
from pathlib import Path


# Layout constants (must match the Rust side).
HEADER_SIZE = 64
PORT_ENTRY_SIZE = 24  # 3 x u64
SHM_MAGIC = 0x46474953


class SharedMemory:
    """Read/write access to a sigflow shared-memory region."""

    def __init__(self, path: str):
        self._path = path
        self._file = open(path, "r+b")
        self._mm = mmap.mmap(self._file.fileno(), 0)

        # Parse header.
        if len(self._mm) < HEADER_SIZE:
            raise ValueError("SHM file too small")

        magic, version, num_in, num_out = struct.unpack_from("<IIhh", self._mm, 0)
        if magic != SHM_MAGIC:
            raise ValueError(
                f"bad SHM magic: expected {SHM_MAGIC:#x}, got {magic:#x}"
            )
        if version != 1:
            raise ValueError(f"unsupported SHM version: {version}")

        self.num_inputs: int = num_in
        self.num_outputs: int = num_out
        self._total_ports: int = num_in + num_out

        # Pre-parse port table.
        self._port_entries: list[tuple[int, int]] = []  # (offset, capacity)
        for i in range(self._total_ports):
            base = HEADER_SIZE + i * PORT_ENTRY_SIZE
            offset, capacity, _ = struct.unpack_from("<QQQ", self._mm, base)
            self._port_entries.append((offset, capacity))

    # ------------------------------------------------------------------
    # Port table helpers
    # ------------------------------------------------------------------

    def _get_frame_size(self, port_index: int) -> int:
        base = HEADER_SIZE + port_index * PORT_ENTRY_SIZE + 16
        return struct.unpack_from("<Q", self._mm, base)[0]

    def _set_frame_size(self, port_index: int, size: int) -> None:
        base = HEADER_SIZE + port_index * PORT_ENTRY_SIZE + 16
        struct.pack_into("<Q", self._mm, base, size)

    # ------------------------------------------------------------------
    # Public read / write
    # ------------------------------------------------------------------

    def read_input(self, port_index: int) -> bytes:
        """Read data from an input port buffer."""
        if port_index >= self.num_inputs:
            raise IndexError(f"input port index {port_index} out of range")
        offset, _capacity = self._port_entries[port_index]
        frame_size = self._get_frame_size(port_index)
        return bytes(self._mm[offset : offset + frame_size])

    def write_output(self, port_index: int, data: bytes) -> None:
        """Write data to an output port buffer (truncated to capacity, loudly)."""
        idx = self.num_inputs + port_index
        if port_index >= self.num_outputs:
            raise IndexError(f"output port index {port_index} out of range")
        offset, capacity = self._port_entries[idx]
        if len(data) > capacity:
            # Truncation corrupts self-describing frames (heatmaps etc.) —
            # keep running but make the cause impossible to miss.
            import sys

            print(
                f"sigflow_sdk: output port {port_index} frame "
                f"({len(data)} B) exceeds SHM capacity ({capacity} B) — "
                "TRUNCATED. Increase the shell's port capacity.",
                file=sys.stderr,
                flush=True,
            )
        length = min(len(data), capacity)
        self._mm[offset : offset + length] = data[:length]
        self._set_frame_size(idx, length)

    def flush(self) -> None:
        """Flush the mmap to ensure the shell can see our writes."""
        self._mm.flush()

    def close(self) -> None:
        """Unmap and close the backing file."""
        self._mm.close()
        self._file.close()
