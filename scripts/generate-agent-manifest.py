#!/usr/bin/env python3
"""Generate the ACP registry manifest from the final stable release archives.

Contract: https://github.com/agentclientprotocol/registry/blob/
d004fca664b46aa8fbddc04ad26eea0acb2f37a3/FORMAT.md
Only platforms published by the CLI release jobs are advertised.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re


PLATFORMS = {
    "darwin-aarch64": "aarch64-apple-darwin",
    "linux-x86_64": "x86_64-unknown-linux-gnu",
}
REPOSITORY = "https://github.com/speakeasy-api/kit"


def manifest(version: str, directory: Path) -> dict:
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("ACP stable manifests require an X.Y.Z release version")
    binary = {}
    for platform, target in PLATFORMS.items():
        archive = f"kit-v{version}-{target}.tar.gz"
        with (directory / archive).open("rb") as stream:
            digest = hashlib.sha256()
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
            checksum = digest.hexdigest()
        binary[platform] = {
            "archive": f"{REPOSITORY}/releases/download/v{version}/{archive}",
            "sha256": checksum,
            "cmd": "./kit",
            "args": ["acp"],
        }
    return {
        "id": "kit",
        "name": "Kit",
        "version": version,
        "description": "A coding agent from Speakeasy",
        "repository": REPOSITORY,
        "authors": ["Speakeasy"],
        "license": "MIT",
        "license_url": f"{REPOSITORY}/blob/v{version}/LICENSE",
        "distribution": {"binary": binary},
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    # Resolve every archive before writing, so missing artifacts fail publication.
    result = manifest(args.version, args.directory)
    (args.directory / "agent.json").write_text(
        json.dumps(result, indent=2) + "\n", encoding="utf-8"
    )


if __name__ == "__main__":
    main()
