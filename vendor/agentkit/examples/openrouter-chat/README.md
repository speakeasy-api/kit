# `openrouter-chat`

Minimal example and test bed for `agentkit-provider-openrouter`.

## Run

```bash
cat > .env <<'EOF'
OPENROUTER_API_KEY=your_api_key_here
OPENROUTER_MODEL=openrouter/hunter-alpha
EOF

cargo run -p openrouter-chat -- "write a short Rust haiku"
```

Interactive mode:

```bash
cargo run -p openrouter-chat
```

The example loads environment variables from the workspace `.env` file automatically.

Useful optional environment variables:

- `OPENROUTER_MODEL`
- `OPENROUTER_APP_NAME`
- `OPENROUTER_SITE_URL`
- `OPENROUTER_MAX_COMPLETION_TOKENS`
- `OPENROUTER_TEMPERATURE`
