#!/bin/sh
# Builds tokenshare_<version>_amd64.deb into target/. Cross-compiles with zig
# (brew install zig; cargo install cargo-zigbuild), targeting glibc 2.17 so it
# runs on any reasonably recent distribution. Needs dpkg-deb (brew install dpkg).
set -eu
cd "$(dirname "$0")/../.."

target=x86_64-unknown-linux-gnu
rustup target add "$target" >/dev/null
cargo zigbuild --release --target "$target.2.17" -p tokenshare

version=$(cargo metadata --no-deps --format-version 1 |
  python3 -c 'import sys,json; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="tokenshare"))')
maintainer=$(git config user.name) email=$(git config user.email)
pkg=target/deb/tokenshare_${version}_amd64
rm -rf "$pkg"
mkdir -p "$pkg/DEBIAN" "$pkg/usr/bin" "$pkg/usr/lib/systemd/user" "$pkg/usr/share/doc/tokenshare" "$pkg/usr/share/man/man1"

install -m 0755 "target/$target/release/tokenshare" "$pkg/usr/bin/tokenshare"
install -m 0644 packaging/linux/tokenshare.service "$pkg/usr/lib/systemd/user/tokenshare.service"
install -m 0644 packaging/linux/config.example.toml "$pkg/usr/share/doc/tokenshare/config.example.toml"
install -m 0644 README.md "$pkg/usr/share/doc/tokenshare/README.md"

# Man page: fill in version and date, then compress as Debian policy asks
# (-n keeps the output reproducible by leaving out the timestamp).
sed -e "s/@VERSION@/$version/" -e "s/@DATE@/$(date +%Y-%m-%d)/" packaging/linux/tokenshare.1 |
  gzip -9n > "$pkg/usr/share/man/man1/tokenshare.1.gz"
chmod 0644 "$pkg/usr/share/man/man1/tokenshare.1.gz"

size=$(du -sk "$pkg/usr" | cut -f1)
cat > "$pkg/DEBIAN/control" <<CONTROL
Package: tokenshare
Version: $version
Section: net
Priority: optional
Architecture: amd64
Depends: libc6 (>= 2.17)
Installed-Size: $size
Maintainer: $maintainer <$email>
Description: Route ollama calls amongst several hosts
 Token Share runs a small delegate that listens on one port and serves ollama
 requests from the local ollama or streams them from a peer delegate that has
 the model. Enable it per user with: systemctl --user enable --now tokenshare
CONTROL

# Files owned by root, whatever the build user is.
dpkg-deb --root-owner-group --build "$pkg" "target/deb/tokenshare_${version}_amd64.deb" >/dev/null
echo "Built: target/deb/tokenshare_${version}_amd64.deb"
