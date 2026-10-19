# Run a Speakeasy assistant with a provisioned credential

Use this guide to run Kit for an AI Control Plane (AICP) assistant without an
interactive provider login. One assistant-wide credential can serve multiple ACP
sessions, each associated with its own control-plane chat.

Before starting, obtain a durable, revocable inference credential from your
control plane, the Speakeasy server origin, and an eligible model ID. Your ACP
client must supply the canonical chat association described in the
[adapter reference](../reference/speakeasy-assistant-inference.md#associate-an-acp-session-with-a-chat).
A standalone `kit prompt` or ordinary terminal client does not supply that
association and cannot use this credential by itself. For interactive dashboard
login instead, see [Speakeasy setup](getting-started-and-configuration.md#speakeasy-ai-control-plane).

## Provisioning

Have your deployment's secret manager supply a JSON file with these fields:

```json
{
  "version": 1,
  "bearer_token": "<durable credential issued by AICP>",
  "origin": "https://your-gram-origin.example"
}
```

Use the durable assistant inference credential, not a short-lived turn token.
Kit does not mint or refresh it. Arrange replacement through the control plane
before expiry; after replacing it, restart the Kit runtime so sessions use the
replacement. Revoke a compromised credential at the control plane.

Install the file before starting Kit. Run these commands as the runtime user;
replace the source path with the file supplied by your secret manager:

```sh
credential_dir="$HOME/.kit/credentials"
credential_name=f5a93b2411bbf44d12df4db53aa064d87685df4fad637f3c61a653f4bc5b200f.json
install -d -m 700 "$credential_dir"
install -m 600 /run/secrets/speakeasy-runtime.json \
  "$credential_dir/$credential_name"
```

Use a real, private directory and a regular file owned by the runtime user; do
not use symlinks. Do not put the token in ACP metadata, prompts, command-line
arguments, or logs. This provider credential is separate from the credential
used to access a [Kit gateway](gateway.md#start-the-host).

Configure your ACP client to launch Kit with the same credential directory.
Replace the project path and model with an existing directory and a model your
Speakeasy server offers:

```sh
export SPEAKEASY_ORIGIN='https://your-gram-origin.example'
kit acp --protocol-version 2 \
  --provider speakeasy --model provider/eligible-model \
  --root /absolute/project \
  --credential-store file --credential-dir "$credential_dir"
```

The process serves ACP on standard input/output; it is not an interactive shell.
After the client creates a session with its chat association, Kit advertises the
eligible models and can run prompts against that chat. Runtime credentials
require an explicit `SPEAKEASY_ORIGIN` matching the JSON `origin`. Use an origin
only: no URL credentials, endpoint path, query, or fragment. Invalid records or
origin mismatches fail rather than falling back to a saved API key.

For local Docker development, the hostname must be reachable **from the Kit
container**. Set the same HTTP origin in the credential file and runtime
environment, and explicitly allow unencrypted development traffic:

```sh
export SPEAKEASY_ORIGIN='http://aicp:8080'
export SPEAKEASY_ALLOW_INSECURE_HTTP=1
```

Use HTTPS without this opt-in outside a trusted development environment. Kit
does not follow authenticated redirects; configure the final server origin.

## Models and inference

Select a model from the session's advertised Speakeasy list. The list comes from
your authenticated server, not the public OpenRouter catalog. If discovery fails,
Kit does not invent fallback models or add an unavailable current selection.

If the list is empty, check credential admission and the server's model-catalog
route with your control-plane operator. The server must return an eligible-model
list within Kit's limits of 2 MiB and 10,000 entries. The exact response contract
is in the [model discovery reference](../reference/speakeasy-assistant-inference.md#discover-eligible-models).

## Canonical chat association

Create each session with its intended control-plane chat. Reconnect or resume
that session to continue the same chat; do not replace its association with a
different chat ID. Cold resume can recover the saved association when the client
omits it. An older unbound session cannot acquire a binding during resume: create
a new, explicitly associated session instead.

Built-in Kit children share the parent's authorized inference association,
including child forks and restarts. External harnesses do not receive it. Direct
ACP forks require the caller to provide the intended association explicitly. See
[the adapter lifecycle rules](../reference/speakeasy-assistant-inference.md#preserve-chat-identity-through-session-lifecycle)
when implementing these operations.

The control plane must validate chat ownership and suppress inference transcript
capture. AICP's ACP projection is the sole transcript writer; enabling both
capture paths can duplicate conversation history.

## Resolve startup and inference failures

| Symptom | Action |
| --- | --- |
| `SPEAKEASY_ORIGIN is required for runtime credentials` | Set the origin on the Kit process, not only on the client connecting to it. |
| Credential origin does not match `SPEAKEASY_ORIGIN` | Correct the deployment environment or provision a credential for the intended server; do not redirect the request to another host. |
| Invalid runtime credential record or version | Re-provision the version-1 JSON file with only the documented fields. Fix file ownership and permissions if the store cannot read it. |
| Runtime inference requires canonical `ai.speakeasy/gram-chat-id` metadata | Fix the ACP client's session creation request using the adapter reference; do not substitute an ACP session ID. |
| `Gram chat binding cannot change on resume` | Resume with the saved association, or create a new session for the other chat. |
| The server rejects catalog or completion requests | Check expiry, revocation, runtime admission, chat ownership, and model eligibility with the control-plane operator. Kit does not obtain a replacement credential automatically. |
