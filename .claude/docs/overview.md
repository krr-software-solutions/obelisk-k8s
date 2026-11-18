# Overview

obelisk-k8s deploys apps to Kubernetes: real prod/dev clusters, and local
[k3d](https://k3d.io) clusters with ephemeral instances. It replaces obelisk-local (the Scala deploy
CLI, `~/dev/repo/current/obelisk-local`), which is frozen and keeps running mission-control prod
until cutover. IP allocation and DNS for local clusters come from
[obeliskd](https://github.com/krr-software-solutions/obeliskd), never from obelisk-local's
ip-allocator.

## Current state

- `obelisk-k8s config repo show`, run from a repo root: loads and checks `.obelisk-k8s/` (`repo.yaml`:
  `repo: { name, helmRepos, build }`; `targets/<name>/target.yaml`: `target: { cluster, release,
  ephemeral }`; `units/<name>/unit.yaml`: `unit: { targets, release, build }`), then prints the
  targets and units (each unit's build path, and whether it's the repo's). A target or unit is named
  by its directory; names must be DNS labels; stray entries in `targets/` and `units/` are skipped
  with a `WARN`. `configPaths`, `generators` and `templates` are gone: each is a load error (at its
  line and column) saying to use a build file's phases instead; so is a target's or unit's `name`
  (saying it's the directory).
- **Build files:** every unit has one: its own `build`, else the repo's (a unit with neither is a load
  error). It's an interpolated path (`${{obelisk:target}}`, `${{obelisk:unit}}`, and
  `${{obelisk:path:unit}}`, the unit's directory as `./units/<name>`; valid in a repo-level `build` too,
  where it's each unit's) that starts with `./` (from `.obelisk-k8s/`), `/`, or
  `${{obelisk:path:unit}}`. See **The design** below for what it renders.
- `obelisk-k8s config repo paths [-t <target>]... [-u <unit>]... [--explain]`: the build file each
  selected unit renders on each selected target (default: all of them). One case prints as is;
  several print one after another, each under a magenta `[ target=…, unit=… ]` header, skipping the
  pairs where the unit doesn't deploy to the target. `--explain` prints a table (as written, the file
  that sets it, as evaluated, found or missing), colored on a terminal. A missing build file is
  shown; rendering one is the error.
- `obelisk-k8s config global show`: loads and checks the machine-wide config (`~/.obelisk-k8s/`, or
  `--global-root` / `OBELISK_K8S_GLOBAL_ROOT`): `repos.yaml` (repo paths: relative to the global root, `~/`, or
  absolute; a trailing `/*` for every repo in a directory; names come from each repo's `repo.yaml`) and
  `clusters/<dir>/cluster.yaml` (and its optional `docker.yaml`). Prints the repos and clusters. A
  repo that won't load, or a target naming a cluster that isn't there, is a `WARN`.
- **A cluster's `docker.yaml`** (`clusters/<dir>/docker.yaml`): where its images push, and the
  credentials its pods pull them with. No selector: pods pull by the name an image is pushed as.

  ```yaml
  docker:
    push:
      prefix: docker.private.homelab-1.krr-software-solutions.com/krrss   # → <prefix>/<name>:<version>
      username: "${{obelisk:aws-ssm:/infra/homelab-1/nexus/push/username}}"   # optional, with password;
      password: "${{obelisk:aws-ssm:/infra/homelab-1/nexus/push/password}}"   #   else your docker login
    pull:                                       # optional: for `images pull-secret`
      auths:
        - host: docker.krr-software-solutions.com
          username: "${{obelisk:aws-ssm:/infra/homelab-1/nexus/pull/username}}"
          password: "${{obelisk:aws-ssm:/infra/homelab-1/nexus/pull/password}}"
  ```

  A local k3d cluster without one gets its images with `k3d image import`.
- `obelisk-k8s config global docker … --cluster <name>` edits it: `show` (passwords masked unless
  just one SSM ref), `push set [--prefix …] [--username … --password …]` (creating the file, which
  then needs `--prefix`), `push unset-credentials`, `pull add|set|remove --host …`, and `remove`.
  Credentials are taken as given (SSM refs, or plain text; single-quote a ref in the shell;
  `--password-stdin` keeps a plain-text one out of `ps` and history). Each change rewrites the file
  whole (comments in it are lost; a command that changes nothing doesn't write; through a symlink,
  the file it points to; mode 0600 when new, else kept, but tightened to the owner's when it holds a
  plain-text password), then loads the global config again, and puts the old file back if that
  fails. A `docker.yaml` that won't load can still be `remove`d.
- `obelisk-k8s images pull-secret --cluster <name> --namespace <ns> [--yes] [--force-conflicts] [--dry-run]`:
  obelisk-local's `docker add-pull-config`. In namespace `<ns>` on cluster `<name>` (from the global
  config, through its kubectl context) it server-side applies (field manager `obelisk-k8s`, outside
  any Helm release) the Secret `docker-pull` (a `kubernetes.io/dockerconfigjson`, `{ auths: { <host>:
  { username, password } } }` from the cluster's `pull.auths`, SSM refs resolved, base64 in `data`:
  `stringData` would always conflict with obelisk-local's ownership) and the namespace's `default`
  ServiceAccount with `imagePullSecrets: [docker-pull]`, so its pods pull without their charts saying
  so. That list is atomic to server-side apply: another manager's entries in it conflict, and
  `--force-conflicts` replaces them. The namespace must exist (run it after `apply`); no repo is
  needed. A remote cluster needs `--yes`; `--dry-run` prints them (SSM refs as written, other
  passwords masked unless they're just one SSM ref; `stringData`, for reading: the apply writes
  `data`), and needs neither `--yes` nor a kubectl context.
- **Blueprints** (`.obelisk-k8s/blueprints/<name>/blueprint.yaml`, in any repo; `config repo show`
  lists them): a reusable batch of units across repos, for one cluster. `blueprint: { ephemeral,
  defaults: { ref }, units: [{ repo, target, unit, ref?, dependsOn? }] }`. `ephemeral` (missing:
  false) is the whole blueprint's: every unit's target must agree. A unit is named `repo/target/unit`
  (distinct within the blueprint); `dependsOn` names others that way, and mustn't loop (units are
  ordered dependencies first, ties in written order). Each unit needs a ref, its own `ref` or
  `defaults.ref`.
