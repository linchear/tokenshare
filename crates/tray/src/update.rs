//! Updating the macOS app in place from the project's GitHub releases.
//!
//! A release carries `latest.json` (the version, and per platform the URL of a
//! `.app.tar.gz` and its minisign signature). The app reads it from the newest
//! release, and if it is newer, downloads the archive, checks the signature
//! against the public key built into the app, and swaps the `.app` bundle.
//! Nothing is installed without the user choosing to. See
//! `docs/packaging.md#updates` for the release side.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use cargo_packager_updater::{Config, Update, UpdaterBuilder};

/// Where `latest.json` is read from: GitHub redirects this to the newest
/// release that is not a draft or a pre-release. A build can point elsewhere
/// with `TOKENSHARE_UPDATE_ENDPOINT` at compile time.
pub const ENDPOINT: &str = match option_env!("TOKENSHARE_UPDATE_ENDPOINT") {
    Some(e) => e,
    None => "https://github.com/linchear/tokenshare/releases/latest/download/latest.json",
};

/// The minisign public key update archives must be signed with, as the
/// `.pub` file minisign writes. Empty until the project has a signing key,
/// which leaves updates off.
const PUBLIC_KEY_FILE: &str = include_str!("../../../packaging/update-key.pub");

/// How long after starting to look for the first time, and how often after.
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(120);
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(30);

/// Where the running app is, as far as updating it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// Inside an app bundle that can be replaced: the bundle's path.
    Bundle(PathBuf),
    /// Run from a quarantined download, which macOS runs from a read-only
    /// copy ("App Translocation"). Moving it to Applications fixes that.
    Translocated,
    /// Not in an app bundle at all (`cargo run`, a bare binary). The updater
    /// would replace the binary's directory, so updates are off.
    NotBundle,
}

/// Where `exe` (the running executable) puts the app.
pub fn location(exe: &Path) -> Location {
    let s = exe.to_string_lossy();
    if s.contains("/AppTranslocation/") {
        return Location::Translocated;
    }
    let macos = exe.parent();
    let contents = macos.and_then(Path::parent);
    let bundle = contents.and_then(Path::parent);
    match (macos, contents, bundle) {
        (Some(m), Some(c), Some(b))
            if m.file_name().is_some_and(|n| n == "MacOS")
                && c.file_name().is_some_and(|n| n == "Contents")
                && b.extension().is_some_and(|e| e == "app") =>
        {
            Location::Bundle(b.to_path_buf())
        }
        _ => Location::NotBundle,
    }
}

/// The public key in the form the updater takes (the `.pub` file, base64),
/// or `None` if this build has none.
pub fn public_key(file: &str) -> Option<String> {
    let file = file.trim();
    let mut lines = file.lines();
    let comment = lines.next()?;
    let key = lines.next()?.trim();
    (comment.starts_with("untrusted comment:") && key.len() == 56 && lines.next().is_none())
        .then(|| base64::engine::general_purpose::STANDARD.encode(format!("{file}\n")))
}

/// This build's key, if it has one.
pub fn built_in_key() -> Option<String> {
    public_key(PUBLIC_KEY_FILE)
}

/// Whether this copy of the app can update itself.
pub enum Availability {
    Ready {
        exe: PathBuf,
        bundle: PathBuf,
        key: String,
    },
    /// Running from a translocated download: it has to be moved first.
    MoveToApplications,
    /// Not a bundle, or a build without a signing key.
    Off,
}

pub fn availability() -> Availability {
    let Some(key) = built_in_key() else {
        return Availability::Off;
    };
    let Ok(exe) = std::env::current_exe() else {
        return Availability::Off;
    };
    match location(&exe) {
        Location::Bundle(bundle) => Availability::Ready { exe, bundle, key },
        Location::Translocated => Availability::MoveToApplications,
        Location::NotBundle => Availability::Off,
    }
}

