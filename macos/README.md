# Kit Desktop for macOS (v0)

A dependency-free SwiftUI client for Kit's native ACP server. It targets macOS 14, disables App Sandbox so Kit and its tools can access workspaces, and launches one retained Kit helper process per opened conversation. Switching conversations never cancels their work.

## Prerequisites

- macOS 14 or newer
- Xcode 16 or newer
- Pinned [XcodeGen](https://github.com/yonaskolb/XcodeGen) 2.45.4 (`scripts/install-xcodegen.sh` installs and verifies the release archive)
- Rust toolchain required by the repository

## Generate, test, build, and run

Run from the repository root:

```sh
scripts/install-xcodegen.sh
scripts/generate-acp-swift.py --check
cargo build --locked --bin kit
scripts/generate-macos-project.sh

xcodebuild \
  -project macos/KitDesktop.xcodeproj \
  -scheme KitDesktop \
  -configuration Debug \
  -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath macos/.build \
  CODE_SIGNING_ALLOWED=NO \
  KIT_BINARY=$PWD/target/debug/kit \
  test

xcodebuild \
  -project macos/KitDesktop.xcodeproj \
  -scheme KitDesktop \
  -configuration Debug \
  -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath macos/.build \
  CODE_SIGNING_ALLOWED=NO \
  KIT_BINARY=$PWD/target/debug/kit \
  build

open macos/.build/Build/Products/Debug/Kit.app
```

A Release build must contain an optimized helper and fails rather than creating an incomplete app:

```sh
cargo build --locked --release --target aarch64-apple-darwin --bin kit
scripts/generate-macos-project.sh
xcodebuild \
  -project macos/KitDesktop.xcodeproj \
  -scheme KitDesktop \
  -configuration Release \
  -destination 'generic/platform=macOS' \
  -derivedDataPath macos/.build-release \
  CODE_SIGNING_ALLOWED=NO \
  build
```

Project generation rejects other XcodeGen builds, including a same-version Homebrew binary, unless they came from the checksum-pinned archive installed by the repository script. The generated `Config/Version.xcconfig` is intentionally untracked: every project generation derives it from `Cargo.toml`, so a package version bump does not require a second manual edit. CI regenerates that local version configuration and fails only when the checked-in Swift models or Xcode project drift.

The build phase always removes the previous helper before atomically copying the selected executable to `Kit.app/Contents/Helpers/kit`; dependency analysis cannot preserve a stale copy. Release ignores `KIT_BINARY` and requires the exact `target/aarch64-apple-darwin/release/kit` output. When no bundled helper exists, runtime lookup honors `KIT_BINARY`, the inherited `PATH`, common Homebrew locations, `~/.cargo/bin`, and the user's login-shell `PATH`. Debug builds also check the repository's `target/debug/kit`.

The app and helper are intentionally thin ARM64 binaries. Kit's current release workflow publishes macOS only for `aarch64-apple-darwin`, so advertising x86_64 desktop support would bundle a helper the release pipeline does not produce. The generated app marketing version and ACP `info.version` derive from the root Cargo package version through `Config/Version.xcconfig` and the app bundle.

## Architecture and protocol

- `AppModel` owns a controller dictionary keyed by conversation ID. Each controller and helper remain alive across sidebar/workspace navigation, so multiple conversations can run independently and update unread/awaiting-user state.
- `ACPClient` launches `kit serve --stdio-protocol-version 2` with root and optional model defaults plus `KIT_RUNTIME_EVENTS=1`. It strictly negotiates ACP v2 and uses typed `session/new`, `session/resume` (with replay from start), cursor-based `session/list`, prompt acceptance, cancel, and close.
- A serial transport queue owns newline framing, JSON decoding, per-session routing, pending requests, timeouts, ordered writes, stderr event parsing, and process shutdown. Replay updates are delivered before the response that completes resume. UI callbacks are delivered on the main actor.
- Streaming text is coalesced to about 30 updates per second. Transcript count, stream text, parser lines, diagnostics, and raw tool output are bounded. Markdown is parsed only after a stream completes.
- The UI supports typed select/boolean config values, context and completed-turn token usage, available-command discovery, copy-last-response, notices, compaction, diagnostics, nested runtime graph rows, foreground cancel, compose detach, and detached-call cancellation. Slash-prefixed text is submitted unchanged; the desktop does not implement TUI command parsing.
- Attachments match the TUI: PNG/JPEG/GIF/WebP and WAV/MP3 are base64 ACP image/audio blocks, limited to 8 files, 10 MiB each, and 20 MiB total.
- `PersistenceStore` uses schema version 3, migrates the versionless v1 state, atomically saves on a utility queue, keeps a validated backup, quarantines corrupt primary files, and refuses unsupported newer schemas. State lives at `~/Library/Application Support/KitDesktop/state.json`; Kit remains the transcript source of truth.
- The pinned schema is in `macos/ACP/Schema`; `scripts/generate-acp-swift.py` deterministically produces the dependency-free Codable wire subset without network access. `ACPClient.swift` is the small handwritten Kit transport/extension layer.
- Process tests launch the shared ACP v2 `fixtures/mock-acp-v2.py`, cover strict negotiation, typed updates, rich streaming/media, prompt acceptance, slash-text preservation, cancel/close, and verify resume replay arrives before the resume response. CI also launches the real `target/debug/kit` lifecycle smoke coverage.

## Current limitations

- Conversation rename/delete/search and transcript export are not part of v0.
- Unknown agent-to-client request methods receive JSON-RPC `Method not found`. Permission requests are safely cancelled because the desktop does not yet expose an interactive permission surface; filesystem, terminal, authentication, and elicitation client capabilities are not advertised.
- Notifications require macOS permission and are posted for completed turns while the app is inactive.
