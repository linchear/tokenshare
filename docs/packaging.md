# Building and packaging

```bash
cargo build --release      # target/release/tokenshare, target/release/tokenshare-tray
cargo test                 # unit tests plus multi-node integration tests
cargo clippy --all-targets -- -D warnings
```

The packaging scripts cross-compile from macOS (or Linux) and need the toolchains
noted at the top of each.

## macOS

```bash
packaging/macos/build-app.sh   # target/release/bundle/Token Share.app
```

A menubar-only app with no Dock icon. It is **unsigned**, so Gatekeeper may ask you
to right-click and choose Open the first time (or clear the quarantine flag with
`xattr -dr com.apple.quarantine "Token Share.app"`).

## Windows

```bash
packaging/windows/build-zip.sh   # target/x86_64-pc-windows-gnu/tokenshare-windows-x64.zip
```

Needs `brew install mingw-w64` and the `x86_64-pc-windows-gnu` Rust target. The zip
holds `tokenshare-tray.exe`, `tokenshare.exe` and `WebView2Loader.dll`, which must
stay next to the tray exe. Unzip anywhere and run the tray exe. (On Windows itself,
`cargo build --release -p tokenshare-tray -p tokenshare` works.)

## Linux

```bash
packaging/linux/build-deb.sh     # target/deb/tokenshare_<version>_amd64.deb
```

Needs `brew install zig dpkg` and `cargo install cargo-zigbuild`. The `.deb`
installs `/usr/bin/tokenshare`, its man page (`man tokenshare`), a systemd *user*
unit, and an example config under `/usr/share/doc/tokenshare/`. It is built against
glibc 2.17 and needs only libc. After `sudo apt install ./tokenshare_*_amd64.deb`,
run `systemctl --user enable --now tokenshare`; configuration is read from
`~/.config/tokenshare/config.toml`. To keep it running without a login session, use
`loginctl enable-linger $USER`. Full guide: [linux.md](linux.md).

## App icon

A blue squircle with a mesh of three linked nodes and a gold token in the middle;
more tokens travel along the links. It is drawn in code
(`crates/tray/examples/app_icon.rs`), and the generated assets are committed:
`packaging/macos/AppIcon.icns` (bundled by `build-app.sh`) and
`crates/tray/icon.ico` (embedded in `tokenshare-tray.exe` by `crates/tray/build.rs`).
After changing the artwork, run `packaging/icon/generate.sh` on a Mac. This is
separate from the small status icons in the menubar/taskbar.

## Repository layout

| Path | What |
| --- | --- |
| `crates/core` | the library: proxy, delegate table, mesh protocol, daemon startup, health, response logging |
| `crates/tokenshare` | the headless daemon and CLI |
| `crates/tray` | the menubar/taskbar app (a stub on Linux) |
| `packaging/` | macOS app, Windows zip and Linux `.deb` scripts, icon generator |
| `design/` | the original design sketch and a plan recording the decisions |