/// Look for a version newer than `current` at `endpoint`. `exe` is the
/// running executable, which decides what an install replaces.
pub fn check(
    current: &str,
    endpoint: &str,
    pubkey: &str,
    exe: &Path,
) -> Result<Option<Update>, String> {
    let config = Config {
        endpoints: vec![endpoint.parse().map_err(|e| format!("{e}"))?],
        pubkey: pubkey.to_string(),
        ..Default::default()
    };
    let current = current.parse().map_err(|e| format!("{e}"))?;
    UpdaterBuilder::new(current, config)
        .executable_path(exe)
        .timeout(TIMEOUT)
        .build()
        .and_then(|u| u.check())
        .map_err(|e| e.to_string())
}

/// Download `update`, check its signature, and replace the app with it.
/// Nothing is changed unless the signature is good. The running app keeps
/// running the old version until it is restarted.
pub fn install(update: &Update) -> Result<(), String> {
    update.download_and_install().map_err(|e| e.to_string())
}

/// Start the app at `bundle` again, as a new process. The caller exits
/// right after (with the delegate already stopped, so the port is free).
pub fn relaunch(bundle: &Path) -> std::io::Result<()> {
    std::process::Command::new("/usr/bin/open")
        .arg("-n")
        .arg(bundle)
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn only_a_real_bundle_can_be_updated() {
        assert_eq!(
            location(Path::new(
                "/Applications/Token Share.app/Contents/MacOS/tokenshare-tray"
            )),
            Location::Bundle("/Applications/Token Share.app".into())
        );
        for not in [
            "/Users/me/tokenshare/target/debug/tokenshare-tray",
            "/Users/me/MacOS/tokenshare-tray",
            "/tmp/Contents/MacOS/tokenshare-tray",
            "tokenshare-tray",
        ] {
            assert_eq!(location(Path::new(not)), Location::NotBundle, "{not}");
        }
        assert_eq!(
            location(Path::new(
                "/private/var/folders/x/T/AppTranslocation/0A1B/d/Token Share.app/Contents/MacOS/tokenshare-tray"
            )),
            Location::Translocated
        );
    }

    #[test]
    fn the_key_file_must_look_like_a_minisign_key() {
        assert_eq!(public_key(""), None, "no key: updates are off");
        assert_eq!(public_key("untrusted comment: x\nshort"), None);
        assert_eq!(
            public_key("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3"),
            None
        );
        let (pk, _) = keys();
        assert!(public_key(&pk).is_some());
        assert!(
            public_key(&format!("\n{pk}\n\n")).is_some(),
            "surrounding blank lines"
        );
    }

    /// A fresh key pair: the `.pub` file text and the secret key.
    fn keys() -> (String, minisign::SecretKey) {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        (kp.pk.to_box().unwrap().to_string(), kp.sk)
    }

    /// The `.app.tar.gz`, made with the same command as the release workflow,
    /// of an app whose executable prints `version`.
    fn archive(version: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir
            .path()
            .join("Token Share.app/Contents/MacOS/tokenshare-tray");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, format!("#!/bin/sh\necho {version}\n")).unwrap();
        std::fs::write(
            dir.path().join("Token Share.app/Contents/Info.plist"),
            "<plist/>",
        )
        .unwrap();
        let out = std::process::Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .args(["--no-mac-metadata", "-czf", "-", "Token Share.app"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        out.stdout
    }

    /// The `.sig` asset: the minisign signature file, base64, as the
    /// release workflow writes it.
    fn sign(sk: &minisign::SecretKey, data: &[u8]) -> String {
        let sig = minisign::sign(None, sk, data, Some("tokenshare"), None).unwrap();
        base64::engine::general_purpose::STANDARD.encode(sig.to_string())
    }

    /// Serve `files` (path -> body) over HTTP until the test ends.
    fn serve(files: Vec<(String, Vec<u8>)>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let files = Arc::new(files);
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut conn) = conn else { continue };
                let files = files.clone();
                std::thread::spawn(move || {
                    let mut line = String::new();
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    r.read_line(&mut line).unwrap();
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap() <= 2 {
                            break;
                        }
                    }
                    let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                    let reply = match files.iter().find(|(p, _)| *p == path) {
                        Some((_, body)) => {
                            let mut v = format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            v.extend_from_slice(body);
                            v
                        }
                        None => b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_vec(),
                    };
                    let _ = conn.write_all(&reply);
                });
            }
        });
        base
    }

    /// An installed app (version 0.1.0) in a temporary folder.
    fn installed_app() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir
            .path()
            .join("Token Share.app/Contents/MacOS/tokenshare-tray");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "#!/bin/sh\necho 0.1.0\n").unwrap();
        (dir, exe)
    }

    /// `latest.json` as `packaging/update-manifest.py` writes it.
    fn manifest(version: &str, base: &str, sig: &str) -> Vec<u8> {
        let out = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packaging/update-manifest.py"
            ))
            .args([
                version,
                &format!("{base}/Token-Share-macos-arm64.app.tar.gz"),
                sig,
            ])
            .output()
            .expect("python3");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    #[test]
    fn a_signed_release_is_found_downloaded_and_installed() {
        let (pk, sk) = keys();
        let key = public_key(&pk).unwrap();
        let tarball = archive("0.2.0");
        let sig = sign(&sk, &tarball);
        let base = serve(vec![(
            "/Token-Share-macos-arm64.app.tar.gz".into(),
            tarball,
        )]);
        // The manifest is served by a second server, as its URL must be known first.
        let latest = serve(vec![(
            "/latest.json".into(),
            manifest("0.2.0", &base, &sig),
        )]);
        let (_dir, exe) = installed_app();

        let update = check("0.1.0", &format!("{latest}/latest.json"), &key, &exe)
            .unwrap()
            .expect("0.2.0 is newer");
        assert_eq!(update.version, "0.2.0");
        install(&update).unwrap();
        let replaced = std::fs::read_to_string(&exe).unwrap();
        assert!(replaced.contains("0.2.0"), "{replaced}");

        // Nothing newer than what is installed.
        assert!(check("0.2.0", &format!("{latest}/latest.json"), &key, &exe)
            .unwrap()
            .is_none());
    }

    #[test]
    fn an_archive_signed_with_another_key_is_not_installed() {
        let (pk, _) = keys();
        let (_, other) = keys();
        let tarball = archive("0.2.0");
        let sig = sign(&other, &tarball);
        let base = serve(vec![(
            "/Token-Share-macos-arm64.app.tar.gz".into(),
            tarball,
        )]);
        let latest = serve(vec![(
            "/latest.json".into(),
            manifest("0.2.0", &base, &sig),
        )]);
        let (_dir, exe) = installed_app();

        let update = check(
            "0.1.0",
            &format!("{latest}/latest.json"),
            &public_key(&pk).unwrap(),
            &exe,
        )
        .unwrap()
        .unwrap();
        assert!(install(&update).is_err());
        assert!(
            std::fs::read_to_string(&exe).unwrap().contains("0.1.0"),
            "the app is untouched"
        );
    }

    #[test]
    fn a_tampered_archive_is_not_installed() {
        let (pk, sk) = keys();
        let tarball = archive("0.2.0");
        let sig = sign(&sk, &tarball);
        let mut tampered = archive("6.6.6");
        tampered.extend_from_slice(b"x");
        let base = serve(vec![(
            "/Token-Share-macos-arm64.app.tar.gz".into(),
            tampered,
        )]);
        let latest = serve(vec![(
            "/latest.json".into(),
            manifest("0.2.0", &base, &sig),
        )]);
        let (_dir, exe) = installed_app();

        let update = check(
            "0.1.0",
            &format!("{latest}/latest.json"),
            &public_key(&pk).unwrap(),
            &exe,
        )
        .unwrap()
        .unwrap();
        assert!(install(&update).is_err());
        assert!(std::fs::read_to_string(&exe).unwrap().contains("0.1.0"));
    }

    #[test]
    fn an_unreachable_server_is_an_error_not_a_crash() {
        let (pk, _) = keys();
        let (_dir, exe) = installed_app();
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/latest.json", l.local_addr().unwrap())
        };
        assert!(check("0.1.0", &dead, &public_key(&pk).unwrap(), &exe).is_err());
    }
}
