#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/codex-sub.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/codex-sub" ;;
    *) rel="x86_64-apple-darwin/codex-sub" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/codex-sub" ;;
    *) rel="x86_64-unknown-linux-musl/codex-sub" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/codex-sub.exe" "$here/target/release/codex-sub" "$here/target/debug/codex-sub.exe" "$here/target/debug/codex-sub"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "codex-sub binary not found in $here" >&2
exit 1
