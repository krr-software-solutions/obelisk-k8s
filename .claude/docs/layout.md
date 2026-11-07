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
- `interp.rs`: `${{obelisk:...}}` interpolation (`InterpString`: the raw string plus its parts):
  `target`, `unit`, and `path:unit` (build paths only). Only for obelisk-k8s's own config files; values
  take only `aws-ssm` refs (`ssm.rs`).
- `ssm.rs`: `${{obelisk:aws-ssm:<key>}}` refs: checking, resolving (the AWS SDK, with credentials
  from the `AWS_SSM_*` variables, or `AWS_SSM_PROFILE`'s profile), and masking what was resolved (`Ssm::redact`).
- `config/mod.rs`: what the config files share: scanning `<kind>/<dir>/<file>`, names (DNS labels: a
  target's or unit's is its directory's; a cluster's defaults to it), duplicate checks.
- `config/global.rs`: `GlobalConfig`, the machine-wide `~/.obelisk-k8s/` (repos, clusters). Behind
  `config global show`.
- `config/global/edit.rs`: changing the global config (`init`, `cluster add` / `remove`, `repo add` /
  `remove`). A child of `global.rs`, so it uses its private helpers.
- `config/repo.rs`: `RepoConfig`, a repo's `.obelisk-k8s/` (repo, targets, units, build paths), loaded
  and checked. Behind `config repo show`.
- `config/build.rs`: a case's build plan (`template build`): its build file rendered, and its output
  checked into phases of values and manifests sources.
- `config/phases.rs`: running a plan's values phases, and agreeing on the release (`template values`).
- `config/values.rs`: the `obelisk:` block, merging values (as a `serde_json::Value` tree,
  `preserve_order`; YAML in and out still goes through `yaml.rs`), and what values sources may set
  under `obelisk:`.
- `config/render.rs`: rendering one source, by type (`inline`, `gotmpl`, `file`, `url`, `helm`,
  `command`).
- `config/manifests.rs`: the rendered resources (`template manifests`): each phase's manifests
  sources, rendered with the values before it.
- `config/apply.rs`: `apply`: rendered manifests installed as Helm releases on each target's cluster.
- `config/ephemeral.rs`: an ephemeral target's copy (`--ephemeral-name` and its values), and checking
  a selection needs one exactly when it has an ephemeral target.
- `config/run.rs`: `RunInputs`, what every case in a run shares: the git facts, the ephemeral copy,
  the global config its targets' clusters come from (each must be in it), and `obelisk.paths`.
- `cluster.rs`: `cluster status` / `up` / `down` / `destroy` for managed clusters (local k3d only).
- `base.rs`: a managed cluster's base (obeliskd gateway ip and names, MetalLB, Envoy Gateway).
- `k3d.rs`: running `k3d` (and `docker`, to tell obelisk-k8s's clusters from others).
- `obeliskd.rs`: asking `obeliskd` for the docker subnet a cluster runs on, and claiming /
  releasing the base's ips and hostnames.
- `git.rs`: the `obelisk.git` facts (sha, tag, dirt), read with `git`.
- `helm.rs`: running Helm: `helm template` on a throwaway chart (for Go templates), and
  `helm upgrade --install` (of charts, and of rendered manifests through a throwaway chart).
- `config/paths.rs`: which (target, unit) cases a command covers, and the build file each renders.
  Behind `config repo paths`.

## Adding a crate

Put it under `code/<name>`, add it to `members` in the root `Cargo.toml`, take shared metadata
with `*.workspace = true`, add `[lints] workspace = true`, and declare dependencies in
`[workspace.dependencies]` first.
