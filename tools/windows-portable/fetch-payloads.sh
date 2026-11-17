#!/usr/bin/env bash
# Fetch third-party payloads for the one-file Windows build.
# Run on Linux (needs curl + bsdtar). RustMIC.exe / RustMIC-gui.exe must already
# be cross-compiled into target/x86_64-pc-windows-gnu/release/.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PAYLOAD="$ROOT/tools/windows-portable/payload"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

rm -rf "$PAYLOAD"
mkdir -p "$PAYLOAD" "$TMP/rustmic" "$TMP/mpv" "$TMP/vbcable"

echo "== own binaries =="
cp -f "$ROOT/target/x86_64-pc-windows-gnu/release/RustMIC.exe" "$PAYLOAD/"
cp -f "$ROOT/target/x86_64-pc-windows-gnu/release/RustMIC-gui.exe" "$PAYLOAD/"

echo "== rustmic client 0.1.2 =="
curl -L --fail -o "$TMP/rustmic.zip" \
  https://github.com/sambukuper/rustmic/releases/download/v0.1.2/rustmic-v0.1.2-windows-x64.zip
bsdtar -xf "$TMP/rustmic.zip" -C "$TMP/rustmic"
# Name it differently: RustMIC.exe already exists and Windows filenames are
# case-insensitive. Do NOT copy the upstream rustmic*.exe as-is.
cp -f "$TMP/rustmic/rustmic.exe" "$PAYLOAD/RustMIC-client.exe"

echo "== yt-dlp =="
curl -L --fail -o "$PAYLOAD/yt-dlp.exe" \
  https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe

echo "== mpv =="
curl -L --fail -o "$TMP/mpv.7z" \
  https://sourceforge.net/projects/mpv-player-windows/files/64bit-v3/mpv-x86_64-v3-20261108-git-55d3feb45c.7z/download
bsdtar -xf "$TMP/mpv.7z" -C "$TMP/mpv"
while IFS= read -r -d '' file; do
  cp -f "$file" "$PAYLOAD/"
done < <(find "$TMP/mpv" -type f \( -iname '*.exe' -o -iname '*.dll' \) -print0)

echo "== VB-CABLE =="
curl -L --fail -o "$TMP/vbcable.zip" \
  https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack43.zip
bsdtar -xf "$TMP/vbcable.zip" -C "$TMP/vbcable"
cp -f "$TMP/vbcable/VBCABLE_Setup_x64.exe" "$PAYLOAD/"

# Windows filenames are case-insensitive: a collision would silently overwrite
# one binary with another. Fail loudly instead.
python3 - "$PAYLOAD" <<'PY'
import os, sys, collections
names = os.listdir(sys.argv[1])
dupes = [n for n, c in collections.Counter(n.lower() for n in names).items() if c > 1]
if dupes:
    raise SystemExit(f'case-insensitive filename collision in payload: {dupes}')
PY

echo "payload ready: $PAYLOAD ($(du -sh "$PAYLOAD" | cut -f1))"
