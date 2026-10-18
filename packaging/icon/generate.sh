#!/bin/sh
# Regenerates the app icon assets from crates/tray/examples/app_icon.rs:
#   packaging/macos/AppIcon.icns   macOS bundle icon (needs iconutil, so macOS only)
#   crates/tray/icon.ico           embedded in the Windows exe by crates/tray/build.rs
#   packaging/icon/app-icon.png    1024 px preview
# The outputs are committed; rerun this only when the artwork changes.
set -eu
cd "$(dirname "$0")/../.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cargo run --release -q -p tokenshare-tray --example app_icon -- "$tmp"
iconutil -c icns "$tmp/AppIcon.iconset" -o packaging/macos/AppIcon.icns
cp "$tmp/icon.ico" crates/tray/icon.ico
cp "$tmp/icon-1024.png" packaging/icon/app-icon.png
echo "Updated packaging/macos/AppIcon.icns, crates/tray/icon.ico, packaging/icon/app-icon.png"
