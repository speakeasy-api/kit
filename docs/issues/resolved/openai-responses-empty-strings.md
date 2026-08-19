# OpenAI Responses empty strings caused protocol failures

Kit rejected empty string values in all Responses fields, including streamed text and reasoning deltas. The official Codex Responses parser accepts present empty strings, so a valid event could terminate a Kit model turn with:

```text
openai-subscription protocol error: Responses string field is missing or outside bounds
```

The parser now accepts empty text, delta, and summary strings while retaining type and size bounds. Function-call arguments still must contain valid JSON, and semantically non-empty function call IDs and names remain required. Validation errors now identify the field that failed, and regression tests cover empty output-text and reasoning-summary streams.
