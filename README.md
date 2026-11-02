# obelisk-k8s

Deploy tooling for Kubernetes, for real prod/dev clusters and local [k3d](https://k3d.io) ones alike.

> **Status:** early. `template`, `apply` and local k3d `cluster up` work; see `.claude/docs/overview.md`.

## Installing

Linux and macOS. The project is private, so it takes a GitLab token that can read it, exported (the
`sh` side downloads with it too):

```sh
export GITLAB_TOKEN=glpat-…   # a token that can read the project (read_api); install.sh needs it too
curl -fsSL -H "PRIVATE-TOKEN: $GITLAB_TOKEN" "https://gitlab.krr-software-solutions.com/api/v4/projects/24/repository/files/install.sh/raw?ref=main" | sh
```

It installs the latest release's binary to `~/.local/bin` (see `install.sh` for the options). It runs
`helm`, `kubectl`, and for local clusters `k3d` and `docker`, which it doesn't install.

## The idea

A deployment goes through a few layers:

```text
config files → values → generators → generated values → templates → k8s resources → Helm release
```

- **Config files:** a repo's values per target and unit, deep-merged in a fixed order.
- **Generators:** Go templates that compute more values from those (rendered by Helm); executables
  are planned. This keeps logic out of the templates.
- **Helm:** does the templating, merges the values files, and applies (`helm upgrade --install`).
  Raw YAML and YAML from a URL get wrapped into throwaway charts, so every unit is a Helm release.
- **obelisk-k8s:** orders the units (CRDs before the resources that use them, across namespaces),
  waits for readiness, and resolves SSM refs before values reach Helm. It never prints resolved secrets.

IP allocation and DNS for local clusters come from
[obeliskd](https://gitlab.krr-software-solutions.com/oss/obeliskd).

## Roadmap

- **Done:** `template` (values, generated values, manifests), `apply`, ephemeral targets, and local
  k3d clusters on obeliskd (`cluster up/down/destroy/status`, with MetalLB and a gateway).
- **Next:** SSM refs, upstream charts and raw/URL YAML as units, `diff`, then up/down hooks (setup,
  database/topic creation, seeding test data) and blueprints: a spec that pulls apps from several
  repos onto a managed cluster.

## Relationship to obelisk-local

obelisk-k8s replaces obelisk-local, the Scala deploy CLI. The two run side by side until prod is
cut over, and then obelisk-local is retired.
