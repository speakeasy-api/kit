# OpenAI Responses additive events terminated model turns

Kit treated every unknown Responses SSE event as a protocol error. The official Codex parser ignores unhandled events, including new delta families, so additive backend events could terminate otherwise valid Kit turns.

Kit now ignores unknown event kinds while continuing to validate and process known data-bearing and terminal events. Regression coverage includes both an unknown ordinary event and an unknown delta event.

Kit also now captures `x-codex-turn-state` from successful HTTP response headers or `response.metadata` events and replays it when a safely retryable turn request is sent again. Effective model metadata in `openai-model` or `x-openai-model` SSE headers is recorded using the same validation as existing model observations.
