# Overview

obelisk-k8s deploys apps to Kubernetes: real prod/dev clusters, and local
[k3d](https://k3d.io) clusters with ephemeral instances. It replaces obelisk-local (the Scala deploy
CLI, `~/dev/repo/current/obelisk-local`), which is frozen and keeps running mission-control prod
until cutover. IP allocation and DNS for local clusters come from
[obeliskd](https://gitlab.krr-software-solutions.com/oss/obeliskd), never from obelisk-local's
ip-allocator.

## Current state

- `obelisk-k8s config repo show`, run from a repo root: loads and checks `.obelisk-k8s/` (`repo.yaml`,
  `targets/<dir>/target.yaml`, `units/<dir>/unit.yaml`), then prints the targets and units. Names must be
  DNS labels; stray entries in `targets/` and `units/` are skipped with a `WARN`.
- `obelisk-k8s config repo paths [-t <target>]... [-u <unit>]... [--explain]`: the config files each
  selected unit gets on each selected target (default: all of them), in merge order. One case prints as
  is; several print one after another, each under a magenta `[ target=…, unit=… ]` header, skipping the
  pairs where the unit doesn't deploy to the target. Config paths can use `${{obelisk:target}}` and `${{obelisk:unit}}`; they
  merge by scope (repo < target < unit < target + unit), then by declaring file (repo < target < unit),
  then in the order written. Missing files are skipped; `--explain` prints a table of every path (as written, as
  evaluated, found or skipped), colored on a terminal.
- `obelisk-k8s config global show`: loads and checks the machine-wide config (`~/.obelisk-k8s/`, or
  `--global-root` / `OBELISK_K8S_GLOBAL_ROOT`): `repos.yaml` (repo paths: relative to the global root, `~/`, or
  absolute; a trailing `/*` for every repo in a directory; names come from each repo's `repo.yaml`) and
  `clusters/<dir>/cluster.yaml`. Prints the repos and clusters. A repo that won't load, or a target
  naming a cluster that isn't there, is a `WARN`.
- `obelisk-k8s config global init`: creates the global root, an empty `repos.yaml` and `clusters/`
  (whichever are missing), then loads it.
- `obelisk-k8s config global cluster add <NAME> --k8s … --location … [--managed] […]` writes
  `clusters/<NAME>/cluster.yaml`; `cluster remove <NAME>` deletes its directory, only if that holds
  nothing but its `cluster.yaml`. Both check the result by loading the global config again.
- `obelisk-k8s config global repo add <PATH>` appends an entry to `repos.yaml` as written (creating
  the file if it's missing); an entry for a place already listed, however it's spelled, is a no-op.
  `repo remove <NAME|PATH>` removes the entry for that place, or the one listing that repo by path.
  A block entry takes the comment lines directly above it (same indent, no blank line between) with
  it. Both edit the file in place with yaml-edit, keeping its comments and formatting, and put it
  back if the result won't load.
- `config repo …` works on the repo in the current directory, or with `--repo <name>`, on the one with
  that name in the global `repos.yaml` (paths are then shown under it, so they work from anywhere).
- `--help` groups options under Core, Extra and Special headings (as obeliskd does).
- `obelisk-k8s completions <bash|zsh|fish|elvish|powershell>`: prints a shell completion script.

## The design

A deployment goes through a few layers:

```text
instance spec → generator → values.yaml → plain Helm chart → k8s resources
```

- **Generator:** any executable that takes inputs and writes a values file. Python is the default.
  Logic lives here, not in Helm Go templates.
- **Helm:** templates, deep-merges the values files (`-f` … `-f`), and applies
  (`helm upgrade --install`, still tentative). Raw YAML and YAML from a URL are wrapped into throwaway
  charts, so every unit is a Helm release.
- **obelisk-k8s:** orders units (CRDs before what uses them, across namespaces), waits for
  readiness, and resolves `ref+awsssm` refs before values reach Helm.

## Roadmap

| Phase | What |
|---|---|
| v1   | a render-only `template` command for prod/dev targets, diffed against obelisk-local on mission-control. Home lab is a layout check (raw YAML, URL YAML, Helm charts) |
| v1.1 | ephemeral instances on local k3d clusters, the first real use |
| v1.2 | up/down hooks: setup, database/topic creation, seeding test data |
| v1.3 | a whole-cluster spec across repos/apps |

Design the layout format on paper against v1.3, but build only the current phase.

## Rules

- **Never print resolved secret values:** not in logs, errors or CI traces.
- **No Obelisk-level Secret vs ConfigMap split.** Each app decides what goes where.
- **Global vs per-instance** deployments are a pattern users build, not a tool feature.
- **Config comes from files, not env vars.** Prefer a YAML config file over threading env vars
  through a generator.

## Open design questions

Nothing below is decided. Don't silently pick an answer in code; raise it.

- The generator contract: inputs, output location and format.
- Do our own apps need Helm charts, or can a generator emit plain YAML?
- Do raw/URL YAML units still get `ref+awsssm` resolution once wrapped (obelisk-local's `doTemplate`)?
- CRD upgrades, given Helm never upgrades `crds/`.
- Is apply part of v1, or only `template`?
- Does the tool own instance identity for clean up/down (v1.1)? Hook shape (v1.2)?

These are tracked in the Aletheia knowledge graph, project OBLSK (start at OBLSK/I-102).
