# Releasing Kit

Kit's source remains private. Public, binary-only releases are published to
[`danielkov/kit-releases`](https://github.com/danielkov/kit-releases).

## One-time repository setup

Configure `KIT_RELEASES_TOKEN` on the private `danielkov/kit` repository.
It must be a fine-grained GitHub token with Contents read/write access only to
`danielkov/kit-releases`; a narrowly installed GitHub App token is preferable
when available.

Signing and notarization run on the trusted local Mac, not in GitHub Actions.
Install the Inlucent Limited Developer ID Application identity in its Keychain
and retain the matching App Store Connect API key in 1Password. The machine also
needs a running Docker-compatible container engine and `gh` authenticated as an
account with write access to `danielkov/kit-releases`. Downloaded `.p8`
keys cannot be downloaded again, so keep the original in the company's secret
manager.

Do not use the private repository's broad personal token for
`KIT_RELEASES_TOKEN`.

## Publish an unsigned prerelease

For fast internal or early testing, tag the release commit with the Cargo version
and a `-pre` suffix:

```sh
git tag v0.1.29-pre
git push origin v0.1.29-pre
```

Use `-pre.N` for additional candidates of the same Cargo version, for example
`v0.1.29-pre.2`. The prerelease workflow runs the Rust checks, builds Linux x64
and macOS arm64 in parallel, and publishes a GitHub prerelease. It does not use
the Developer ID certificate or submit the macOS binary to Apple for
notarization. Consequently, macOS Gatekeeper may require users to explicitly
approve the prerelease binary.

Prerelease tags are separate from the local signed release process. Install a specific
prerelease with:

```sh
mise use github:danielkov/kit-releases@0.1.29-pre
```

## Prepare a signed macOS release locally

Run signing and notarization on a trusted macOS machine so Apple queue delays do
not consume metered GitHub-hosted macOS runner time. The script reads the App
Store Connect API key from 1Password, uses the installed Developer ID identity,
and preserves the exact submitted binary under `dist/notarize/`:

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
No GitHub Actions workflow participates in a signed release.

The first Linux build creates a local container image and takes longer. Later
builds reuse that image, the Cargo registry volume, and the target directory.
The lower-level `notarize-release.sh` and `build-linux-release.sh` scripts remain
available for diagnosis, but normal releases should use `release-local.sh`.

Verify the published result from a clean environment:

```sh
mise use github:danielkov/kit-releases@0.1.32
kit --version
```
