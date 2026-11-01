# Layout

A Cargo workspace; every crate lives under `code/`.

| Path                   | Crate             | Kind                      | Role                                    |
|------------------------|-------------------|---------------------------|-----------------------------------------|
| `code/obelisk-k8s`     | `obelisk-k8s`     | lib                       | everything: model, rendering, logic     |
| `code/obelisk-k8s-cli` | `obelisk-k8s-cli` | bin (named `obelisk-k8s`) | the CLI: argument parsing, wiring, error reporting only |

Keep the binary thin. Logic goes in the `obelisk-k8s` library, where it can be tested. A command
in `main.rs` parses its arguments, calls into the library, and prints the result; failures come
back as `ObeliskK8sError` and `main` reports them (`ERROR - …`, exit code 1).

## The library (`code/obelisk-k8s/src/`)

- `error.rs`: `ObeliskK8sError`, every error the library can fail with.
- `yaml.rs`: all YAML, including `Spanned` (a value plus its line and column). Nothing else names the
  YAML crate (serde-saphyr), so it can be swapped here.
- `config/repo.rs`: `RepoConfig`, a repo's `.obelisk-k8s/` (repo, targets, units), loaded and checked.
  Behind `config repo show`.

## Adding a crate

Put it under `code/<name>`, add it to `members` in the root `Cargo.toml`, take shared metadata
with `*.workspace = true`, add `[lints] workspace = true`, and declare dependencies in
`[workspace.dependencies]` first.
