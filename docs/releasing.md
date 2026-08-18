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
and retain the matching App Store Connect API key in 1Password. Downloaded `.p8`
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

Prerelease tags do not trigger the signed release workflow. Install a specific
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

1. Update `Cargo.toml` and `Cargo.lock` to the new version and commit it.
2. Prepare the signed and notarized macOS archive locally as described above.
3. Create a hidden draft release containing that archive:

   ```sh
   version=0.1.31
   gh release create "v$version" \
     "dist/notarize/v$version/kit-v$version-aarch64-apple-darwin.tar.gz" \
     --repo danielkov/kit-releases \
     --target main \
     --draft \
     --title "Kit $version" \
     --notes "Prebuilt Kit binaries. Source code is maintained separately."
   ```

4. Push the release commit and its matching source tag:

   ```sh
   git push origin HEAD
   git tag v0.1.31
   git push origin v0.1.31
   ```

The signed release workflow verifies the tag, runs the Rust checks, builds only
Linux x64, downloads the locally prepared macOS archive from the draft, creates
`SHA256SUMS`, and publishes the draft atomically. It uses no GitHub-hosted macOS
runner and fails rather than publishing without both platform archives.

Verify the published result from a clean environment:

```sh
mise use github:danielkov/kit-releases@0.1.28
kit --version
```
