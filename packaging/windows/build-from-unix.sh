#!/bin/sh
# Cross-builds the Windows binaries from macOS or Linux (GNU toolchain) and
# zips them to target/x86_64-pc-windows-gnu/tokenshare-windows-x64.zip.
# Needs: mingw-w64 (brew install mingw-w64) and `rustup target add x86_64-pc-windows-gnu`.
set -eu
cd "$(dirname "$0")/../.."
target=x86_64-pc-windows-gnu
command -v x86_64-w64-mingw32-gcc >/dev/null || { echo "mingw-w64 not found (brew install mingw-w64)" >&2; exit 1; }

cargo build --release --target $target -p tokenshare-tray -p tokenshare

out=target/$target
dist=$out/dist
rm -rf "$dist"; mkdir -p "$dist"
cp "$out/release/tokenshare-tray.exe" "$out/release/tokenshare.exe" "$dist/"
# The tray's settings window loads this DLL at startup; it must sit next to the exe.
cp "$out"/release/build/webview2-com-sys-*/out/x64/WebView2Loader.dll "$dist/"
rm -f "$out/tokenshare-windows-x64.zip"
(cd "$dist" && zip -q ../tokenshare-windows-x64.zip tokenshare-tray.exe tokenshare.exe WebView2Loader.dll)
echo "Built: $out/tokenshare-windows-x64.zip"
