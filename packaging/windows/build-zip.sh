#!/bin/sh
# Cross-builds the Windows binaries (GNU toolchain) and zips them with the
# WebView2 loader the tray app needs. Needs mingw-w64 (brew install mingw-w64).
set -eu
cd "$(dirname "$0")/../.."
target=x86_64-pc-windows-gnu
rustup target add "$target" >/dev/null
cargo build --release --target "$target" -p tokenshare-tray -p tokenshare
out="target/$target"
dist="$out/dist"
rm -rf "$dist"
mkdir -p "$dist"
cp "$out/release/tokenshare-tray.exe" "$out/release/tokenshare.exe" "$dist/"
cp "$out"/release/build/webview2-com-sys-*/out/x64/WebView2Loader.dll "$dist/"
rm -f "$out/tokenshare-windows-x64.zip"
(cd "$dist" && zip -q "../tokenshare-windows-x64.zip" tokenshare-tray.exe tokenshare.exe WebView2Loader.dll)
echo "Built: $out/tokenshare-windows-x64.zip"
