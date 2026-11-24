## Summary

Describe what changed and why.

## Verification

- [ ] `RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --locked`
- [ ] `cargo test --workspace --all-targets --locked`
- [ ] `cargo doc --workspace --no-deps --locked`
- [ ] Tested hardware/display paths cleanly without speculative startup writes
