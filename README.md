# obelisk-k8s

Deploy tooling for Kubernetes, for real prod/dev clusters and local [k3d](https://k3d.io) ones alike.

> **Status:** early. `template`, `apply`, `diff` and local k3d `cluster up` work; see `.claude/docs/overview.md`.

## Installing

Linux (x86_64, aarch64) and macOS:

```sh
curl -fsSL https://github.com/krr-software-solutions/obelisk-k8s/releases/latest/download/install.sh | sh
```

It downloads and verifies the latest release's binary, puts it in `~/.local/bin`, and installs shell
completions for each of bash, zsh and fish you have (zsh needs `~/.local/share/zsh/site-functions`
on its `fpath`; the installer says so). Set
`OBELISK_K8S_VERSION` for another version, `OBELISK_K8S_INSTALL_DIR` for another place. Or take an
archive from the [releases](https://github.com/krr-software-solutions/obelisk-k8s/releases). It runs
`helm`, `kubectl`, and for local clusters `k3d` and `docker`, which it doesn't install. `diff` also
needs Helm's [helm-diff](https://github.com/databus23/helm-diff) plugin:
`helm plugin install https://github.com/databus23/helm-diff`.

## The idea

A deployment goes through a few layers:

```text
build file → build plan → phases (values, then manifests) → k8s resources → Helm release
```

- **Build file:** each unit's (or the repo's) Go template, rendered by Helm per target into a
  **build plan**: an ordered list of phases. The logic of what a unit is made of lives here.
- **Phases:** each lists values sources and manifests sources, and renders them against the values
  the phases before it made. Values sources (inline YAML, Go templates, files, URLs, or any command:
  values in on stdin, values out) deep-merge, in order, into the values the next phases see.
- **Manifests:** the resources: Go templates, raw YAML files or URLs, upstream Helm charts, or
  commands. Every unit is installed as one Helm release (`helm upgrade --install`).
- **obelisk-k8s:** orders the units (CRDs before the resources that use them, across namespaces),
  waits for readiness, and resolves SSM refs before values reach Helm. It never prints resolved secrets
  (unless asked to, with `template --resolve --reveal` or `diff --reveal`).

IP allocation and DNS for local clusters come from
[obeliskd](https://github.com/krr-software-solutions/obeliskd).

## Roadmap

- **Done:** `template` (build plan, values, manifests), `apply`, `diff` (what `apply` would
  change, via helm-diff), ephemeral targets, and local
  k3d clusters on obeliskd (`cluster up/down/destroy/status`, with MetalLB and a gateway).
- **Also done:** build files and phases, with values and manifests sources of every type: inline
  values, Go templates, raw files, URLs, upstream Helm charts, and commands.
- **Also done:** adopting existing resources (`apply --take-ownership`), and SSM refs
  (`${{obelisk:aws-ssm:<key>}}`, credentials from `AWS_SSM_ACCESS_KEY` / `AWS_SSM_SECRET_KEY` /
  `AWS_SSM_REGION`, or a profile named by `AWS_SSM_PROFILE`).
- **Also done:** image specs in a phase (`template images`): parsed and checked, not built yet.
- **Next:** images built in a phase, then up/down hooks (setup,
  database/topic creation, seeding test data) and blueprints: a spec that pulls apps from several
  repos onto a managed cluster.

## Relationship to obelisk-local

obelisk-k8s replaces obelisk-local, the Scala deploy CLI. The two run side by side until prod is
cut over, and then obelisk-local is retired.
