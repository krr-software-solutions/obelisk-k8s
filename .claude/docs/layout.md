# Layout

A Cargo workspace; every crate lives under `code/`.

| Path                   | Crate             | Kind                      | Role                                    |
|------------------------|-------------------|---------------------------|-----------------------------------------|
| `code/obelisk-k8s`     | `obelisk-k8s`     | lib                       | everything: model, rendering, logic     |
| `code/obelisk-k8s-cli` | `obelisk-k8s-cli` | bin (named `obelisk-k8s`) | the CLI: argument parsing, wiring, error reporting only |

Keep the binary thin. Logic goes in the `obelisk-k8s` library, where it can be tested. `main.rs`
defines the CLI (the clap types, `--help` headings, completions) and dispatches each command to
`commands.rs`, which calls into the library and prints the result. Failures come back as
`ObeliskK8sError` and `main` reports them (`ERROR - …`, exit code 1).

## The library (`code/obelisk-k8s/src/`)

- `error.rs`: `ObeliskK8sError`, every error the library can fail with.
- `yaml.rs`: all YAML, including `Spanned` (a value plus its line and column), and in-place list
  edits that keep comments and formatting. Nothing else names the YAML crates (serde-saphyr to read
  and write, yaml-edit to edit), so they can be swapped here.
- `interp.rs`: `${{obelisk:...}}` interpolation (`InterpString`: the raw string plus its parts). Only for
  obelisk-k8s's own config files, never values files.
- `config/mod.rs`: what the config files share: scanning `<kind>/<dir>/<file>`, names (DNS labels,
  defaulting to the directory), duplicate checks.
- `config/global.rs`: `GlobalConfig`, the machine-wide `~/.obelisk-k8s/` (repos, clusters). Behind
  `config global show`.
- `config/global/edit.rs`: changing the global config (`init`, `cluster add` / `remove`, `repo add` /
  `remove`). A child of `global.rs`, so it uses its private helpers.
- `config/repo.rs`: `RepoConfig`, a repo's `.obelisk-k8s/` (repo, targets, units), loaded and checked.
  Behind `config repo show`.
- `config/values.rs`: the merged values for a unit on a target (`template values`), as a
  `serde_json::Value` tree (`preserve_order`); YAML in and out still goes through `yaml.rs`.
- `config/generate.rs`: the generated values (`template generated-values`): generators rendered and
  merged over the config values.
- `config/manifests.rs`: the rendered resources (`template manifests`): templates rendered with the
  generated values.
- `config/ephemeral.rs`: an ephemeral target's copy (`--ephemeral-name` and its values), and checking
  a selection needs one exactly when it has an ephemeral target.
- `git.rs`: the `obelisk.git` facts (sha, tag, dirt), read with `git`.
- `helm.rs`: running `helm template` on a throwaway chart (for Go templates).
- `config/paths.rs`: which config files a unit gets on a target, and their merge order. Behind
  `config repo paths`.

## Adding a crate

Put it under `code/<name>`, add it to `members` in the root `Cargo.toml`, take shared metadata
with `*.workspace = true`, add `[lints] workspace = true`, and declare dependencies in
`[workspace.dependencies]` first.
