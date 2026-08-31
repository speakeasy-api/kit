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
Clippy, and tests, builds Linux x86-64 and macOS arm64 CLI archives plus the
ARM64 Kit.app ZIP, generates checksums, and publishes them to the tagged GitHub release. Prerelease tags are
marked as prereleases on GitHub.

## macOS signing and notarization

The release workflow signs the standalone Mach-O executable with the code-signing
identifier `com.speakeasy.kit`. It also archives `Kit.app`, signs its bundled helper
first with `com.speakeasy.kit`, and then signs the outer app with
`com.speakeasy.kit.desktop`. Both signatures enable the Hardened Runtime and use
an explicit empty entitlement set: the unsandboxed app and CLI need no Hardened
Runtime exceptions. The workflow submits both products to Apple's notary service,
staples and validates the app ticket, verifies nested signatures, and runs Gatekeeper
assessment before packaging `Kit-vVERSION-aarch64-apple-darwin.zip`. This Developer
ID distribution does not require an Apple App ID or provisioning profile.

Create the credentials as follows:

1. In Keychain Access, use **Certificate Assistant > Request a Certificate From
   a Certificate Authority** to save a certificate signing request (CSR).
2. In Apple Developer **Certificates, Identifiers & Profiles**, create a
   **Developer ID Application** certificate from that CSR. If that option is not
   available for your role, ask the team's Account Holder to create it. Import
   the downloaded certificate on the Mac that created the CSR.
3. In Keychain Access, export the Developer ID certificate together with its
   private key as a password-protected PKCS#12 (`.p12`) file. Record the exact
   identity shown by `security find-identity -v -p codesigning`. It normally has
   the form `Developer ID Application: <Organization> (<TEAM_ID>)`.
4. In App Store Connect **Users and Access > Integrations**, create a team API
   key that can access the notary service. Record its key ID and issuer ID, and
   retain the downloaded `.p8`; Apple does not allow it to be downloaded again.
5. Store the `.p12`, its password, and the `.p8` in the company's secret manager.
   Configure these repository Actions secrets:

   - `MACOS_CERTIFICATE_P12_BASE64`: base64-encoded `.p12`
   - `MACOS_CERTIFICATE_PASSWORD`: `.p12` export password
   - `MACOS_SIGNING_IDENTITY`: exact Keychain identity from step 3
   - `APPLE_API_KEY_P8_BASE64`: base64-encoded `.p8`
   - `APPLE_API_KEY_ID`: App Store Connect API key ID
   - `APPLE_API_ISSUER_ID`: App Store Connect issuer ID

For example, from a trusted Mac authenticated to GitHub CLI:

```sh
repo=speakeasy-api/kit
base64 < DeveloperIDApplication.p12 | gh secret set MACOS_CERTIFICATE_P12_BASE64 -R "$repo"
read -r -s 'p12_password?P12 password: '; echo
printf %s "$p12_password" | gh secret set MACOS_CERTIFICATE_PASSWORD -R "$repo"
unset p12_password
printf %s 'Developer ID Application: Example Corp (TEAMID)' | gh secret set MACOS_SIGNING_IDENTITY -R "$repo"
base64 < AuthKey_KEYID.p8 | gh secret set APPLE_API_KEY_P8_BASE64 -R "$repo"
printf %s 'KEYID' | gh secret set APPLE_API_KEY_ID -R "$repo"
printf %s 'issuer-uuid' | gh secret set APPLE_API_ISSUER_ID -R "$repo"
```

The workflow fails on a partial configuration rather than silently publishing
an unsigned build. With none of these secrets configured, it still publishes the
standalone archive and Kit.app ZIP unsigned and unnotarized, and identifies the
release as such in the release notes. XcodeGen 2.45.4 is downloaded with a pinned
SHA-256 before the app archive is generated; the checked-in project is independently
regenerated and drift-checked in CI.

## Verify a release

Install a specific published version from a clean environment:

```sh
mise use github:speakeasy-api/kit@0.1.83
kit --version
```
