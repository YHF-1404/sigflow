"""Tests for the sigflow_sdk SharedMemory helper.

These tests create a temporary SHM file with the correct sigflow header
layout and verify that the Python SharedMemory class can parse and
read/write it correctly.
"""

import mmap
import struct
import tempfile
from pathlib import Path

import sys
import os

# Ensure the SDK package is importable when running from the repo root.
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from sigflow_sdk._shm import SharedMemory, HEADER_SIZE, PORT_ENTRY_SIZE, SHM_MAGIC


def _create_shm_file(
    path: str,
    num_inputs: int,
    num_outputs: int,
    capacity: int,
) -> None:
    """Create an SHM file with a valid header and port table (mirrors the Rust side)."""
    total_ports = num_inputs + num_outputs
    port_table_size = total_ports * PORT_ENTRY_SIZE
    buffers_size = total_ports * capacity
    total_size = HEADER_SIZE + port_table_size + buffers_size

    with open(path, "wb") as f:
        f.write(b"\x00" * total_size)

    with open(path, "r+b") as f:
        mm = mmap.mmap(f.fileno(), total_size)

        # Header.
        struct.pack_into("<I", mm, 0, SHM_MAGIC)
        struct.pack_into("<I", mm, 4, 1)  # version
        struct.pack_into("<H", mm, 8, num_inputs)
        struct.pack_into("<H", mm, 10, num_outputs)

        # Port table.
        buf_base = HEADER_SIZE + port_table_size
        for i in range(total_ports):
            entry_off = HEADER_SIZE + i * PORT_ENTRY_SIZE
            offset = buf_base + i * capacity
            struct.pack_into("<Q", mm, entry_off, offset)
            struct.pack_into("<Q", mm, entry_off + 8, capacity)
            struct.pack_into("<Q", mm, entry_off + 16, 0)  # frame_size

        mm.flush()
        mm.close()


def test_parse_header():
    """SharedMemory correctly parses num_inputs/num_outputs from the header."""
    with tempfile.NamedTemporaryFile(suffix=".shm", delete=False) as tmp:
        path = tmp.name

    try:
        _create_shm_file(path, num_inputs=2, num_outputs=3, capacity=256)
        shm = SharedMemory(path)
        assert shm.num_inputs == 2
        assert shm.num_outputs == 3
        shm.close()
    finally:
        os.unlink(path)


def test_read_input():
    """SharedMemory can read data written to an input buffer by the shell."""
    with tempfile.NamedTemporaryFile(suffix=".shm", delete=False) as tmp:
        path = tmp.name

    try:
        cap = 1024
        _create_shm_file(path, num_inputs=1, num_outputs=1, capacity=cap)

        # Simulate the shell writing input data directly.
        with open(path, "r+b") as f:
            mm = mmap.mmap(f.fileno(), 0)
            # Port 0 (input): read offset from port table.
            entry_off = HEADER_SIZE
            offset = struct.unpack_from("<Q", mm, entry_off)[0]
            data = b"hello from shell"
            mm[offset : offset + len(data)] = data
            # Update frame_size.
            struct.pack_into("<Q", mm, entry_off + 16, len(data))
            mm.flush()
            mm.close()

        shm = SharedMemory(path)
        result = shm.read_input(0)
        assert result == b"hello from shell"
        shm.close()
    finally:
        os.unlink(path)


def test_write_output():
    """SharedMemory can write output data that the shell will read."""
    with tempfile.NamedTemporaryFile(suffix=".shm", delete=False) as tmp:
        path = tmp.name

    try:
        cap = 1024
        _create_shm_file(path, num_inputs=1, num_outputs=1, capacity=cap)

        shm = SharedMemory(path)
        shm.write_output(0, b"output bytes")
        shm.flush()

        # Verify by reading the raw mmap.
        with open(path, "r+b") as f:
            mm = mmap.mmap(f.fileno(), 0)
            # Port 1 (first output): read offset from port table.
            entry_off = HEADER_SIZE + PORT_ENTRY_SIZE  # second port entry
            offset = struct.unpack_from("<Q", mm, entry_off)[0]
            frame_size = struct.unpack_from("<Q", mm, entry_off + 16)[0]
            result = bytes(mm[offset : offset + frame_size])
            mm.close()

        assert result == b"output bytes"
        shm.close()
    finally:
        os.unlink(path)


def test_roundtrip_multiple_ports():
    """Write to multiple output ports and read them back."""
    with tempfile.NamedTemporaryFile(suffix=".shm", delete=False) as tmp:
        path = tmp.name

    try:
        cap = 256
        _create_shm_file(path, num_inputs=2, num_outputs=2, capacity=cap)

        shm = SharedMemory(path)
        shm.write_output(0, b"out0")
        shm.write_output(1, b"out1")
        shm.flush()

        # Re-open to verify.
        shm2 = SharedMemory(path)
        # Outputs are at port indices num_inputs+0 and num_inputs+1.
        # Use raw mmap to verify frame_size.
        entry_off_out0 = HEADER_SIZE + 2 * PORT_ENTRY_SIZE  # port index 2
        entry_off_out1 = HEADER_SIZE + 3 * PORT_ENTRY_SIZE  # port index 3

        with open(path, "r+b") as f:
            mm = mmap.mmap(f.fileno(), 0)
            off0 = struct.unpack_from("<Q", mm, entry_off_out0)[0]
            fs0 = struct.unpack_from("<Q", mm, entry_off_out0 + 16)[0]
            off1 = struct.unpack_from("<Q", mm, entry_off_out1)[0]
            fs1 = struct.unpack_from("<Q", mm, entry_off_out1 + 16)[0]
            assert bytes(mm[off0 : off0 + fs0]) == b"out0"
            assert bytes(mm[off1 : off1 + fs1]) == b"out1"
            mm.close()

        shm.close()
        shm2.close()
    finally:
        os.unlink(path)
