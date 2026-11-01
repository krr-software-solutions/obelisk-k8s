# obelisk-k8s

Deploy tooling for Kubernetes, for real prod/dev clusters and local [k3d](https://k3d.io) ones alike.

> **Status:** early. Nothing here works yet.

## The idea

A deployment goes through a few layers:

```text
instance spec → generator → values.yaml → plain Helm chart → k8s resources
```

- **Instance spec:** what you want, e.g. "ephemeral instance `xyz`, database in the global namespace".
- **Generator:** any executable that takes inputs and writes a values file. Python is the default.
  This keeps logic out of heavy Helm Go templates.
- **Helm:** does the templating, merges the values files, and applies (`helm upgrade --install`).
  Raw YAML and YAML from a URL get wrapped into throwaway charts, so every unit is a Helm release.
- **obelisk-k8s:** orders the units (CRDs before the resources that use them, across namespaces),
  waits for readiness, and resolves SSM refs before values reach Helm. It never prints resolved secrets.

IP allocation and DNS for local clusters come from
[obeliskd](https://gitlab.krr-software-solutions.com/oss/obeliskd).

## Roadmap

- **v1:** a render-only `template` command for prod/dev targets.
- **v1.1:** ephemeral instances on local k3d clusters.
- **v1.2:** up/down hooks (setup, database/topic creation, seeding test data).
- **v1.3:** a cluster spec that pulls apps from several repos ("give me a whole cluster that looks like this").

## Relationship to obelisk-local

obelisk-k8s replaces obelisk-local, the Scala deploy CLI. The two run side by side until prod is
cut over, and then obelisk-local is retired.
