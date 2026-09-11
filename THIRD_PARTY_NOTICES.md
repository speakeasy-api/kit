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
- Files: `packages/opencode/src/plugin/openai/codex.ts` and, at commit
  `1b937c860b6fd8a83e69f916b1236515aa17ea0d`,
  `packages/core/src/session/compaction.ts`
- License: MIT, reproduced in `third_party/licenses/OPENCODE-MIT.txt`
- Copyright: Copyright (c) 2025 opencode

## Clipboard image dependencies

The clipboard image feature uses the following 18 packages. Where an upstream
license expression offers multiple choices, Kit elects the MIT option. Exact
upstream license texts and notices are reproduced at the paths below.

| Packages | Version(s) | Selected license | License text / notice |
| --- | --- | --- | --- |
| `arboard` | 3.6.1 | MIT | `third_party/licenses/ARBOARD-MIT.txt` |
| `clipboard-win`, `error-code` | 5.4.1, 3.4.0 | Boost Software License 1.0 | `third_party/licenses/CLIPBOARD-BOOST-1.0.txt` |
| `crunchy` | 0.2.4 | MIT | `third_party/licenses/CRUNCHY-MIT.txt` |
| `dispatch2`, `objc2`, `objc2-app-kit`, `objc2-core-foundation`, `objc2-core-graphics`, `objc2-encode`, `objc2-foundation`, `objc2-io-surface` | 0.3.1, 0.6.4, 0.3.2, 0.3.2, 0.3.2, 4.1.0, 0.3.2, 0.3.2 | MIT | `third_party/licenses/OBJC2-LICENSE.md` |
| `fax` | 0.2.7 | MIT | `third_party/licenses/FAX-MIT.txt` |
| `gethostname` | 1.1.0 | Apache License 2.0 | `third_party/licenses/GETHOSTNAME-APACHE-2.0.txt` |
| `half` | 2.7.1 | MIT | `third_party/licenses/HALF-MIT.txt` |
| `tiff` | 0.11.3 | MIT | `third_party/licenses/TIFF-MIT.txt` |
| `x11rb`, `x11rb-protocol` | 0.13.2 | MIT | `third_party/licenses/X11RB-MIT.txt` |

Copyright notices present in the audited upstream license files are retained in
the reproduced files. The `objc2` notice also preserves the upstream statement
concerning bindings derived from Apple SDKs. None of these audited releases supplies a separate required
`NOTICE` file.

## Cargo dependency notice baseline

The clipboard dependency audit and table above are intentionally limited to the
18 packages newly selected for clipboard image support. They do not constitute a
license audit of Kit's pre-existing Cargo dependency graph. Release packaging now
distributes this file and `third_party/licenses`, but a comprehensive notice set
for the pre-existing dependency closure remains separate follow-up work.
