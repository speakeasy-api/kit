# Releasing Kit

Kit's source is public at
[`speakeasy-api/kit`](https://github.com/speakeasy-api/kit). Binary releases are
published to
[`danielkov/kit-releases`](https://github.com/danielkov/kit-releases). GitHub
Actions runs checks only; release artifacts are built and published locally.

## One-time release-machine setup

Signing and notarization run on the trusted local Mac, not in GitHub Actions.
Install the Inlucent Limited Developer ID Application identity in its Keychain
and retain the matching App Store Connect API key in 1Password. The machine also
needs a running Docker-compatible container engine and `gh` authenticated as an
account with write access to `danielkov/kit-releases`. Downloaded `.p8` keys
cannot be downloaded again, so keep the original in the company's secret
manager.

## Prepare a signed macOS release locally

Run signing and notarization on a trusted macOS machine so Apple queue delays do
not consume metered GitHub-hosted macOS runner time. Set
`KIT_NOTARY_API_KEY_DOCUMENT`, `KIT_NOTARY_API_KEY_VAULT`,
`KIT_NOTARY_API_KEY_ID`, and `KIT_NOTARY_API_ISSUER_ID` in the ignored repository
root `.env` file or export them in the release environment. The script loads
`.env` when present, reads the App Store Connect API key from 1Password, uses the
installed Developer ID identity, and preserves the exact submitted binary under
`dist/notarize/`:

```sh
caffeinate -i scripts/notarize-release.sh v0.1.29
```

The working tree must be clean when the script starts and the requested version
must match `Cargo.toml`. The script builds an immutable archive of the current
commit, records its SHA in `source-commit.txt`, and does not read later source
edits. `caffeinate` prevents idle sleep while `notarytool --wait` runs. The API
key exists only in a permission-restricted temporary directory and is deleted
when the script exits. If Apple accepts the submission, the script
creates the final macOS archive and its SHA-256 checksum. Do not delete the
output directory while a submission is active; it contains the exact signed
binary Apple is evaluating.

## Publish a signed release

Update `Cargo.toml` and `Cargo.lock` to the new version, commit every intended
release change, and run the local orchestrator from a clean working tree:

```sh
scripts/release-local.sh
```

The script runs format, Clippy, and tests; signs and notarizes macOS; builds
Linux x86-64 with a cached native ARM64 cross-build container; smoke-tests the
Linux binary in an x86-64 Debian container; generates checksums; and creates a
hidden draft release. Only after all artifacts are ready does it push the source
branch and tag, publish the draft atomically, and test installation through mise.
GitHub Actions only runs formatting, Clippy, and tests; it does not package or
publish release artifacts.

The first Linux build creates a local container image and takes longer. Later
builds reuse that image, the Cargo registry volume, and the target directory.
The lower-level `notarize-release.sh` and `build-linux-release.sh` scripts remain
available for diagnosis, but normal releases should use `release-local.sh`.

Verify the published result from a clean environment:

```sh
mise use github:danielkov/kit-releases@0.1.32
kit --version
```
