#!/bin/sh
# Puts the tools Shruti ships next to its executable (Tauri sidecars):
# whisper-cli from the local Vulkan build (see TECH-STACK.md), and the
# official standalone yt-dlp release, checked against its SHA2-256SUMS.
# Run once before `npm run tauri dev` or `npm run tauri build`.
set -eu
cd "$(dirname "$0")/.."

target=$(rustc -vV | sed -n 's/^host: //p')
out=src-tauri/binaries
mkdir -p "$out"

whisper=vendor/whisper.cpp/build/bin/whisper-cli
if [ ! -x "$whisper" ]; then
    echo "missing $whisper: build whisper.cpp first (TECH-STACK.md)" >&2
    exit 1
fi
install -m 755 "$whisper" "$out/shruti-whisper-cli-$target"

base=https://github.com/yt-dlp/yt-dlp/releases/latest/download
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/yt-dlp_linux" "$base/yt-dlp_linux"
curl -fsSL -o "$tmp/SHA2-256SUMS" "$base/SHA2-256SUMS"
(cd "$tmp" && grep ' yt-dlp_linux$' SHA2-256SUMS | sha256sum -c -)
install -m 755 "$tmp/yt-dlp_linux" "$out/shruti-yt-dlp-$target"

echo "whisper-cli: $(wc -c < "$out/shruti-whisper-cli-$target") bytes"
echo "yt-dlp: $("$out/shruti-yt-dlp-$target" --version)"
