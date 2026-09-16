# Native voice

Talk to Kit in the terminal using your ChatGPT subscription. Native voice is
**experimental and off by default**. It is included in the packaged desktop
release binaries—you do not need to build Kit from source.

## What you need

- The [installed Kit binary](getting-started-and-configuration.md#install-and-verify-the-kit-binary)
  with the terminal UI. The headless container images do not include voice.
- A ChatGPT account with access to the subscription voice service, signed in
  through Kit. Voice uses Kit's OpenAI login, not an API key or another app's login.
  Service availability can vary by account.
- A working default microphone and audio output. Use headphones: voice does not
  currently provide acoustic echo cancellation.
- On Linux, the optional [audio runtime libraries](getting-started-and-configuration.md#linux-runtime-libraries)
  and access to a working audio service or device. Development headers and build
  tools are not required to use the packaged binary.

Starting a voice call can consume subscription quota. Enabling the feature or
launching Kit does not start a call or turn on your microphone.

## Enable voice and connect

Enable voice, sign in if needed, and start the terminal UI:

```sh
kit config set experimental.voice true
kit auth login openai --credential-store keychain
kit tui --root /path/to/project --credential-store keychain
```

Use the same credential store for login and the TUI. If you already use file-backed
credentials, keep using `--credential-store file --credential-dir <private-directory>`
for both commands instead of switching to keychain.

If Kit is already running, restart the TUI after changing the setting. Then enter:

```text
/voice on
```

Once connected, Kit listens through your default microphone and plays replies
through your default audio output.

## Control your call

| Command | Effect |
| --- | --- |
| `/voice on` | Start a call, or resume the microphone in an existing muted call. |
| `/voice mute` | Pause microphone capture while keeping the call open. Audio already sent cannot be recalled. |
| `/voice off` | End the call and release the audio devices. Also cancels a connection in progress. |

The microphone stays enabled until you mute or end the call. Switching Kit
sessions or exiting the TUI also ends the call. If the connection fails, Kit stops
the call; it does not reconnect automatically. Use `/voice on` to try again.

To hide the voice controls, run `kit config set experimental.voice false` and
restart the TUI. Use `/voice off` to end a call in the meantime.

## Ask Kit to do work

You can talk with the voice assistant and ask it to pass tasks to your current
Kit session. Speech transcripts appear in the TUI, but each transcript is not
automatically submitted as a prompt: work starts when the voice assistant hands
a task to Kit.

These tasks use the same tools, permissions, and approval UI as typed prompts.
If Kit is already working, another voice task is rejected rather than replacing
or redirecting the current task. The completed task's final response and status
are sent back to the voice service; internal reasoning and raw tool output are not.

Ending a call does **not** cancel work Kit has already accepted. Use Kit's normal
turn cancellation to stop that work.

## Current limitations

- Voice does not automatically receive your existing Kit conversation. Explain
  the context it needs in the call.
- Voice uses the default input and output devices. Unsupported audio configurations
  are reported before a call is created.
- Restricted networks and some VPN configurations can prevent voice from connecting.
  Voice requires UDP connectivity and does not have a relay fallback.
- Heavy congestion can stop a call. Long calls also have a limit of 256 task
  handoffs; reconnect manually if you reach it.

## Troubleshooting

### Voice commands are unavailable

Check `kit config get experimental.voice`. Set it to `true` and restart the TUI.
Voice is not available in the headless container images.

### Microphone or playback fails

Check your system's default input and output devices and microphone permissions.
On Linux, check that the [audio runtime libraries](getting-started-and-configuration.md#linux-runtime-libraries)
are installed and that your audio service is running. Missing libraries or unusable
devices cause a local error before Kit creates a subscription call.

If you hear echo, use headphones.

### Authentication or access is denied

Confirm that login and the TUI use the same credential store and ChatGPT account.
For authentication errors, sign in again with `kit auth login openai` using that
store.

An HTTP 403 does not by itself mean your account lacks voice access. If the error
persists, contact support with the sanitized error line and approximate attempt
time. Review diagnostics before sharing them; do not share credentials or raw
request headers or bodies. Kit does not retry calls automatically or fall back
to an API key. Each manual retry may consume subscription quota.

### A VPN or multiple network interfaces prevent connection

Kit normally chooses the default IPv4 network interface. To select another local
interface, set `KIT_VOICE_BIND` before launching the TUI, using an IP address
assigned to your machine:

```sh
KIT_VOICE_BIND=192.168.1.20:0 kit tui --root /path/to/project --credential-store keychain
```

Replace the example IP with your local interface's address. Port `0` lets the
system choose an available port. This environment setting is not saved in Kit's
configuration and does not bypass network restrictions.
