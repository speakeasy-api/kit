# Kit compiles both AWS-LC and ring TLS providers

Kit's dependency graph currently compiles both `aws-lc-sys` and `ring`. AWS-LC is selected by Reqwest's `rustls` feature and `jsonwebtoken`'s `aws_lc_rs` backend. The A2A client independently selects ring through its default `tls-rustls` feature. This duplicates cryptographic providers and makes AWS-LC's native build the largest unit in clean checks.

The preferred optimization is a coordinated ring-only configuration:

- change Kit and AgentKit Reqwest dependencies from `rustls` to `rustls-no-provider`;
- change `agentkit-mcp` from RMCP's `reqwest` feature to `reqwest-tls-no-provider`;
- change Kit's `jsonwebtoken` backend from `aws_lc_rs` to `rust_crypto`; and
- explicitly install `rustls::crypto::ring::default_provider()` before any Reqwest client is constructed.

An isolated full-graph experiment removed AWS-LC and retained A2A HTTPS. A clean `cargo check --locked --timings` improved from 52.98 seconds to 43.83 seconds on `aarch64-apple-darwin` with Rust 1.94.0, approximately 17%. User CPU fell from 180.90 to 156.60 seconds and system CPU from 42.35 to 26.58 seconds. The package count increased from 468 to 477 because `jsonwebtoken`'s `rust_crypto` feature enables RSA, ECDSA, and Ed25519 dependencies together, but these pure-Rust crates were still cheaper to build than AWS-LC and its native toolchain.

A feature-only prototype is not safe to ship. It compiled successfully, but the full Kit test suite failed when Reqwest constructed clients before a process-level provider had been installed:

```text
No rustls crypto provider is configured. When using the `rustls-no-provider`
feature you must install a crypto provider before building a Client.
```

Provider installation must cover Kit's direct blocking and asynchronous Reqwest clients as well as AgentKit and RMCP construction paths. AgentKit is also usable as a library, so initialization ownership and idempotency must be explicit rather than relying accidentally on A2A's Cargo features.

## Alternatives considered

- Disabling the A2A client's default features removes ring but also disables A2A HTTPS.
- An AWS-LC-only graph requires A2A to support an AWS-LC or provider-neutral TLS feature and retains the dominant native build cost.
- Native TLS introduces platform-specific OpenSSL and system-framework behavior.
- Replacing `jsonwebtoken` with a narrow RS256 implementation could reduce the RustCrypto graph, but a security-sensitive JWT rewrite is not justified solely as a build optimization without separate design and security review.

## Acceptance criteria

- Only one Rustls crypto provider is present in `cargo tree -e features`.
- A2A, MCP, provider HTTP, OAuth token exchange/revocation, and OpenAI RS256 JWT verification continue to work.
- Provider installation is deterministic, idempotent, and tested before every client-construction path that can run independently.
- `cargo check --locked --all-targets`, Clippy with warnings denied, and the complete locked test suite pass.
- Clean build timings are measured on macOS ARM64 and Linux x64 before landing.
- Affected AgentKit crates are released before Kit updates its exact pins.