- **Refs** are chains, `a|b|c`: the first candidate that names a commit wins. A candidate starting
  with one of the repo's remotes (`origin/…`) is looked up only after fetching the repo (`git fetch
  --all --prune`): at most once per repo per run, shared by the env's ref and every unit's, and only
  if some chain reaches a remote candidate (`--no-fetch` uses it as it is); any other (a local
  branch, tag or sha) is looked up locally, without the network. So code must be committed, and only pushed for a remote candidate.
  Note `origin/HEAD` only exists where something set it (`git remote set-head origin -a`).
- **Envs** (`<global root>/envs/<name>.yaml`, listed by `config global show`): a machine-local
  instance of a blueprint. `env: { repo, blueprint, ref, ephemeral }`: the repo the blueprint is in,
  its name, the ref (chain) of that repo to read it at, and the copy's name (required if the
  blueprint is ephemeral, else not allowed). An env file that won't load is a `WARN` (it's left
  out), and an error only when that env is asked for.
- `obelisk-k8s env show <NAME> [--no-fetch]`: resolves an env: pins the blueprint repo's ref, checks
  it out, loads the blueprint there, then pins and checks out each unit's repo, and checks the unit
  against its repo's config at that commit (target and unit exist, the unit deploys to the target,
  the target is ephemeral exactly when the blueprint is, every target deploys to one cluster, which
  is in the global config). Prints the plan, dependencies first; fetches and new checkouts on stderr.
  Every commit is checked out as a detached worktree of its registered repo at
  `<global root>/envs/target/<env>/<repo>/<full sha>/` (the blueprint repo's own commit too): each
  env has its own, reused while it resolves to that commit (one that's there but isn't that commit
  is an error saying how to remove it). Resolving (`env show` / `template` / `diff`) only adds
  checkouts; `env apply` removes, once it has applied, every checkout under the env's directory it
  no longer pins (`git worktree remove --force`, so anything generated in it goes too), and
  `env down` removes them all.
- `obelisk-k8s env template <build|values|manifests> <NAME>`, `env diff <NAME> [--reveal]
  [--exit-code]`, `env apply <NAME> [--yes] [--prune | --no-prune]` (all `[--no-fetch]`): resolve the
  env as `env show` does, then render / diff / apply each unit from its checkout, exactly as
  `template` / `diff` / `apply` would there, dependencies first. What the env adds:
  `obelisk.env: { name, blueprint, repo }` and `obelisk.paths.blueprint` (the directory holding
  `blueprint.yaml`, in the env's checkout; both `null` outside an env), so a build file can list
  values files in the blueprint repo (missing ones are skipped, as ever); for an ephemeral
  blueprint the copy is `env.ephemeral`, with **no** ephemeral values (tentative); and the release
  labels `obelisk-k8s/env` and `obelisk-k8s/blueprint`. `diff` plans every unit before touching the
  cluster (two units that would be the same release are an error), so a unit whose build needs
  another installed (e.g. an app token from its API) can't be diffed on a fresh copy. `apply`
  first finds the env's releases whose unit left the blueprint (labelled `obelisk-k8s/env=<NAME>`
  on any local cluster, with `repo/target/unit` labels naming no unit of it, so a blueprint that's
  emptied or moved cluster still finds them; a cluster it can't look on is a `WARN`), which need
  `--prune` or `--no-prune` (an error now, before anything changes; remote clusters aren't looked
  on, for now: an env on one is a `WARN`), and uninstalls what it prunes. Then it goes **unit by
  unit, dependencies first, planning each only once what it depends on is installed**: plan, check
  no earlier unit is the same release, check release ownership (**Release ownership**, below),
  build and deliver its images (as `images build` does, from its checkout; a tag already in the
  local docker daemon isn't rebuilt, still delivered. TODO (fast-follow): that trusts the tag to
  name its contents, fine for `0.0.0-<sha>`, stale for a fixed version, and k3d still re-imports
  what it already has), install. A unit that fails stops it, with the units before it installed.
  Last, it removes the checkouts it no longer pins.
- `obelisk-k8s env down <NAME>`: takes an env down, even one whose file is gone: uninstalls every
  release labelled `obelisk-k8s/env=<NAME>` on each local cluster with a kubectl context (one it
  can't look on, e.g. stopped, is a `WARN`; remote clusters aren't looked on, for now, which is a
  `WARN` naming them), then removes `envs/target/<NAME>/` (each checkout through
  `git worktree remove`). The env's file stays, so `env apply` brings it back.
- **Repos not checked out** in `repos.yaml`: a bare repo (a checkout holding only `.git`, or a
  `<name>.git` dir), or a checkout of a branch without `.obelisk-k8s/`, is a repo like any other:
  its `.obelisk-k8s/` is read from git at `origin/HEAD` (what the remote's default branch was at the
  last fetch), else `HEAD`, through a throwaway `git archive | tar` (no worktree), so `config global
  show` lists its targets, units and blueprints and `config repo show --repo <name>` prints them.
  There are no files to render from, so `template` / `diff` / `apply` / `images` `--repo <name>`
  refuse it: it deploys through an env (which checks it out). A repo whose config is only on an
  unmerged branch isn't found this way (register a checkout of that branch instead). A repo name
  listed in more than one place (e.g. a bare repo and a stale clone of it, or two worktrees under
  one `/*`) loads, with a `WARN`, but using it (`--repo`, an env) is an error naming every place:
  picking one could deploy from the wrong place.
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
- `obelisk-k8s template build [-t <target>]... [-u <unit>]...`: each selected unit's build plan on each
  selected target: its build file rendered (`helm`), checked, and printed as YAML. Same selection as
  `config repo paths`; several cases print as a YAML stream, each under a `# [ target=…, unit=… ]`
  comment. Takes `--repo` too (as does every `template` command); `--resolve` has nothing to resolve
  here.
- `obelisk-k8s template values [-t <target>]... [-u <unit>]...`: the values each selected unit ends up
  with on each selected target: the tool's `obelisk: { repo, target, unit, cluster, ephemeral, git,
  paths }` block, with every phase's values sources deep-merged over it in order (maps key by key;
  lists, scalars and `null` replace), and the agreed `obelisk.release` (see **Release** below).
  `obelisk:` is reserved: a values source may only set `obelisk.release.{name, namespace, version}`,
  as strings (kept per source and left out of the merge; they're agreed on after every phase), and
  anything else under it is an error (`obelisk.version`, the version's old place, says to move it to
  `obelisk.release.version`). SSM refs (`${{obelisk:aws-ssm:<key>}}`, below) show as written unless
  `--resolve`. Quoted strings stay as written; plain scalars come out parsed, the YAML 1.1 way where
  it matters (`0755` is `493`), with known limits listed in `config/values.rs`. Replaces the old
  `values` (config files only) and `generated-values`.
- `obelisk.paths: { repo: { root, obelisk }, global: { obelisk }, unit, target, blueprint }`: the repo
  root, the repo's `.obelisk-k8s/`, the global config's directory on this machine, the case's unit
  and target directories (where its `unit.yaml` / `target.yaml` are), and, as part of an env, the
  directory holding its `blueprint.yaml` in the env's checkout (`null` otherwise); absolute and
  normalized (`.` and `..` resolved; symlinks kept); an error if any isn't UTF-8. Alongside it,
  `obelisk.env: { name, blueprint, repo }` is the env (`null` outside one). Machine-specific, so rendered output
  that uses it differs between machines (a source path under the repo root is shown relative to it,
  though: see **The design**).
- `obelisk.cluster: { name, context, supernet, subnet }` is the target's cluster, from the global
  config: its name, the kubectl context it's reached through (k3d's own for a local k3d cluster,
  else its `kubectlContext`; `null` without one), and the obeliskd supernet and subnet it runs on
  (`null` without `obeliskd`). So every `template` command (and `apply`) loads the global config,
  and a selected target whose cluster isn't in it is an error.
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
  targets), and the values are the top layer: merged over the `obelisk:` block (so every phase sees
  them) and again over every phase's values (so they always win), under the same rules as a values
  source (so they can set `obelisk.release.version` too, and it shows up as `--ephemeral-values` or
  the path). The build file doesn't see them (its `.Values` is just the `obelisk:` block).
  `apply` and `diff` take the same flags.
- `obelisk.release.version`: after every phase's values, exactly one distinct version is required for
  each unit on each target, from any of: `release.version` in its target or unit (interpolated), or
  `obelisk.release.version` in a values source (or the ephemeral values). A top-level `version` in a
  target or unit (its old place) is a load error saying to move it under `release`. It's added to the
  values (at `obelisk.release.version`), and manifests' charts get it as
  `version` and `appVersion`, so it must be `SemVer`-ish for `template manifests` (e.g. `0.0.0-<sha>`,
  or a tag like `v1.2.3`). Note that `git describe` output like `v1.2.3-4-gabc1234` is SemVer for a
  *prerelease of* 1.2.3, so it sorts before `v1.2.3`.
- `obelisk-k8s template manifests [-t …]… [-u …]…`: the k8s resources each unit renders: every phase's
  manifests sources (see the types below), each rendered on its own against the values of the phases
  before its own (plus the agreed `obelisk.release`), e.g. through a throwaway Helm chart with them as
  `.Values`, joined in phase order then source order as Helm prints them (each document under a
  `# Source:` comment naming the source; Helm orders the documents *within* one template by kind). A
  source may print any number of documents. No shared `define`s across files for now (tentative).
  `.Release.{Name, Namespace}` is the case's agreed release (see below). A unit with no manifests
  sources (or only missing files) renders nothing, and its values phases don't run.
- `obelisk-k8s export --in-values-path a.b.c --out-values-path d.e.f [--format yaml|json]` (default
  yaml): for a `command` values source, which gets the values (YAML) on stdin. Writes what's at
  `a.b.c` to a file, to hand a values subtree to something that wants one:
  `{ type: command, command: [obelisk-k8s, export, --in-values-path, app.config, --out-values-path, files.config] }`.
  - If the values already have a path (a string) at `d.e.f`, e.g. from an `inline` source in an
    earlier phase, that's the file: from where it runs (a `command` source's is the repo root), its
    directories made, overwritten if it's there (its mode kept; a new one is 0600). It prints `{}`:
    the path is already in the values, and the same every run.
  - Otherwise (nothing, or `null`, there) it's a new file in the temp dir (mode 0600), and it prints
    `d: { e: { f: <its path> } }`, so later phases see the path. That name is random, so the path
    differs every run (keep it out of manifests, or `diff` always shows a change).
  Dot-separated keys into mappings (no key may hold a `.`; no list indices); nothing at
  `--in-values-path` is an error (a `null` there exports as `null`), and so is a
  `--out-values-path` under `obelisk:`, or one set to anything but a string or `null`. The command
  runs `obelisk-k8s` from `PATH`, which may not be the binary rendering. The file is kept (nothing
  deletes it). On `apply` / `template --resolve` the values have SSM refs resolved: don't export a
  subtree holding secrets (it's meant for things like `obeliskd batch alloc`'s input).
- `obelisk-k8s template images [-t …]… [-u …]…`: the image specs each unit declares: every phase's
  images sources, rendered like manifests sources (against the values of the phases before their
  own, plus the agreed release), each a YAML stream of specs, printed under a `# Source:` comment
  naming the source and phase (`images build` builds them). A spec:

  ```yaml
  name: krrss-mc/web                # a repository path, unique in the unit
  version: "1.2.3"                  # a docker tag (no `+`): the template decides (e.g. obelisk.release.version)
  preBuild:                         # optional: { type: file, path } or { type: command, command }
    type: file
    path: ./units/my-app/pre-image-build.sh
    cwd: ./units/my-app             # optional, default the repo root
    env: { MODE: release }          # optional: set over what it inherits
  dockerfile: { type: file, path: ./units/my-app/Dockerfile }
  contextPath: ./units/my-app       # optional: the build context, default the repo root
  ```

  A `name` is a docker repository path, kept as is wherever the image goes (so images keep their
  registry layout, e.g. `krrss-mc/auth/web-server`, instead of being flattened): `/`-separated path
  segments, each Docker's `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*` (lowercase letters and digits,
  separated by single `.`s or `_`s, `__`, or runs of `-`, as in a push prefix). With more than one,
  the first can't look like a registry host (have a `.`, or be `localhost`): docker would push it
  there. Docker allows a repository 255 characters, host included, so the name as docker builds it
  (`docker.io/library/<name>`, or `docker.io/<name>` with a `/`) must fit, and so must
  `<push.prefix>/<name>` (checked by `images build` and `template manifests`). Not a DNS label like
  the other names: it never names a file or a Helm release.
  The Dockerfile, a `preBuild` script, its `cwd` and the `contextPath` must exist. A phase's
  manifests see the images declared in earlier phases (never their own) as
  `obelisk.images.<name>.{ref, repository, tag}`: `ref` is `<push.prefix>/<name>:<version>` on a
  cluster with a `docker.yaml`, `<name>:<version>` on a local k3d cluster without one (`k3d image
  import`ed); any other cluster is an error, as for `images build`. So
  `image: {{ .Values.obelisk.images.web.ref }}` (a name with a `-`, `.` or `/` is still one key:
  `{{ (index .Values.obelisk.images "krrss-mc/web").ref }}`). Values sources don't see them: images render
  with the agreed release, which is only known after every values phase. So `template manifests`,
  `apply` and `diff` render (and check) the images sources of the phases before the last manifests
  phase.
- `obelisk-k8s images build [-t …]… [-u …]… [-i <image>]… [--no-push]`: builds those images, each tag
  (`<name>:<version>`) once however many cases declare it: its `preBuild` (in its `cwd`, with its
  `env`), then `docker build --file <dockerfile> --tag <name>:<version> <contextPath>`. Then (unless
  `--no-push`) delivers each to each selected target's cluster, once per destination: with a
  `docker.yaml`, `docker tag` + `docker push` as `<prefix>/<name>:<version>` (with its `push`
  credentials, SSM refs resolved, written to a throwaway docker client config on your own docker
  context, just before the push; else your own login), each ref once however many clusters push
  it the same way; a local k3d cluster without one, `k3d image import`; any other cluster is an
  error. Destinations and credentials are checked before anything builds. Tools' output goes to
  stderr; stdout lists what was built, pushed or imported. One tag declared two different ways is
  an error. `apply` doesn't build or push. A push `prefix` must start with a registry host (a `.`
  or `:`, or `localhost`), not Docker Hub.
- `obelisk-k8s apply [-t …]… [-u …]… [--ephemeral-*] [--yes] [--take-ownership] [--force-conflicts] [--prune | --no-prune]`: installs what `template manifests`
  renders, one Helm release per unit on a target: its agreed release name, into its agreed namespace
  (created if missing), at its agreed version, via a throwaway chart that ships the rendered
  manifests as-is (`{{ .Files.Get }}`, no second templating), with `helm upgrade --install
  --rollback-on-failure` on the target's cluster's context (`k3d-<name>` for a local k3d cluster,
  else its `kubectlContext`; none is an error). Everything is planned before anything installs:
  clusters resolved, every case rendered, and two cases that would be the same release on the same
  context (the second would replace the first), or a case whose manifests render nothing (an upgrade
  would empty the release), are errors. A remote cluster needs `--yes`. A unit with no manifests
  sources is skipped. Each install waits up to 5m and rolls back if it doesn't come up; if one fails, the ones
  before it stay applied. Takes `--repo` too; the clusters come from the global config. Two
  one-offs, named as Helm's, for moving resources over from obelisk-local: `--take-ownership` adopts
  resources already in the cluster that aren't the release's, another release's included (without
  it Helm refuses them: "invalid ownership metadata"), and then a failed install isn't rolled back
  (the revision it would go back to doesn't have what was adopted, so Helm would delete it), nor is
  a release left failed by any later run;
  `--force-conflicts` forces server-side apply (which the release then keeps) and takes fields
  another field manager owns (left on, it fights what the cluster manages). A refusal either would
  get past (without it already on) says which.
- **Release labels:** `apply` puts `obelisk-k8s/repo`, `obelisk-k8s/target`, `obelisk-k8s/unit`,
  `obelisk-k8s/ephemeral` (the copy's name) and `obelisk-k8s/sha` (`obelisk.git.sha.value`) on every
  release (Helm's `--labels`, on its release record, merged over the ones there). `ephemeral` and
  `sha` are removed (`null`) when there's none, on an upgrade; a first install leaves them out (it
  would keep `null` as text). Find them with `helm list -A -l obelisk-k8s/unit=app`, read them with
  `helm get metadata` (`helm list` never prints them). `env apply` adds `obelisk-k8s/env` and
  `obelisk-k8s/blueprint` (removed, likewise, outside an env).
- **Release ownership:** before installing anything, `apply` and `env apply` read the labels of
  every release they'd install over, and refuse to take one from its owner: a plain `apply` never
  installs over an env's release (`obelisk-k8s/env` set), and an env installs only over its own,
  never another env's nor one applied outside an env. A release without labels (from before them)
  is anyone's.
- **Units that are gone:** a release labelled with this repo and a selected target (and the same
  ephemeral copy, or none), applied outside an env (an env prunes its own), whose unit isn't in the
  repo for that target any more (deleted, or no longer deploying there). `apply` looks for them on
  each selected target's context (`-t`, else every target; a target with no units left counts, so
  `apply` with nothing left to install only prunes), as the selected ephemeral copy on an ephemeral
  target (`helm list -A -l …,obelisk-k8s/unit!=<each unit>`); a release the plan installs anyway
  (same name, namespace, context) isn't one. If there are any, `apply` needs `--prune` (uninstall each,
  waiting up to 5m, before installing anything) or `--no-prune` (leave them, without looking);
  without either it's an error listing them. Unlabelled releases (from before labels) aren't seen.
  `--prune` refuses a `-u` that leaves out a unit of a selected target (with a subset, a release
  another unit now installs, e.g. a renamed unit keeping its release name, could look gone), and
  refuses while the repo skips any `targets/` or `units/` entry (a unit whose `unit.yaml` is
  missing would look gone). On an ephemeral target, `--ephemeral-name` picks the copy to look at,
  even when the target has no units left. A cluster it can't look on is an error for a target
  something installs on, or with `--prune`; otherwise a `WARN`.
  For now all of this is only for local clusters: remote targets aren't looked at (with `--prune`
  or `--no-prune`, each selected one is a `WARN` saying so; without, nothing is said, as for CI on
  a remote cluster), so the flags only ever act on local ones.
- `obelisk-k8s diff [-t …]… [-u …]… [--ephemeral-*] [--take-ownership] [--reveal] [--exit-code]`:
  what `apply` would change, without changing anything. Planned exactly as `apply` plans (the same
  checks and errors, charts rendered for the cluster's capabilities), remote clusters included
  without `--yes` (it only reads). Each case with manifests goes into the same throwaway chart, and
  the [helm-diff](https://github.com/databus23/helm-diff) plugin diffs it against what's live on its
  context (`helm diff upgrade --allow-unreleased --three-way-merge --normalize-manifests`; a release
  not installed yet shows as all new). Needs the plugin (`helm plugin install
  https://github.com/databus23/helm-diff`); without it, it fails before rendering anything, saying
  so. Prints each case under a magenta `[ target=…, unit=…, release=…, namespace=…, context=… ]`
  header, then helm-diff's output (colored on a terminal), or `no changes` (`no manifests: nothing
  to diff` for a unit without manifests sources). `--take-ownership` diffs as `apply
  --take-ownership` would install (without it, a resource that isn't the release's is the same
  refusal `apply` gets, with the same hint); there's no `--force-conflicts` (helm-diff has none,
  and a forced server-side dry run without it fails on the very conflicts it would take).
  `--exit-code` exits 2 if anything would change (helm-diff printed a diff), as helm-diff and
  terraform do: an error is `ERROR - …` and exit 1, so CI can tell drift from failure. SSM refs always resolve, and resolved values print masked
  (errors too), with helm-diff masking Secrets' data, unless `--reveal` (both in plaintext, with a
  warning on stderr). Masking covers the values resolved this run: an older value still in the
  cluster can show in plaintext where it isn't in a Secret.
- **Release:** `obelisk.release.{name, namespace}` exists only to be `.Release.{Name, Namespace}` (and
  the Helm release `apply` will install), with `obelisk.release.version` (above) as its version. Like
  the version, each must have exactly one distinct value per unit on a target, from
  `release: { name, namespace, version }` in its target or unit (interpolated) or `obelisk.release.*`
  in any phase's values sources (or the ephemeral values); none is an error, as is a conflict. Name and
  namespace must be valid names; a release name at most 53 characters. The build file and Go template
  values sources render with `.Release` as `obelisk-build` / `default`, whatever the unit (the release
  isn't agreed until after every phase's values): use `.Values.obelisk` there.
- `config repo …` works on the repo in the current directory, or with `--repo <name>`, on the one with
  that name in the global `repos.yaml` (paths are then shown under it, so they work from anywhere).
- `--help` groups options under Core, Extra and Special headings (as obeliskd does).
- `obelisk-k8s completions <bash|zsh|fish|elvish|powershell>`: prints a shell completion script.

## The design

A unit on a target goes through a few stages, each one inspectable with `template …`:

```text
build file ─▶ build plan ─▶ phase 1 … phase n ─▶ values (+ agreed release) ─▶ manifests ─▶ (apply)
(Go template,  (`template    (values sources,      (`template values`)          (`template
 rendered by    build`)       merged in order)                                   manifests`)
 Helm)
```

- **The build file** (a unit's `build`, else the repo's) is a Go template, rendered through a throwaway
  Helm chart with just the `obelisk:` block as `.Values`. It's where the logic of what a unit is made
  of lives (per target, per unit, …). It must print one mapping, the **build plan**:

  ```yaml
  build:
    phases:
      - name: base                  # a valid name, unique in the plan
        values:
          - { type: inline, values: { replicas: 1 } }
          - { type: gotmpl, path: "{{ .Values.obelisk.paths.repo.obelisk }}/{{ .Values.obelisk.target }}.values.yaml.gotmpl" }
      - name: out
        manifests:
          - { type: gotmpl, path: ./app.yaml.gotmpl }
        images:                     # image specs (see `template images`, `images build`)
          - { type: gotmpl, path: ./images.yaml.gotmpl }
  ```

- **Phases** run in order. V₀ is the `obelisk:` block (with the ephemeral values over it). Everything in
  phase k, its values, manifests and images sources, renders against V₍ₖ₋₁₎, the values after the
  phases before it; its own sources don't see each other. Phase k's values outputs then deep-merge over
  V₍ₖ₋₁₎, in source order, into Vₖ (and the ephemeral values over that again). A phase may have values,
  manifests and images sources (any of them, or none), in any number and order. After every phase's values, the
  release is agreed on (**Release**, above); then each phase's manifests (and images) render against its V₍ₖ₋₁₎ with
  the agreed `obelisk.release.{name, namespace, version}` added, and `template values` is Vₙ with it.
- **Sources** are `{ type, … }`, and each renders on its own:
  - `{ type: inline, values }`: a values mapping, as it is. Values only.
  - `{ type: gotmpl, path }`: a Go template, rendered through a throwaway Helm chart (all of Helm's
    functions; `.Values` is the values).
  - `{ type: file, path, parse? }`: used as it is (nothing in it is templated). As a values source
    it's read as YAML with `parse: true`, or as one string (the file's text, verbatim) with
    `parse: false`, which needs a `nestOutput` to go under (e.g. a cert: `{ type: file, path:
    ./tls.pem, nestOutput: tls.cert }`). Without `parse`, it's `true` for a `.yaml`, `.yml` or
    `.json` (any case) and `false` otherwise, so a values file named otherwise needs `parse: true`.
    Elsewhere `parse` is an error: a manifests source is used as it is, an images one read as YAML.
  - `{ type: url, url, sha256?, parse? }`: fetched with `curl` every run (https only, redirects too; 2m
    timeout), checked against `sha256` when given (without one, a `WARN`, printed as soon as the plan
    is built: before anything renders from it or installs), used as it is. `parse` as for a `file`,
    by the extension of the URL's path (its query and fragment left off).
  - `{ type: helm, chart, repo?, version, unnestInput? }`: an upstream chart, `helm template`d as
    the case's release, CRDs included. `repo` is a URL or a name in `repo.yaml`'s `helmRepos`; an
    `oci://` chart takes none. Its values are the mapping at `unnestInput` (below), or none without
    one (never all the values). Manifests only. `apply` renders it for the target
    cluster's capabilities (its Kubernetes and API versions, from `kubectl` on its context, so charts
    that check `.Capabilities` render what that cluster can take); `template` renders offline, with
    Helm's defaults.
  - `{ type: command, command: [program, args…], env?, parse? }`: run from the repo root with the
    values (YAML) on stdin; its stdout is the output (as a values source, read as YAML unless it
    says `parse: false`, as for a `file`; there's no extension to go by). `env` (`{ NAME: value }`, strings only) is set over
    the environment it inherits. `./…` resolves from `.obelisk-k8s/`, an absolute path is used as
    it is, a bare name is looked up on `PATH` (anything else is an error). No timeout. Its
    `# Source:` names only the program (arguments may hold secrets).
  - **Nesting**, on any source where it means something: `unnestInput: a.b.c` gives it only what's
    at `a.b.c` in the values (as `.Values` for a `gotmpl`, which needs a mapping there; on stdin for
    a `command`, any value), not all of them, so it loses `.Values.obelisk` too; nothing there is an
    error. `nestOutput: d.e.f` puts a values source's output (any one value, a list or scalar too)
    at `d.e.f` before it merges; an empty output still merges nothing. Keys are dot-separated into
    mappings (no key may hold a `.`; no list indices). `unnestInput` is for `gotmpl`, `command` and
    `helm` (the types with an input); `nestOutput` for every type but `helm`, in `values:` only (and
    never under `obelisk:`). Anything else is an error when the plan is built.
  A `path` is absolute or starts with `./` (from `.obelisk-k8s/`), and is skipped if it's missing.
  It's kept absolute and normalized (`..` resolved), and shown relative to the repo root when it's
  under it (`.obelisk-k8s/…`, `.` for the root), so `# Source:` comments and errors are the same on
  every machine. Image spec paths are kept absolute too, and `template images` prints them so. Nothing in a plan is `${{obelisk:…}}`
  interpolated (it's a Go template's output: use `.Values.obelisk`), and its strings are read like a
  values file's (a plain `1` is a number: quote `version: "1"`). A values source's output must be one
  mapping (or nothing); a manifests source's is resources, each from a `file`, `url` or `command`
  put under its own `# Source:` comment.
- **Helm** renders, and applies (`helm upgrade --install`): every unit is a Helm release, whatever
  its manifests sources' types. An upstream chart's resources are installed as part of the unit's
  release (not as that chart's own release), hooks and CRDs included as rendered; a CRD and resources
  of its kind in one unit may fail on a first install (put them in separate units). Helm 4 upgrades
  with server-side apply: a field something else in the cluster owns (an HPA's `spec.replicas`) is a
  conflict, so don't set what the cluster manages. Each build file and Go template source is its own
  `helm` run.
- **SSM** (`ssm.rs`): `${{obelisk:aws-ssm:<key>}}` in a string (anywhere in it; never a key) is the
  SSM parameter `<key>` (a `:version` / `:label` selector works), decrypted; `$${{obelisk:` writes a
  literal `${{obelisk:` (a `${{` that isn't `obelisk:` is left alone), any other `${{obelisk:…}}` in values is an error, and so is obelisk-local's
  `ref+awsssm://…`. Refs are resolved in each values source's output (an `inline` one's too, and the
  ephemeral values) before it merges, so every later phase, Go template and chart sees plaintext (a
  `url` values source with refs needs its `sha256`); and in `file` / `url` manifests sources (a `url`
  with refs needs its `sha256`); never in the build file's output, nor in what `gotmpl`, `helm` or
  `command` manifests sources print. A `file` / `url` manifests source with refs is re-written from its parsed form, so its
  comments go and its plain scalars are re-spelled as in values (`08` → `8.0`): quote them. `apply`
  and `diff` always resolve; `template` only with `--resolve` (else refs are only checked), and then
  prints every resolved value masked as `<aws-ssm:KEY>` (in errors too; every occurrence, so a short
  value like `5432` masks unrelated text too), as `diff` does, unless `--reveal` (which needs
  `--resolve`) asks for plaintext: the one deliberate exception to never printing a resolved value
  (with `diff --reveal`), with a warning on stderr. Credentials are read the first time a ref is found:
  `AWS_SSM_ACCESS_KEY`, `AWS_SSM_SECRET_KEY` and `AWS_SSM_REGION` if all are set (the GitLab CI
  variables), else the named profile `AWS_SSM_PROFILE` (`~/.aws`; SSO, assume-role and
  `credential_process` work; its region unless `AWS_SSM_REGION` is set), else it's an error. A
  source's keys are fetched together (Rust AWS SDK, `GetParameters` in tens) and kept for the run;
  missing ones fail naming them all.
- **obelisk-k8s** will order units (CRDs before what uses them, across namespaces), and wait for
  readiness, and build a phase's `images` (only parsed, for now).

## Roadmap

| Phase | What |
|---|---|
| v1   | a render-only `template` command for prod/dev targets, diffed against obelisk-local on mission-control. Home lab is a layout check (raw YAML, URL YAML, Helm charts) |
| v1.1 | ephemeral instances on local k3d clusters, the first real use |
| v1.2 | up/down hooks: setup, database/topic creation, seeding test data |
| v1.3 | a whole-cluster spec across repos/apps: blueprints and envs (`env show` / `template` / `diff` / `apply` / `down`) |

Design the layout format on paper against v1.3, but build only the current phase.

Wanted, deferred for now:
- [ ] SSM refs written as-is in `.gotmpl` files: today Helm reads the `{{` in `${{obelisk:aws-ssm:…}}`
  as an action, so a ref there must be escaped (`{{ "${{obelisk:aws-ssm:/x}}" | quote }}`), or kept in
  a plain `type: file` values file. The idea: rewrite each `${{obelisk:…}}` into that escaped form
  before Helm renders any build file or gotmpl source (`$${{` stays a literal).

## Rules

- **Never print resolved secret values:** not in logs, errors or CI traces.
- **No Obelisk-level Secret vs ConfigMap split.** Each app decides what goes where.
- **Global vs per-instance** deployments are a pattern users build, not a tool feature.
- **Config comes from files, not env vars.** Prefer a YAML config file over threading env vars
  through a values source.

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

- ~~The generator contract: inputs, output location and format.~~ Decided: build files and phases
  (**The design**, above): a values source gets the values before its phase (YAML on stdin, or
  `.Values`) and outputs one mapping.
- Do our own apps need Helm charts, or can a Go template / command emit plain YAML?
- ~~Images in a phase (`images:`): what they declare.~~ Decided: image specs (`template images`,
  above), any number per source; images sources are `gotmpl`, `file`, `command` or `inline`.
- ~~Building and pushing images.~~ Decided: `images build` (plain `docker build`, tagged
  `<name>:<version>`, context `contextPath`), pushing per the cluster's `docker.yaml` or
  `k3d image import`, unless `--no-push`; manifests get the refs under `obelisk.images`; the pull
  secret is `images pull-secret` (outside `apply`). Still open: whether `apply` builds or applies
  the pull secret.
- ~~Do raw/URL YAML units still get SSM resolution (obelisk-local's `doTemplate`)?~~ Decided: `file`
  and `url` manifests sources always do (**SSM**, above).
- CRD upgrades, given Helm never upgrades `crds/`.
- TODO: a chart is manifests only, so there's no way to get its default values (`helm show
  values`) into the values. Probably wants cleaning up, e.g. a `type: helmValues` values source
  (with `nestOutput`); not decided.
- ~~Is apply part of v1, or only `template`?~~ Decided: it is (`obelisk-k8s apply`, above).
- Does the tool own instance identity for clean up/down (v1.1)? Hook shape (v1.2)?
- Blueprints/envs: env applies pass no ephemeral values (tentative); reusing local images only when
  they're really the same (TODO above); `env down` only looks on local clusters.

These are tracked in the Aletheia knowledge graph, project OBLSK (start at OBLSK/I-102).
