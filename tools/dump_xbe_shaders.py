#!/usr/bin/env python3
"""Extract embedded NV2A shader sources from a Splinter Cell XBE.

Scans the whole file for null-terminated ASCII strings beginning with one of
the Xbox shader-assembler magics (`xps.`, `xvs.`, `vs.`, optionally preceded
by `@`). Writes each unique shader to <out>/ alongside a manifest.json that
records file offset, length, kind, and a guessed name (when the binary also
contains a matching `Failed to compile <name>!` diagnostic).

Usage: dump_xbe_shaders.py <xbe-path> <out-dir>

Pure stdlib; no XBE parsing required.
"""
from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path

SHADER_RE = re.compile(rb"@?(xps|xvs|xvsw|xvss|vs)\.1\.[01](?!\d)")
COMPILE_ERR_RE = re.compile(rb"Failed to compile (\w+)!")
ASSEMBLE_ERR_RE = re.compile(rb"Failed to assemble (\w+ shader)")


def find_shaders(buf: bytes) -> list[dict]:
    out: list[dict] = []
    seen: set[int] = set()
    for m in SHADER_RE.finditer(buf):
        start = m.start()
        # Include leading `@` if there is one (Xbox assembler accepts it).
        if start > 0 and buf[start - 1 : start] == b"@":
            start -= 1
        if start in seen:
            continue
        end = buf.find(b"\x00", start)
        if end < 0 or end - start > 4096:
            continue
        body = buf[start:end]
        if not body or any(c < 9 or (13 < c < 32) for c in body):
            continue
        if b"\n" not in body:
            continue
        seen.add(start)
        kind = "psh" if body.lstrip(b"@").startswith(b"xps") else "vsh"
        out.append({"offset": start, "length": end - start, "kind": kind, "body": body})
    return out


def clean(body: bytes) -> str:
    """Collapse the assembler's space-padding so the file is readable.

    Original strings pad each line with trailing spaces to a fixed width.
    We strip per-line trailing whitespace and collapse runs of >=2 internal
    spaces; tabs and structure are preserved.
    """
    text = body.decode("ascii", errors="replace")
    lines = []
    for line in text.splitlines():
        line = re.sub(r"  +", " ", line).rstrip()
        lines.append(line)
    return "\n".join(lines).rstrip() + "\n"


def gather_error_names(buf: bytes) -> list[str]:
    names = [m.group(1).decode() for m in COMPILE_ERR_RE.finditer(buf)]
    seen: set[str] = set()
    out: list[str] = []
    for n in names:
        if n not in seen:
            seen.add(n)
            out.append(n)
    return out


def label_shaders(shaders: list[dict], names: list[str]) -> None:
    """Tag pixel shaders with the closest preceding 'Failed to compile X!' name.

    The engine's compile sequence stores the error strings adjacent to the
    shader sources in .data, so for any binary that contains both we can
    pair them up by file order. Vertex shaders aren't covered by this loop.
    """
    pixel = [s for s in shaders if s["kind"] == "psh"]
    if len(pixel) == len(names):
        for shader, name in zip(pixel, names):
            shader["label"] = name


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    xbe_path = Path(argv[1])
    out_dir = Path(argv[2])
    out_dir.mkdir(parents=True, exist_ok=True)

    buf = xbe_path.read_bytes()
    shaders = find_shaders(buf)
    if not shaders:
        print(f"{xbe_path}: no shader sources found", file=sys.stderr)
        return 1

    label_shaders(shaders, gather_error_names(buf))

    # Stable filename: label if known, else first-line tag + short hash.
    used: dict[str, int] = {}
    manifest = []
    for s in shaders:
        body: bytes = s["body"]
        digest = hashlib.sha1(body).hexdigest()[:8]
        if "label" in s:
            stem = s["label"]
        else:
            first = body.lstrip(b"@").split(b"\n", 1)[0].strip().decode("ascii", "replace")
            tag = re.sub(r"[^A-Za-z0-9]+", "_", first).strip("_") or "shader"
            stem = f"{tag}_{digest}"
        ext = ".psh" if s["kind"] == "psh" else ".vsh"
        n = used.get(stem, 0)
        used[stem] = n + 1
        name = f"{stem}{ext}" if n == 0 else f"{stem}_{n}{ext}"
        (out_dir / name).write_text(clean(body))
        manifest.append({
            "name": name,
            "offset": f"0x{s['offset']:x}",
            "length": s["length"],
            "kind": s["kind"],
            "sha1": digest,
            "label": s.get("label"),
        })

    (out_dir / "manifest.json").write_text(
        json.dumps({"source": str(xbe_path), "shaders": manifest}, indent=2) + "\n"
    )
    print(f"{xbe_path.name}: {len(shaders)} shader(s) -> {out_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
