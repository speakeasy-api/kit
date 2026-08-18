# Releasing Kit

Kit's source remains private. Public, binary-only releases are published to
[`danielkov/kit-releases`](https://github.com/danielkov/kit-releases).

## One-time repository setup

Configure these Actions secrets on the private `danielkov/kit` repository:

- `KIT_RELEASES_TOKEN`: a fine-grained GitHub token with Contents read/write
  access only to `danielkov/kit-releases`. A narrowly installed GitHub App token
  is preferable when available.
- `MACOS_CERTIFICATE_P12_BASE64`: base64-encoded Developer ID Application
  certificate and private key exported as PKCS#12.
- `MACOS_CERTIFICATE_PASSWORD`: password for that PKCS#12 export.
- `MACOS_SIGNING_IDENTITY`: the full Developer ID Application identity.
- `APPLE_API_KEY_P8_BASE64`: the base64-encoded `.p8` private key for an App
  Store Connect team API key.
- `APPLE_API_KEY_ID`: key ID shown for that API key.
- `APPLE_API_ISSUER_ID`: issuer ID shown under App Store Connect API access.

Create the certificate and API key under the Inlucent Limited Apple Developer team.
The Developer ID certificate and notarization API key must belong to that same
provider. Downloaded `.p8` keys cannot be downloaded again, so retain the original
in the company's secret manager. Encode it without line wrapping before adding the
secret:

```sh
base64 < AuthKey_KEYID.p8 | tr -d '\n' | \
  gh secret set APPLE_API_KEY_P8_BASE64 --repo danielkov/kit
```

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

## Publish a signed release

1. Update `Cargo.toml` and `Cargo.lock` to the new version.
2. Merge the release commit to the default branch.
3. Tag that commit with the matching `v`-prefixed version and push it:

   ```sh
   git tag v0.1.28
   git push origin v0.1.28
   ```

The signed release workflow verifies the tag, runs the Rust checks, builds Linux
x64 and macOS arm64 binaries, signs and notarizes macOS, generates SHA-256
checksums, and creates the public release. A missing target or credential fails the release
rather than publishing a partial one.

Verify the published result from a clean environment:

```sh
mise use github:danielkov/kit-releases@0.1.28
kit --version
```
