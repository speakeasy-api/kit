# OpenAI subscription fatal records hide the failure classification

An ACP-backed OpenAI subscription session ended with a fatal record that contained only:

```text
kind: provider
code: provider_error
message: openai-subscription failed (provider_error)
```

The corresponding transcript ends before the provider failure and intentionally does not persist terminal errors. The fatal record is therefore the only durable diagnostic, but `fatal::provider_code` did not recognize the safe classifications already produced by the OpenAI subscription provider. The original cause cannot be recovered from this record.

## Resolution

Fatal records now distinguish transient and permanent response failures, provider protocol failures, credential-worker failures, and retry-budget exhaustion. The record still excludes raw provider messages, response bodies, URLs, and provider-controlled error fields. Existing transport classifications and their boolean diagnostics are unchanged.
