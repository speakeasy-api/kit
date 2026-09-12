# Experimental native subscription voice

> **Experimental, opt-in feature.** Native voice is included in every build but disabled by default. Set `experimental.voice = true` in `~/.kit/config.toml` and restart Kit to make the controls available. This setting is snapshotted at TUI startup; config edits do not change a running TUI. Starting Kit does not start a voice call. Use `/voice on` explicitly to connect and start microphone capture.

Native voice is a runtime opt-in TUI feature. It connects directly to the ChatGPT subscription realtime service using Kit's own OpenAI login. It does not run Codex, use its app-server or credential files, or open a browser for audio. Ordinary Kit OAuth login can still require a browser. There is no API-key fallback.

**Live service access, account eligibility, and physical audio quality require manual validation.** Offline tests cannot establish these. `/voice on` creates a subscription call and can consume subscription quota; connection creation is never automatically retried. The service is an experimental, source-derived contract, not a guaranteed public API.

## Build, authenticate, and launch

Release binaries include voice through the default `tui` Cargo feature; the container images build with `--no-default-features` and omit both the terminal client and voice. Building the full binary needs Rust, a C/C++ toolchain, and the platform's audio development libraries (ALSA and PulseAudio development headers on Linux). The pure-Rust Opus codec does not need CMake. The existing `aws-lc-rs` crypto backend is unchanged and can still require CMake on some targets/configurations. On macOS, install the Xcode Command Line Tools; install CMake if the crypto build requests it. On Debian/Ubuntu, install `build-essential cmake pkg-config libasound2-dev libpulse-dev`. On Alpine, install `build-base cmake linux-headers perl pkgconf alsa-lib-dev pulseaudio-dev`. Windows needs the MSVC C++ build tools and may need CMake for the crypto build. Cross-platform builds and physical devices still need validation on each target.

