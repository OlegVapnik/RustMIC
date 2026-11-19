#!/usr/bin/env python3
"""Replica of the launcher's find_container() to validate a packed exe.

Mirrors tools/windows-portable/src/main.rs: search the marker backwards, walk
the file table without copying data and accept only the candidate whose table
ends exactly at EOF.
"""
import struct
import sys

MARKER = b"RUSTMIC-PAYLOAD-V1\n"


def read_u32(b, at):
    if at + 4 > len(b):
        return None, at
    return struct.unpack_from("<I", b, at)[0], at + 4


def read_u64(b, at):
    if at + 8 > len(b):
        return None, at
    return struct.unpack_from("<Q", b, at)[0], at + 8


def walks_to_eof(b, at, count):
    for _ in range(count):
        name_len, at = read_u32(b, at)
        if name_len is None or name_len == 0 or name_len > 512:
            return False
        if at + name_len > len(b):
            return False
        at += name_len
        data_len, at = read_u64(b, at)
        if data_len is None or at + data_len > len(b):
            return False
        at += data_len
    return at == len(b)


def find_container(b):
    end = len(b)
    while end >= len(MARKER):
        pos = b.rfind(MARKER, 0, end)
        if pos < 0:
            return None
        at = pos + len(MARKER)
        count, after = read_u32(b, at)
        if count is not None and 0 < count <= 4096 and walks_to_eof(b, after, count):
            return at
        end = pos + len(MARKER) - 1
    return None


def main(path):
    b = open(path, "rb").read()
    positions = []
    i = b.find(MARKER)
    while i != -1:
        positions.append(i)
        i = b.find(MARKER, i + 1)
    at = find_container(b)
    if at is None:
        raise SystemExit("no container found")
    count, at = read_u32(b, at)
    total = 0
    names = []
    for _ in range(count):
        name_len, at = read_u32(b, at)
        name = b[at:at + name_len].decode()
        at += name_len
        data_len, at = read_u64(b, at)
        at += data_len
        total += data_len
        names.append(f"{name} ({data_len})")
    print(f"file: {path} ({len(b)} bytes)")
    print(f"marker positions: {positions}")
    print(f"accepted container at: {at - total} ... eof")
    print(f"files: {count}, payload {total / 1048576:.1f} MiB")
    for n in names:
        print("  " + n)
    if at != len(b):
        raise SystemExit("container does not end at EOF")
    print("OK")


if __name__ == "__main__":
    main(sys.argv[1])
