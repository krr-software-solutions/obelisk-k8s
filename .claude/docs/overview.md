# Overview

obelisk-k8s deploys apps to Kubernetes: real prod/dev clusters, and local
[k3d](https://k3d.io) clusters with ephemeral instances. It replaces obelisk-local (the Scala deploy
CLI, `~/dev/repo/current/obelisk-local`), which is frozen and keeps running mission-control prod
until cutover. IP allocation and DNS for local clusters come from
[obeliskd](https://github.com/krr-software-solutions/obeliskd), never from obelisk-local's
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
- `obelisk-k8s cluster up|down|destroy <NAME>`: create (or start, if stopped) / stop (keeping its
  state) / delete a managed local k3d cluster from the global config. `up` adds its kubectl context
  `k3d-<NAME>` to the default kubeconfig without switching to it. Each is a no-op when there's
  nothing to do, and each refuses a k3d cluster of the same name that obelisk-k8s didn't create (it marks its own with the
  Docker label `obelisk-k8s.managed=true` on `server-0`). Created with k3d's load balancer container
  and k3s's traefik and servicelb left out: MetalLB is meant to hand out LoadBalancer addresses.
  With `obeliskd: { supernet, subnet }`, it runs on that obeliskd docker subnet's network
  (`obd-<supernet>-<subnet>`): the subnet must be in obeliskd's listing (the error says how to
  allocate it) and its network in Docker; obeliskd keeps both after `destroy`. A `kubectlContext`
  other than `k3d-<NAME>`, or an existing cluster on a different network than its config says, is a
  `WARN`. Names longer than 50 characters are refused (k3d's container hostnames must fit). k3d's
  errors come through as their `ERRO` / `FATA` lines, without colors.
- `cluster up` on an obeliskd cluster also installs (or repairs) its **base**, so an app is
  reachable by name with just an `HTTPRoute`: an obeliskd ip `<cluster>-gateway` (tagged with the
  cluster) with hostnames `<cluster>.obd.internal` and `*.` / `*.*.` / `*.*.*.<cluster>.obd.internal`;
  MetalLB (pool: just that ip, `autoAssign: false`); Envoy Gateway; and GatewayClass `envoy` +
  Gateway `private-default-gateway` in `envoy-gateway-system` on that ip (the home lab's names, so
  routes carry over). Chart versions are pinned in `base.rs`. `destroy` releases the obeliskd ips
  tagged with the cluster in its subnet, and their hostnames (renaming the cluster, or dropping its
  `obeliskd`, before `destroy` leaves them behind). A cluster without `obeliskd` gets no base (a
  `WARN`). `up` needs `helm` and `kubectl`, and the network (Helm fetches the charts each run). A
  release that doesn't come up is rolled back, one stuck mid-operation is reported with how to clear
  it, and each waits up to 5m. Helm doesn't update CRDs: after bumping a pin, apply the new CRDs by
  hand or `destroy` and `up`.
- `obelisk-k8s cluster status [NAME]`: a table of the global config's clusters (or one): kind,
  managed, state (`running` / `stopped` / `missing` / `not ours`, for managed local k3d clusters;
  `-` otherwise), kubectl context and Docker network, as k3d reports them for a local k3d cluster
  that exists (`-` when it doesn't), else the configured `kubectlContext`. Only asks k3d when there's
  a local k3d cluster to look at.
- `obelisk-k8s config global cluster add <NAME> --k8s … --location … [--managed] […]` writes
  `clusters/<NAME>/cluster.yaml`; `cluster remove <NAME>` deletes its directory, only if that holds
  nothing but its `cluster.yaml`. Both check the result by loading the global config again.
- `obelisk-k8s config global repo add <PATH>` appends an entry to `repos.yaml` as written (creating
  the file if it's missing); an entry for a place already listed, however it's spelled, is a no-op.
  `repo remove <NAME|PATH>` removes the entry for that place, or the one listing that repo by path.
  A block entry takes the comment lines directly above it (same indent, no blank line between) with
  it. Both edit the file in place with yaml-edit, keeping its comments and formatting, and put it
  back if the result won't load.
- `obelisk-k8s template values [-t <target>]... [-u <unit>]...`: the merged values each selected unit
  gets on each selected target: its found config files deep-merged in merge order (maps key by key;
  lists, scalars and `null` replace), with the tool's `obelisk: { repo, target, unit, cluster, ephemeral, git }` block on top.
  `obelisk:` is reserved: a config file may only set `obelisk.release.{name, namespace}` and
  `obelisk.version`, as strings (kept per file and left out of this output; they're agreed on after the
  generators: see **Release** and `obelisk.version` below), and anything else under it is an
  error. SSM refs aren't resolved yet.
  Quoted strings stay as written; plain scalars come out parsed, the YAML 1.1 way where it matters
  (`0755` is `493`), with known limits listed in `config/values.rs`. Same selection as
  `config repo paths`; several cases print as a YAML stream, each under a `# [ target=…, unit=… ]`
  comment. Takes `--repo` too.
- `obelisk.cluster: { name, supernet, subnet }` is the target's cluster, from the global config:
  its name, and the obeliskd supernet and subnet it runs on (`null` without `obeliskd`). So every
  `template` command (and `apply`) loads the global config, and a selected target whose cluster
  isn't in it is an error.
- The `obelisk:` block also carries what git says about the repo: `obelisk.git: { sha: { short, value
  }, tag: { exact, value }, dirty: { tracked, untracked, any } }` (`null` outside a git repo; `tag.*`
  null without tags; `dirty` null if `git status` can't say). It's whichever repo the repo root is in,
  so a repo root that's a subdirectory gets the enclosing repo's HEAD and dirt. Any tag counts: with
  several on HEAD, an annotated one wins, then the first by name.
- **Ephemeral targets:** a target with `ephemeral: true` (missing or `null`: false) is a stack that
  runs many times side by side. Every `template` command on one needs `--ephemeral-name <NAME>` (a DNS
  label) and exactly one of `--ephemeral-values <YAML>` / `--ephemeral-values-path <PATH>`; that's an
  error to leave out even when the target is only selected by default (no `-t`), and giving them when
  none of the targets being rendered is ephemeral is an error too (a target counts if a selected
  unit renders on it). `obelisk.ephemeral` is the name (`null` on other
  targets), and the values are the last config layer, merged over every config file under the same
  rules (so they can set `obelisk.version` too, and it shows up as `--ephemeral-values` or the path).
  diff/apply will take the same flags.
- `obelisk.version`: after the generators, exactly one distinct version is required for each unit on
  each target, from any of: `version` in its target or unit (interpolated), or `obelisk.version` in a
  config file or generator. It's added to the generated values, and templates' charts get it as
  `version` and `appVersion`, so it must be `SemVer`-ish for `template manifests` (e.g. `0.0.0-<sha>`,
  or a tag like `v1.2.3`). Note that `git describe` output like `v1.2.3-4-gabc1234` is SemVer for a
  *prerelease of* 1.2.3, so it sorts before `v1.2.3`.
- `obelisk-k8s template manifests [-t …]… [-u …]…`: the k8s resources each unit renders: its
  `templates:` (listed like `generators:`, `{ type: gotmpl, path }`), each rendered on its own through a
  throwaway Helm chart with the generated values as `.Values`, joined in merge order as Helm prints
  them (each document under a `# Source:` comment naming the template; Helm orders the documents
  *within* one template by kind). A template may print any number of documents. No shared `define`s
  across files for now (tentative). `.Release.{Name, Namespace}` is the case's agreed release (see
  below). Takes `--repo` too.
- `obelisk-k8s apply [-t …]… [-u …]… [--ephemeral-*] [--yes]`: installs what `template manifests`
  renders, one Helm release per unit on a target: its agreed release name, into its agreed namespace
  (created if missing), at its agreed version, via a throwaway chart that ships the rendered
  manifests as-is (`{{ .Files.Get }}`, no second templating), with `helm upgrade --install
  --rollback-on-failure` on the target's cluster's context (`k3d-<name>` for a local k3d cluster,
  else its `kubectlContext`; none is an error). Everything is planned before anything installs:
  clusters resolved, every case rendered, and two cases that would be the same release on the same
  context (the second would replace the first), or a case whose templates render nothing (an upgrade
  would empty the release), are errors. A remote cluster needs `--yes`. A unit with no templates is
  skipped. Each install waits up to 5m and rolls back if it doesn't come up; if one fails, the ones
  before it stay applied. Takes `--repo` too; the clusters come from the global config.
- `obelisk-k8s template generated-values [-t …]… [-u …]…`: the merged values with each of the unit's
  generators deep-merged on top, in merge order. `generators:` (in `repo.yaml`, `target.yaml`,
  `unit.yaml`) lists `{ type: gotmpl, path }` like config paths (interpolated, missing files skipped).
  Each renders through a throwaway Helm chart (`helm` must be installed) with the merged config values
  as `.Values` (`.Release` is the unit / `default`: use `.Values.obelisk`); all see the same input. An
  output must be one mapping or nothing, and may set `obelisk.release.{name, namespace}` like a config
  file. Takes `--repo` too. Each generator is its own `helm` run (targets × units × generators of them
  for a full run). A unit with generators needs a name Helm accepts as a release name (≤ 53 chars).
- **Release:** `obelisk.release.{name, namespace}` exists only to be `.Release.{Name, Namespace}` (and
  the Helm release `apply` will install). Like the version, each must have exactly one distinct
  value per unit on a target, from `release: { name, namespace }` in its target or unit
  (interpolated) or `obelisk.release.*` in a config file or generator; none is an error, as is a
  conflict. Both must be valid names; a release name at most 53 characters. Generators still render
  with `.Release` as the unit / `default` (the release isn't agreed until after them).
- `config repo …` works on the repo in the current directory, or with `--repo <name>`, on the one with
  that name in the global `repos.yaml` (paths are then shown under it, so they work from anywhere).
- `--help` groups options under Core, Extra and Special headings (as obeliskd does).
- `obelisk-k8s completions <bash|zsh|fish|elvish|powershell>`: prints a shell completion script.

## The design

A unit on a target goes through a few stages, each one inspectable with `template …`:

```text
config files ─▶ values ─▶ generators ─▶ generated values ─▶ templates ─▶ k8s resources ─▶ (apply)
               (merge +    (gotmpl,                         (gotmpl,
                obelisk:)   rendered by Helm)                 rendered by Helm)
```

- **Config files** (`configPaths`) deep-merge into the values, with the tool's `obelisk:` block on top.
- **Generators** (`generators:`) are Go templates that compute more values from those. Logic lives
  here, in the repo's own `.gotmpl` files.
- **Templates** (`templates:`) are Go templates that render k8s resources from the generated values.
  Generators and templates each render on their own through a throwaway Helm chart, so they get all of
  Helm's functions.
- **Helm** renders, and applies (`helm upgrade --install`): every unit is a Helm
  release. Upstream charts, raw YAML and YAML from a URL (wrapped into throwaway charts), and
  executable generators are planned, not built.
- **obelisk-k8s** will order units (CRDs before what uses them, across namespaces), wait for
  readiness, and resolve `ref+awsssm` refs (on the generated values, before templates).

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

## k3d dials (TODO)

Every k3d cluster is created the same way for now. The dials it will need, none configurable yet:

- [x] k3d's load balancer container: off (`--no-lb`).
- [x] k3s's traefik and servicelb (klipper): off; ingress and MetalLB come in as units.
- [x] The Docker network: obeliskd's docker subnet, when `obeliskd` is set.
- [x] MetalLB, with a pool of the gateway's obeliskd ip (`autoAssign: false`).
- [x] DNS: `<cluster>.obd.internal` and up to three wildcard levels below, at the gateway.
- [x] Teardown of obeliskd ips/hosts tagged with the cluster on `destroy`.
- [ ] More LoadBalancer ips (e.g. a database's own): one obeliskd ip each, added to the pool.
- [ ] HTTPS: cert-manager, a local CA and a wildcard cert on the gateway; trust-manager.
- [ ] Port mappings to the host (none: on Linux, the Docker network is reachable directly).
- [ ] Servers / agents count; memory limits. (With several servers, "running" means all of them.)
- [ ] The obeliskd subnet's size: the "allocate one" hint suggests `--size 22 --reserve-size 24`;
      obelisk-k8s could allocate it itself (and ask obeliskd with `--supernet` / `--subnet` filters).
- [ ] k3s version / image.
- [ ] A registry (k3d's own, or a shared one) and registry mirrors.
- [ ] Volumes (host paths), e.g. for local storage or a shared image cache.
- [ ] The API server's host port (random now) and its address on the obeliskd network.
- [ ] Other k3s args (e.g. disabling metrics-server or local-storage).

## Open design questions

Nothing below is decided. Don't silently pick an answer in code; raise it.

- The generator contract: inputs, output location and format.
- Do our own apps need Helm charts, or can a generator emit plain YAML?
- Do raw/URL YAML units still get `ref+awsssm` resolution once wrapped (obelisk-local's `doTemplate`)?
- CRD upgrades, given Helm never upgrades `crds/`.
- ~~Is apply part of v1, or only `template`?~~ Decided: it is (`obelisk-k8s apply`, above).
- Does the tool own instance identity for clean up/down (v1.1)? Hook shape (v1.2)?

These are tracked in the Aletheia knowledge graph, project OBLSK (start at OBLSK/I-102).
