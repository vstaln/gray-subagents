#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-subagents.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-subagents" ;;
    *) rel="x86_64-apple-darwin/gray-subagents" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-subagents" ;;
    *) rel="x86_64-unknown-linux-musl/gray-subagents" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-subagents.exe" "$here/target/release/gray-subagents" "$here/target/debug/gray-subagents.exe" "$here/target/debug/gray-subagents"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-subagents binary not found in $here" >&2
exit 1
