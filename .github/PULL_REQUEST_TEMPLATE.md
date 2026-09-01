## Summary

<!-- Describe the user-visible change and the invariant it preserves. -->

## Validation

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo test --workspace --all-targets --all-features --locked --no-fail-fast`
- [ ] `cargo +1.88.0 check --workspace --all-targets --all-features --locked`
- [ ] Relevant protocol, OpenCode, and benchmark smokes
- [ ] Real adapter and `cargo deny` checks when applicable

## Safety

- [ ] Source-write-free and path-authorization boundaries remain intact.
- [ ] New process, network, cache, or workspace-code effects are documented.
- [ ] Structured/text statuses agree and new outputs are bounded.
- [ ] English and Turkish public docs remain synchronized.
- [ ] No secrets, private source, prompts, raw paths, session IDs, or logs are included.

## Residual Risk

<!-- List assumptions, platform gaps, and checks not run. -->
