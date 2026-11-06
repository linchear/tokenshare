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

## Updates

The macOS app can update itself from the project's GitHub releases. Windows and
Linux do not, yet.

**How it works.** A little after starting and then once a day (while
`check_for_updates` is on), and whenever you choose **Check for Updates…** in its
menu, the app reads `latest.json` from the newest release:

```
https://github.com/linchear/tokenshare/releases/latest/download/latest.json
```

If that version is newer than its own, the menu offers **Install Token Share
*x.y.z* and Restart**, and a notification says so (once per version). Choosing it
downloads `tokenshare-macos-arm64.app.tar.gz`, checks its
[minisign](https://jedisct1.github.io/minisign/) signature against the public key
built into the app, replaces the `.app`, stops the delegate and starts the new
version. An archive that is not signed with the project's key is refused and
nothing changes. If the app is in a folder you cannot write to, macOS asks for an
administrator password. Nothing is installed without being chosen.

The app offers updates only when it runs from an app bundle, and only if it was
built with a public key in `packaging/update-key.pub`. Run from a download that
macOS has not let move (App Translocation), its menu says to move it to
Applications first. Intel Macs are not offered updates: releases are built for
Apple Silicon only.

**Setting it up (once).** Whoever publishes releases makes the key pair:

```bash
brew install minisign
minisign -G -W -p packaging/update-key.pub -s tokenshare-update.key
```

`-W` makes a key without a password, which the release workflow needs. Then:

1. Commit `packaging/update-key.pub`. Apps built after that commit accept updates
   signed with the key.
2. Add the contents of `tokenshare-update.key` as the repository secret
   `TOKENSHARE_UPDATE_KEY` (Settings > Secrets and variables > Actions).
3. Keep `tokenshare-update.key` somewhere safe and offline, and never commit it.
   Anyone with it can sign updates that every installed app will accept. If it
   leaks, make a new pair, commit the new public key, and ask everyone to
   download the next release by hand: apps only trust the key they were built with.

**Releasing.** Bump `version` in the workspace `Cargo.toml`, merge, and push a tag
`v` + that version. The release workflow refuses a tag that does not match. On top
of the zip, the macOS job builds `tokenshare-macos-arm64.app.tar.gz`, signs it
(`.sig`, the minisign signature in base64) and writes `latest.json` with
`packaging/update-manifest.py`; all three are attached to the release. Without the
secret the release is still published, with a warning, but installed apps are not
offered it. Apps from before the first signed release have no updater, so people
update to that one by hand.

To try the updater against your own server, build with
`TOKENSHARE_UPDATE_ENDPOINT=https://…/latest.json`.

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
| `packaging/` | macOS app, Windows zip and Linux `.deb` scripts, icon generator, the update key and `latest.json` writer ([updates](#updates)) |
| `design/` | the original design sketch and a plan recording the decisions |
