#!/usr/bin/env python3
"""Extract compact codec_id=8 test fixtures from a QEMU plugin trace.

Inputs (on disk): the trace JSON produced by the binassist QEMU plugin,
default path /var/tmp/sc1_proto_audio_trace.json. The audio replay tests
used to load this file directly, which made them require ~27 MB of data
on the dev machine to run.

Outputs (committed to tests/fixtures/, gzipped):
  codec8_kernel_transitions.bin.gz
    Layout: u32 LE count, then `count` records of (1B nibble + 52B entry +
    52B post). Entry/post bytes are the engine's 52-byte ChannelState
    struct.
  codec8_stereo_calls.bin.gz
    Per outer dispatcher call, the input block, both channel state entries,
    and the engine's expected pcm_output for the first `samples_per_call`
    samples. Layout: u32 LE count, then `count` records of
    (128B block + 104B state + 256*2B expected pcm).

Re-run after recapturing the trace; tests read the bytes through
flate2 + a small in-line decoder.
"""

from __future__ import annotations

import gzip
import io
import json
import struct
import sys
from pathlib import Path

TRACE_PATH = Path("/var/tmp/sc1_proto_audio_trace.json")
FIXTURES_DIR = Path(__file__).resolve().parent.parent / "tests" / "fixtures"

# Per-call window for the stereo PCM fixture. The engine emits up to
# 1536 valid samples per outer call; 256 walks the state forward enough
# to catch any regression while keeping the fixture under ~50 KB gz.
SAMPLES_PER_CALL = 256
BLOCK_BYTES_PER_CALL = SAMPLES_PER_CALL // 2  # one byte = two nibbles


def pack_kernel_transitions(trace: dict) -> bytes:
    captures = trace["kernel_captures"]
    out = io.BytesIO()
    out.write(struct.pack("<I", len(captures)))
    for c in captures:
        entry = bytes(c["state_at_stack8"][:52])
        post = bytes(c["state_at_stack8_post"][:52])
        if len(entry) != 52 or len(post) != 52:
            raise SystemExit(
                f"capture has truncated state: entry={len(entry)} post={len(post)}"
            )
        out.write(bytes([c["stack4"] & 0xFF]))
        out.write(entry)
        out.write(post)
    return out.getvalue()


def pack_stereo_calls(trace: dict) -> bytes:
    calls = trace["calls"]
    out = io.BytesIO()
    out.write(struct.pack("<I", len(calls)))
    for c in calls:
        block = bytes(c["block_buffer"][:BLOCK_BYTES_PER_CALL])
        state = bytes(c["channel_state_entry"][:104])
        pcm = bytes(c["pcm_output"][: SAMPLES_PER_CALL * 2])
        if len(block) != BLOCK_BYTES_PER_CALL:
            raise SystemExit(f"call has short block_buffer: {len(block)}")
        if len(state) != 104:
            raise SystemExit(f"call has short channel_state_entry: {len(state)}")
        if len(pcm) != SAMPLES_PER_CALL * 2:
            raise SystemExit(f"call has short pcm_output: {len(pcm)}")
        out.write(block)
        out.write(state)
        out.write(pcm)
    return out.getvalue()


def main() -> None:
    if not TRACE_PATH.exists():
        sys.exit(f"trace not found: {TRACE_PATH}")
    FIXTURES_DIR.mkdir(parents=True, exist_ok=True)
    with TRACE_PATH.open() as f:
        trace = json.load(f)

    pairs = [
        ("codec8_kernel_transitions.bin.gz", pack_kernel_transitions(trace)),
        ("codec8_stereo_calls.bin.gz", pack_stereo_calls(trace)),
    ]
    for name, raw in pairs:
        path = FIXTURES_DIR / name
        gz = gzip.compress(raw, compresslevel=9, mtime=0)
        path.write_bytes(gz)
        print(f"{path}: raw={len(raw):,}B gz={len(gz):,}B")


if __name__ == "__main__":
    main()
