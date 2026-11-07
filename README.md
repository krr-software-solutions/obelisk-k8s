# obelisk-k8s

Deploy tooling for Kubernetes, for real prod/dev clusters and local [k3d](https://k3d.io) ones alike.

> **Status:** early. `template`, `apply` and local k3d `cluster up` work; see `.claude/docs/overview.md`.

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
`helm`, `kubectl`, and for local clusters `k3d` and `docker`, which it doesn't install.

## The idea

A deployment goes through a few layers:

```text
config files → values → generators → generated values → templates → k8s resources → Helm release
```

- **Config files:** a repo's values per target and unit, deep-merged in a fixed order.
- **Generators:** compute more values from those: Go templates (rendered by Helm), files, URLs, or
  any command (values in on stdin, values out). This keeps logic out of the templates.
- **Templates:** render the resources: Go templates, raw YAML files or URLs, upstream Helm charts,
  or commands. Every unit is installed as one Helm release (`helm upgrade --install`).
- **obelisk-k8s:** orders the units (CRDs before the resources that use them, across namespaces),
  waits for readiness, and resolves SSM refs before values reach Helm. It never prints resolved secrets
  (unless asked to, with `template --resolve --reveal`).

IP allocation and DNS for local clusters come from
[obeliskd](https://github.com/krr-software-solutions/obeliskd).

## Roadmap

- **Done:** `template` (values, generated values, manifests), `apply`, ephemeral targets, and local
  k3d clusters on obeliskd (`cluster up/down/destroy/status`, with MetalLB and a gateway).
- **Also done:** generators and templates of every type: Go templates, raw files, URLs, upstream
  Helm charts, and commands.
- **Also done:** adopting existing resources (`apply --take-ownership`), and SSM refs
  (`${{obelisk:aws-ssm:<key>}}`, credentials from `AWS_SSM_ACCESS_KEY` / `AWS_SSM_SECRET_KEY` /
  `AWS_SSM_REGION`, or a profile named by `AWS_SSM_PROFILE`).
- **Next:** `diff`, then up/down hooks (setup,
  database/topic creation, seeding test data) and blueprints: a spec that pulls apps from several
  repos onto a managed cluster.

## Relationship to obelisk-local

obelisk-k8s replaces obelisk-local, the Scala deploy CLI. The two run side by side until prod is
cut over, and then obelisk-local is retired.
