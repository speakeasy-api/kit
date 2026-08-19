# Third-Party Notices

Kit's native OpenAI subscription implementation in `src/provider/openai_auth.rs` and
`src/provider/chatgpt.rs` is derived from protocol behavior in the following
commit-addressed source files. Kit does not distribute or invoke either upstream program.

## OpenAI Codex

- Repository: https://github.com/openai/codex
- Commit: `79b4f03d35962b005b007a015113b38930711665`
- Files: `codex-rs/core/src/client.rs`, `codex-rs/login/src/server.rs`,
  `codex-rs/login/src/pkce.rs`, `codex-rs/login/src/auth/revoke.rs`,
  `codex-rs/login/src/token_data.rs`, and `codex-rs/protocol/src/models.rs`
- License: Apache License 2.0, reproduced in `third_party/licenses/CODEX-APACHE-2.0.txt`
- Copyright: Copyright 2025 OpenAI

## OpenCode

- Repository: https://github.com/anomalyco/opencode
- Commit: `fe82a1b6ca4f535beb973b0867017e3f639f85ed`
- File: `packages/opencode/src/plugin/openai/codex.ts`
- License: MIT, reproduced in `third_party/licenses/OPENCODE-MIT.txt`
- Copyright: Copyright (c) 2025 opencode
