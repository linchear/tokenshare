#!/bin/sh
# Builds "Token Share.app", a menubar-only app, into target/release/bundle/.
set -eu
cd "$(dirname "$0")/../.."
cargo build --release -p tokenshare-tray

version=$(cargo metadata --no-deps --format-version 1 |
  python3 -c 'import sys,json; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="tokenshare-tray"))')
app="target/release/bundle/Token Share.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp target/release/tokenshare-tray "$app/Contents/MacOS/tokenshare-tray"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Token Share</string>
  <key>CFBundleIdentifier</key><string>com.tokenshare.tray</string>
  <key>CFBundleExecutable</key><string>tokenshare-tray</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
echo "Built: $app"