Linux uses cubeb's production C ALSA/PulseAudio backends with lazy library loading.
Audio libraries are optional at runtime and are loaded only for voice. Missing
libraries or usable devices cause a local error before a subscription call is
created. See [Linux runtime libraries](getting-started-and-configuration.md#linux-runtime-libraries)
for optional package names and container requirements. The Linux native build
requires CMake; the pure-Rust Opus codec does not.

```sh
cargo build --locked --release
./target/release/kit config set experimental.voice true
./target/release/kit auth login openai --credential-store keychain
./target/release/kit tui --root /path/to/project --credential-store keychain
```

Use the same credential backend for login and the TUI. File storage is also supported through `--credential-store file --credential-dir <private-directory>`. Do not copy tokens out of another application's credential file. Kit and Codex sharing an OAuth client ID does not establish token compatibility or service entitlement.

## Controls

Wear headphones before connecting. This initial implementation has no acoustic echo cancellation.

| Command | Effect |
| --- | --- |
| `/voice on` | Create a subscription voice call and start listening, or resume microphone capture in the existing call. |
| `/voice mute` | Request microphone pause. Audio already sent cannot be recalled. |
| `/voice off` | Stop voice, release audio devices, and close the connection. |

Microphone capture stays enabled until `/voice mute` or `/voice off`. Muting keeps the subscription session open; `/voice on` resumes listening without creating another call. Muting during connection keeps capture paused when the call becomes ready. Startup remains cancellable with `/voice off`. Session changes and TUI exit stop voice. A transport error stops the call; reconnect manually. Stopping voice does not cancel an agent task that was already accepted: use Kit's normal turn cancellation for that.

Speech transcripts are displayed, not automatically executed as prompts. Only explicit service delegation events submit work to the current Kit session. Those tasks use the same tools, permissions, and approval UI as typed prompts. A busy session rejects another voice delegation rather than silently steering or replacing a turn. Final assistant prose and the turn's stop status return to the voice service; reasoning and raw tool output are not forwarded.

## Passive agent awareness and background work

After the voice connection is ready, the TUI sends a private `kit/voice/state`
ACP notification for the current Kit session. Config opt-in and a pending or
failed connection do not announce active voice. Mute/unmute do not change this
state. Turning voice off, a failure, or disconnection clears it; before switching
ACP sessions, the TUI clears the old session's state. Exit sends a best-effort
nonblocking clear, with server connection teardown as the cleanup fallback.

This is status, not a prompt. It never starts a model turn, interrupts live work,
or inserts a notice at the next live tool boundary or autonomous background
completion. The runtime delivers internal voice-state guidance only on the next
genuine **user turn**, like skill notifications. If no user turn follows, there
is no model execution to deliver the notice. Speech transcripts remain display-only;
an explicit accepted voice delegation is user-submitted work, not a state notice.

When voice is active, that guidance asks the agent to keep conversation responsive
and use background work for longer independent tasks when appropriate. It does
not itself launch a task, change approvals, or grant permission for additional
work. Existing accepted work continues after `/voice off`; use normal cancellation
to stop it.

## Initial limits

- Default input/output devices must support integer or floating-point PCM at 8–192 kHz with 1–32 channels. Capture channels are averaged to mono; playback is duplicated to all channels. A streaming low-pass resampler converts between device rates and the 48 kHz Opus clock. Unsupported configurations are reported before call creation.
- Native Opus/WebRTC uses a local UDP interface and remote ICE candidates. No STUN/TURN discovery or relay is configured. Restricted networks can fail.
- The default interface is selected from the OS IPv4 route without sending a probe packet. On multihomed/VPN systems, set `KIT_VOICE_BIND` to a concrete local socket address, for example `192.168.1.20:0`; port zero requests an ephemeral port. This setting is not persisted.
- Protocol/audio queues and message sizes are bounded. Congestion stops the call instead of silently dropping delegated work. A session stops after 256 unique delegation IDs; reconnect manually.
- Voice does not automatically upload the existing Kit conversation. Only speech and accepted delegated-task replies provide voice context in this initial scope.

## Required manual validation

Run these checks manually only when you accept subscription usage and microphone access. Automated development checks must not create paid calls or activate audio devices.

1. Log in using Kit and the same persistent credential backend as the TUI. Use headphones and run `/voice on`; microphone capture starts when connected. Confirm subscription authentication succeeds with `gpt-live-1-codex`. A 401/403 or entitlement error must stop without API-key fallback.
2. Confirm the operating system's microphone permission prompt and selected devices are appropriate. Verify that capture starts without another command. Speak a harmless greeting, then `/voice mute`. Confirm intelligible two-way speech and that speech after mute is not transmitted. Run `/voice on` again and confirm capture resumes in the same call.
3. Ask for a read-only task such as a repository summary. Confirm a delegation appears in the existing Kit session and its final answer is spoken. Ordinary conversation must not create agent tasks.
4. Ask for a task that requires an existing approval. Confirm the normal approval UI is shown; reject it and verify the voice response accurately reports the outcome. Do not approve destructive work merely to test voice.
5. Start a typed Kit task and issue a voice delegation. Confirm the busy rejection and no turn replacement. Disconnect voice during another accepted task; confirm the task continues until normal cancellation.
6. Try `/voice off` during connection and during speech, then change/resume sessions and quit. Confirm audio stops, device indicators clear, and old replies do not reach a subsequent voice call.
7. Disconnect the network. Confirm voice fails visibly, releases devices, and does not reconnect or create another quota-consuming call automatically.

## Dependency scope and offline validation

The included native stack is `str0m =0.23.1` (defaults disabled, `aws-lc-rs`), `cubeb`, `cubeb-core`, and `cubeb-sys =0.38.0` on Linux, `cpal =0.18.2` (defaults disabled) on non-Linux platforms, `opus-pure =0.2.1` (defaults disabled; no dependencies or native build script), and `tokio-tungstenite =0.29.0` (defaults disabled, `handshake`). Relative to the preceding voice worktree, this codec switch adds only `opus-pure 0.2.1` and removes `opus 0.4.0` and `opusic-sys 0.7.5`. It does not upgrade any existing release. `cmake` remains locked through the existing crypto stack.

Activation was explicitly approved by the user. The earlier dependency assessment collected archive/source evidence but did **not** finish its deeper semantic/native/transitive review; user approval does not make that review complete. The prior review limitations still apply to the unchanged native/crypto stack; switching the codec does not complete that broader review. Cross-platform native bindings and publisher/tag provenance remain residual review limitations. RustSec checks are not proof of native-code safety.

The codec sends 48 kHz mono float PCM in 960-sample (20 ms) packets, with an
explicit 51 kbps target, complexity 9, and constrained VBR. These match the
previous libopus automatic bitrate for that rate/channel/frame configuration
and its complexity/rate-control defaults, not a promise of identical audio or
bitstreams. Talkspurt resets retain these settings. Receive buffers hold 5,760
samples (120 ms); playback uses the decoder's actual packet duration, not the
buffer capacity.

Codec audit: the published SHA-256 is
`de6a3cef053e1fdea38a5e50edb0789590a31184c5a59f6cd5e462fe844793e7`.
The archive matches registry metadata and upstream commit
`4a9f7231aef586c4d6365080288d513a393f71ca` (`v0.2.1`), except Cargo's
normalized manifest, VCS metadata and generated dependency-free lockfile.
Registry owner and publisher are Stephen Berry (`stephenberry`), also the
publisher of the two earlier releases. The release is not yanked; its license
is BSD-3-Clause and MSRV is 1.88 (below Kit's). It has no build script, proc
macro, runtime/build/dev dependencies, or default features; `probe` stays off.
Source inspection found no library process/network/download behavior, but the
codec does contain unsafe architecture-specific SIMD. This is a scoped source
and provenance review, not a full codec security verification. Sole ownership,
a lightweight unsigned release tag, and the young fork are residual risks.
Upstream has unreleased Ogg packet-size limiting, encoder range-buffer assertion
hardening, and allocation improvements: this integration uses raw RTP packets,
not Ogg, but the pin does not include those changes. The range-buffer change
adds release assertions to internal encoder size invariants; it is not a
replacement for decoder input validation. RustSec reported no vulnerabilities
and one pre-existing yanked package (`chacha20 0.10.1`).

Reproduce offline validation from the repository root (no credentials or audio devices required):

```sh
cargo check --locked
cargo test --locked --lib voice
cargo test --locked --lib tui::command::tests
cargo clippy --locked --lib --bin kit -- -D warnings
```

The voice tests cover in-memory WebRTC/Opus, local HTTP/WebSocket upgrades, protocol shapes, and TUI lifecycle behavior. These do not validate physical audio or subscription service access; use the manual checklist above for those.

## Contract provenance

The inspected source checkout is `c4017a87aacc7558002b7cb510025e967c1d765e`. The supplied shorter revision did not exactly identify that checkout.

- Subscription signaling: `POST https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas`, with JSON `{sdp, session}`. The public API's multipart branch is not this subscription route.
- Native media: Opus RTP over ICE/DTLS/SRTP, with the negotiated `oai-events` SCTP channel.
- Control: source-matched authenticated v3/frameless sideband; delegation is `delegation.created`, replies are `delegation.context.append`. The data channel is not treated as delegated-work authority.

The browser/Codex spike under `scripts/voice-spike` is independent scaffolding and is not launched by this feature.

## Diagnosing subscription signaling denials

A `realtime signaling returned HTTP 403` does **not** by itself prove that the
account lacks voice entitlement. Source comparison and a bounded no-audio live
diagnostic found the following:

- The direct subscription endpoint/query, `gpt-live-1-codex` model, JSON
  `{sdp, session}` shape and client delegation match the Frameless Bidi request
  builder. No public-API multipart conversion is needed. However, the authentic
  Codex v3 request uses `audio: {"output": {"voice": "cove"}}`, not `marin`.
  Upstream `default_realtime_voice` selects the v1 voice set for v3 (`cove` default);
  `marin` belongs to the v2 set. Kit now uses `cove`.
- A single Python-transport probe using Kit's own credentials and truthful
  `kit/0.1.133` User-Agent / `0.1.133` version, changing only the output voice from
  the earlier denied payload to `cove`, returned HTTP 201. The diagnostic immediately
  sent `session.close` over the sideband and closed the socket. No microphone,
  recording, audio transmission, or delegated work was used. This validates call
  creation for that attempt, not physical audio or all accounts. The harness was
  0.1.131; the probe correctly read the repository package version, 0.1.133.
- Kit supplies the same bearer/account headers and `openai-alpha: quicksilver=v2`.
  Credentials come from Kit's selected storage and account binding, not Codex files.
- The OAuth client ID and requested scopes match upstream: `openid profile email
  offline_access api.connectors.read api.connectors.invoke`. This is source-level
  parity, not verification of any existing token's granted scopes.
- Upstream transport supplies both originator and User-Agent. Native signaling
  previously omitted User-Agent; it now sends `kit/<version>` and keeps the truthful
  `originator: kit`. The spike initializes Codex with client name `kit_voice_spike`;
  its success does not prove native Kit identity has identical service policy.
- Upstream `model-provider-info::create_openai_provider` also sets a distinct
  `version` header from its package version; provider headers reach HTTP signaling
  and the realtime WebSocket header merge. Kit omitted this header and now sends
  its own `CARGO_PKG_VERSION` on signaling and the retained sideband headers, not a
  Codex version. This corrects a source-confirmed omission, not a proven 403 fix.
- Upstream also supports optional `x-session-id`, conditional attestation and
  configured residency headers. Kit does not fabricate attestation or residency
  claims, impersonate Codex, or attempt browser challenge bypasses. These differences
  are not proven causes of the observed denial.

Signaling errors report categories for an explicit edge challenge, HTML response,
selected known JSON error codes, or an unclassified denial. Error bodies are read
only up to 16 KiB. Diagnostics can include bounded remote messages, error codes,
and request IDs after control-character filtering and best-effort redaction of
credential labels and opaque words. Arbitrary remote prose cannot be guaranteed
secret-free: review diagnostics before sharing them. The category is a diagnostic
hint, not proof of which infrastructure denied access.
No call is automatically retried, and no API-key fallback is used.

Rebuild locally (no credential access is required; add CMake to `PATH` only if the existing crypto build needs it):

```sh
cargo build --offline --locked --release
./target/release/kit
```

If you choose to retry `/voice on`, it contacts the subscription service and may
consume quota. Share only the new sanitized error line. For an edge challenge or
HTML denial, ask the service operator whether direct native subscription signaling
is supported; do not add browser automation. For a scope or authentication denial,
confirm the selected Kit credential backend/account before deciding to log in again.
For an unclassified denial, ask the operator to investigate the route and client
identity using the approximate attempt time. Do not collect raw headers/bodies or
export credentials. Header corrections alone were not demonstrated to resolve
this 403; the supported-voice probe above succeeded without impersonating Codex.

Linux pins the cubeb family to 0.38.0. The `cubeb-sys` `unittest-build` feature
selects real production C backends by disabling `BUILD_RUST_LIBS`; it does not
replace audio with test stubs. This avoids the unlocked nested Cargo build for
the Rust backend. `LAZY_LOAD_LIBS` stays enabled. Re-audit this upstream feature
and bundled licenses on every upgrade. Controlled builds install ALSA/PulseAudio
headers but no optional JACK, sndio, or system SpeexDSP development packages,
and use a fresh CMake cache without backend or lazy-loading overrides.
