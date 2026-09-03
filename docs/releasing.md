# Releasing Kit

Kit's source and binary releases are public at
[`speakeasy-api/kit`](https://github.com/speakeasy-api/kit).
GitHub Actions builds and publishes release artifacts on demand.

## Publish a release

Ordinary pull requests do not update release versions. In GitHub, open
**Actions > release**, select **Run workflow** on `main`, and run it. Do not create
or push the release tag first.

The workflow verifies that the version in `Cargo.toml` matches the latest release
tag, then examines commits since that tag. Any breaking commit advances the minor
version; other commits advance the patch version. It prepares a release commit that
updates `Cargo.toml` and `Cargo.lock`, then runs
formatting, Clippy, tests, artifact builds, and release-note generation against that
exact commit.

After every required build succeeds, the workflow atomically advances `main` to the
release commit and creates the release tag. If `main` advances while the workflow is
running, the release stops without updating either ref and must be run again. A
rerun of the failed jobs resumes publication when the release commit and tag were
pushed but a later publishing step failed.

The atomic push authenticates with the repository's write-enabled `kit-release`
deploy key. Add **Deploy keys** to the `main` ruleset bypass list and store that
key's private half in the repository Actions secret `RELEASE_DEPLOY_KEY`. The
default `GITHUB_TOKEN` cannot bypass the pull-request and status-check rules.

The newly built Linux Kit CLI reviews every change since the previous release and
writes the GitHub release notes before the workflow attaches the archives and
checksums. Configure the repository Actions secret `OPENROUTER_API_KEY` for this
step. Kit uses the `release-notes` skill with the `anthropic/claude-sonnet-5` OpenRouter model.

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
