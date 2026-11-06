#!/usr/bin/env python3
"""Print latest.json, which tells installed apps about a release.

    update-manifest.py VERSION URL SIGNATURE [PLATFORM]

URL is where the signed .app.tar.gz is downloaded from, SIGNATURE the
contents of its .sig file (the minisign signature, base64), and PLATFORM the
updater's name for the platform, macos-aarch64 by default. The tray app's
updater (crates/tray/src/update.rs) reads this format.
"""
import json
import sys
from datetime import datetime, timezone


def main(argv):
    if len(argv) not in (4, 5):
        sys.exit(__doc__.strip())
    version, url, signature = argv[1:4]
    platform = argv[4] if len(argv) == 5 else "macos-aarch64"
    if not signature.strip():
        sys.exit("the signature is empty")
    manifest = {
        "version": version.removeprefix("v"),
        "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": {
            platform: {"url": url, "signature": signature.strip(), "format": "app"}
        },
    }
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main(sys.argv)
