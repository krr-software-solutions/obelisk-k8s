# CLAUDE.md

Lean working notes for obelisk-k8s: Kubernetes deploy tooling for prod/dev and local k3d clusters,
in Rust. Keep this file short; detail lives in `.claude/docs/`.

## Before you push

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Clippy runs pedantic; treat its warnings as errors. CI doesn't run these yet (see Development).

## Docs

- [Overview](.claude/docs/overview.md): what obelisk-k8s is, current state, and the open design questions.
- [Layout](.claude/docs/layout.md): the workspace, the crates, and where logic goes.
- [Development](.claude/docs/development.md): commands, CI, MRs, and conventions.
