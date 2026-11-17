#!/usr/bin/env bash
# Build the one-file Windows app: RustMIC-portable.exe.
#
# Prerequisites on Linux:
#   rustup target add x86_64-pc-windows-gnu
#   llvm-mingw (default: /tmp/opencode/llvm-mingw/opt/llvm-mingw) or a normal
#   MinGW-w64 toolchain in PATH. llvm-mingw has no libgcc, so we link its
#   libunwind under the names rustc expects (libgcc/libgcc_eh).
#   curl + bsdtar (for fetch-payloads.sh)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/tools/windows-portable"
LLVM_ROOT="${LLVM_MINGW_ROOT:-/tmp/opencode/llvm-mingw/opt/llvm-mingw}"
STUBS="${MINGW_STUB_DIR:-/tmp/opencode/llvm-mingw-stubs}"

export PATH="$HOME/.cargo/bin:$PATH"
if [ -d "$LLVM_ROOT/bin" ]; then
  export PATH="$LLVM_ROOT/bin:$PATH"
  mkdir -p "$STUBS"
  ln -sf "$LLVM_ROOT/x86_64-w64-mingw32/lib/libunwind.a" "$STUBS/libgcc.a"
  ln -sf "$LLVM_ROOT/x86_64-w64-mingw32/lib/libunwind.a" "$STUBS/libgcc_eh.a"
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="${CROSS_LINKER:-x86_64-w64-mingw32-clang}"
  export RUSTFLAGS="${RUSTFLAGS:-} -C panic=abort -C link-arg=-L$STUBS"
else
  # Fall back to a MinGW-w64 gcc installation.
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="${CROSS_LINKER:-x86_64-w64-mingw32-gcc}"
  export RUSTFLAGS="${RUSTFLAGS:-} -C panic=abort"
fi

echo "== RustMIC + GUI (windows-gnu) =="
(cd "$ROOT" && cargo build --target x86_64-pc-windows-gnu --release --features gui)

echo "== launcher (windows-gnu) =="
(cd "$HERE" && cargo build --target x86_64-pc-windows-gnu --release)

echo "== payloads =="
"$HERE/fetch-payloads.sh"

echo "== pack =="
python3 "$HERE/pack.py" \
  "$HERE/payload" \
  "$HERE/target/x86_64-pc-windows-gnu/release/rustmic-launcher.exe" \
  "$HERE/RustMIC-portable.exe"

echo "done: $HERE/RustMIC-portable.exe"
