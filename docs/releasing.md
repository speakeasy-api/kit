# Releasing Kit

Kit's source and binary releases are public at
[`speakeasy-api/kit`](https://github.com/speakeasy-api/kit).
GitHub Actions builds and publishes release artifacts from version tags.

## Publish a release

Update `Cargo.toml` and `Cargo.lock` to the new version, commit the release, then
push a matching tag:

```sh
git tag v0.1.83
git push origin v0.1.83
```

For a prerelease, use `-pre` or `-pre.N`, for example `v0.1.83-pre.1`.
The release workflow verifies the tag against `Cargo.toml`, runs formatting,
Clippy, and tests, builds Linux x86-64 and macOS arm64 archives, generates
checksums, and publishes them to the tagged GitHub release. Prerelease tags are
marked as prereleases on GitHub.

## Optional macOS signing and notarization

The macOS build does not require Apple credentials. When none are configured,
the workflow publishes an unsigned, unnotarized archive and identifies it as
such in the release notes. This is the expected setup until signing credentials
are added to the repository.

To enable signing and notarization, configure all of these repository secrets:

- `MACOS_CERTIFICATE_P12_BASE64`
- `MACOS_CERTIFICATE_PASSWORD`
- `MACOS_SIGNING_IDENTITY`
- `APPLE_API_KEY_P8_BASE64`
- `APPLE_API_KEY_ID`
- `APPLE_API_ISSUER_ID`

The workflow fails on a partial configuration rather than silently publishing
an unsigned build. Downloaded `.p8` keys cannot be downloaded again, so retain
the original in the company's secret manager.

## Verify a release

Install a specific published version from a clean environment:

```sh
mise use github:speakeasy-api/kit@0.1.83
kit --version
```
