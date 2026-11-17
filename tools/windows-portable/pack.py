#!/usr/bin/env python3
"""Append the payload directory to the cross-compiled launcher exe.

Container format (little endian, appended after the ASCII marker):
    u32 count
    repeat count: u32 name_len, name bytes, u64 data_len, data bytes

`build-portable.sh` produces `RustMIC-portable.exe = launcher.exe + container`.
"""
import pathlib
import struct
import sys

MARKER = b"RUSTMIC-PAYLOAD-V1\n"


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: pack.py <payload-dir> <launcher.exe> <output.exe>", file=sys.stderr)
        return 2
    payload = pathlib.Path(sys.argv[1])
    launcher = pathlib.Path(sys.argv[2])
    output = pathlib.Path(sys.argv[3])
    files = sorted(p for p in payload.rglob("*") if p.is_file())
    if not files:
        print(f"no payload files in {payload}", file=sys.stderr)
        return 1

    container = bytearray(MARKER)
    container += struct.pack("<I", len(files))
    for path in files:
        name = path.relative_to(payload).as_posix().encode("utf-8")
        data = path.read_bytes()
        container += struct.pack("<I", len(name))
        container += name
        container += struct.pack("<Q", len(data))
        container += data

    output.write_bytes(launcher.read_bytes() + bytes(container))
    print(f"packed {len(files)} files ({len(container) / 1_048_576:.1f} MiB) -> {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
