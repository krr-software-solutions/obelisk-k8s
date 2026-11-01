# CLAUDE.md

Lean working notes for obelisk-k8s. Keep this file short; detail lives in `.claude/docs/`.

## Before you push

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Clippy runs pedantic; treat its warnings as errors.

## Docs

- [Layout](.claude/docs/layout.md): the workspace, the crates, and where logic goes.
