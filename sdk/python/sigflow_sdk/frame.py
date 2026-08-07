"""Frame metadata and payload views for sigflow Python plugins.

Mirrors the Rust side (`sigflow-types/src/frame.rs`): every data frame carries
a self-anchoring ``FrameHeader`` so downstream nodes can reconstruct absolute
sample timestamps.  Headers travel in the JSON control channel (``process`` /
``processed`` messages); bulk sample bytes travel through shared memory.

The JSON field names here must match the Rust ``FrameHeader`` serde encoding
exactly — they are the wire contract.
"""

from dataclasses import dataclass, field

# ``flags`` bits — keep in sync with sigflow-types/src/frame.rs.
FLAG_DISCONTINUITY = 1 << 0
"""A gap precedes this frame; ``gap_samples`` says how many were lost."""
FLAG_TRIG_INDEXED = 1 << 1
"""``sample_index`` is on the paired signal stream's per-channel sample axis."""
FLAG_ANNOTATED = 1 << 2
"""The payload's last ``annotation_len`` bytes are an annotation blob."""

_HEADER_FIELDS = (
    "seq",
    "sample_index",
    "t0_ns",
    "tx_egress_mono_ns",
    "gap_samples",
    "n_samples",
    "flags",
    "annotation_len",
)


@dataclass
class FrameHeader:
    """Self-anchoring per-frame metadata (see the Rust doc comments)."""

    seq: int = 0
    sample_index: int = 0
    t0_ns: int = 0
    tx_egress_mono_ns: int = 0
    gap_samples: int = 0
    n_samples: int = 0
    flags: int = 0
    annotation_len: int = 0

    # -- flag helpers -------------------------------------------------------

    def is_discontinuity(self) -> bool:
        return bool(self.flags & FLAG_DISCONTINUITY)

    def set_discontinuity(self, gap_samples: int) -> None:
        self.flags |= FLAG_DISCONTINUITY
        self.gap_samples = gap_samples

    def is_trig_indexed(self) -> bool:
        return bool(self.flags & FLAG_TRIG_INDEXED)

    def is_annotated(self) -> bool:
        return bool(self.flags & FLAG_ANNOTATED)

    # -- JSON wire encoding -------------------------------------------------

    @classmethod
    def from_dict(cls, d: dict | None) -> "FrameHeader":
        """Build from the serde JSON encoding; missing/unknown fields tolerated."""
        h = cls()
        if isinstance(d, dict):
            for f in _HEADER_FIELDS:
                v = d.get(f)
                if isinstance(v, (int, float)):
                    setattr(h, f, int(v))
        return h

    def to_dict(self) -> dict:
        return {f: getattr(self, f) for f in _HEADER_FIELDS}


RATE_ANNOTATION_SCHEMA = 0x7366_7261_7465_3031
"""流速率注解 schema（"sfrate01"）：流的源节点（时钟权威）在每个数据帧上
带内宣告 {"fs_hz":F}——该流自己 t0 轴上的每秒样本数。消费端优先级：
带内宣告 > 帧头推断 > 先验参数（与 Rust 侧 sigflow-types 同值同义）。"""


def make_annotation(schema_id: int, body: bytes) -> bytes:
    """拼注解尾巴 [schema_id: u64 LE | body]。调用方自行把返回值追加到
    payload 末尾，并设置 header.annotation_len 与 FLAG_ANNOTATED。"""
    return schema_id.to_bytes(8, "little") + body


def encode_rate_annotation(fs_hz: float) -> bytes:
    """速率注解体（JSON）。"""
    return f'{{"fs_hz":{fs_hz!r}}}'.encode()


def parse_annotation(blob: bytes) -> tuple[int, bytes] | None:
    """Split an annotation blob into ``(schema_id, bytes)``.

    The blob layout is ``[annotation_schema_id: u64 LE | annotation bytes]``.
    Returns ``None`` if the blob is too short to hold the schema id.
    """
    if len(blob) < 8:
        return None
    schema_id = int.from_bytes(blob[:8], "little")
    return schema_id, blob[8:]


@dataclass
class Frame:
    """One input frame: header + raw payload bytes (+ shell-measured latency).

    ``data`` is the full payload buffer.  When the frame is annotated the
    sample bytes and the trailing annotation blob are split via ``samples()``
    / ``annotation()`` — sample-only consumers can just call ``samples()``
    unconditionally.
    """

    header: FrameHeader = field(default_factory=FrameHeader)
    data: bytes = b""
    latency_ns: int = 0

    def samples(self) -> bytes:
        """Payload minus any trailing annotation blob."""
        n = self.header.annotation_len
        if not self.header.is_annotated() or n == 0 or n > len(self.data):
            return self.data
        return self.data[: len(self.data) - n]

    def annotation(self) -> bytes | None:
        """The trailing annotation blob, or ``None`` when not annotated."""
        n = self.header.annotation_len
        if not self.header.is_annotated() or n == 0 or n > len(self.data):
            return None
        return self.data[len(self.data) - n :]


@dataclass
class FrameOut:
    """One output frame the plugin wants to publish: header + payload bytes.

    A plugin returns these from ``process()``.  Ports omitted from the result
    (or given ``None``) publish nothing that tick: the runtime reports a
    zero header + zero written bytes, which the shell recognises as "idle
    output" and skips.
    """

    header: FrameHeader = field(default_factory=FrameHeader)
    data: bytes = b""
