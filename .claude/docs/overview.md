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
