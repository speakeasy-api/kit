# Conformance stress tests

Lock- and filesystem-heavy matrices are ignored by the default conformance target. Run them only
as exact tests, serially, on an otherwise idle worker:

```sh
cargo test --locked --test conformance edit_format::final_mutation_is_rejected_500_iterations_parallel -- --ignored --exact --test-threads=1
cargo test --locked --test conformance ws_revision::full_manager_restart_rotates_identity_500_iterations_parallel -- --ignored --exact --test-threads=1
cargo test --locked --test conformance store_append::sixty_four_real_connections_allocate_one_gapless_committed_prefix -- --ignored --exact --test-threads=1
```

Retain the exact command, elapsed time, pass count, and any SQLite `Busy` or workspace-owner
`Unavailable` result as stress evidence. Do not run these commands concurrently with default CI
lanes or dogfood acceptance.
