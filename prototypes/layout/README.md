# Layout prototype (paper)

What an app repo's deploy description looks like on disk for obelisk-k8s. This is a paper design: nothing
here is parsed yet. The example trees are:

- `mission-control/`: the v1 target. Shows units for infra, internal and mission-control (auth, llm,
  renewal-watcher and renewal-processor look the same as internal), plus v1.1 and v1.2 bits on paper.
- `homelab/`: a sample of its 30-odd units, covering Helm charts (repo and OCI), raw YAML in the repo,
  YAML from a URL, and SSM refs inside raw YAML.
- `home/`: v1.3 on paper, the `~/.obelisk-k8s/` registry and a whole-cluster spec.

Tracked in the Aletheia knowledge graph under OBLSK/T-12 and OBLSK/T-13.

## The shape

```text
<repo>/.obelisk-k8s/
  repo.yaml                     name (the repo's v1.3 address), helmRepos
  targets/<target>.yaml         kubeContext, aws profile, default namespace
  values.yaml, values.<target>.yaml   shared files that units list
  units/[<group>/...]<unit>/
    unit.yaml                   the source (one of chart / manifests / url / generate), values, namespace, dependsOn, targets
    values*.yaml
    manifests.yaml, ...         raw YAML, beside unit.yaml
```

A unit is any directory under `units/` that holds a `unit.yaml`. Its name is the directory's name, which
must be unique in the repo (homelab's names already are). Group directories exist for humans only.

The root is `.obelisk-k8s/`, not `.obelisk/`, because obelisk-local keeps `.obelisk/` during the
side-by-side period (OBLSK/I-97).

## unit.yaml

```yaml
# exactly one source:
chart: { repo: <helmRepos key>, name: x, version: 1.2.3 }   # or { oci: oci://..., version } or { path: ./chart }
manifests: [manifests.yaml]                                   # files beside unit.yaml
url: https://.../v1.4.1/install.yaml                          # optional sha256:
generate: { command: java, args: [...] }                      # alone: emits manifests. With chart: emits values

values: { prod: [../../values.yaml, values.yaml] }            # per release-target, merged in order

namespace: { name: x, create: true, privileged: true }        # default: the release-target's namespace, not created
dependsOn: [other-unit]                                       # ordering; v1.3 reserves `repo:unit`
targets: [prod]                                               # default: every release-target
```

- Every unit becomes one Helm release, named after the unit. `manifests`, `url` and generated manifests are
  wrapped into a throwaway chart (OBLSK/I-101).
- `namespace.create` and `namespace.privileged` match obelisk-local's `managed` and `privileged`. No namespace
  and none on the target is obelisk-local's `none`: the manifests carry their own namespaces.

## Values: an explicit list per unit (Kalin, locked)

```yaml
values:
  prod: [../../values.yaml, ../../values.prod.yaml, values.yaml, values.prod.yaml]
```

Each unit's `values` maps a target to its values files, which are deep-merged in order. Paths are relative
to the unit dir. A target the unit doesn't list gets no values files. This is keyed per target rather than
obelisk-local's unit list plus target list, so shared and per-unit files can interleave the way
mission-control's do today.

## The generator contract (OBLSK/Q-23)

```text
<command> <args...> --values <merged.yaml> --context <context.yaml> --output <file>
```

- `merged.yaml` is the unit's listed values, merged, with SSM refs already resolved (see below).
- `context.yaml` holds what the tool knows: `repo`, `unit`, `target`, `namespace`, and from v1.1 `instance`.
  These go in a file, not env vars.
- The output is plain k8s manifests if the unit has no `chart`, or a values file layered last onto the chart
  if it has one.
- cwd is the repo root, as today, so `target/obelisk-build.jar` resolves.

## SSM refs (OBLSK/Q-29)

The tool resolves `ref+awsssm` refs (whole string only, including `+json` and `+yaml`) in everything
repo-authored that enters the pipeline: the merged values (before the generator or Helm sees them), and the
`manifests` and `url` documents (before wrapping). Generator output is **not** resolved. There's no
`doTemplate` flag: resolution always happens.

The resolution has to happen before the generator because mission-control's generator embeds secrets inside
one JSON string: `stringData.secret.json`, from obelisk-yaml's `Enrich` (`encodeSplitRoot`). A
whole-string pass over the output would never find those refs. As a result the generator sees plaintext,
exactly as it does today.

## mission-control in v1 (OBLSK/Q-22)

Today, `obelisk-build` emits obelisk-local `Component`s, and obelisk-local's `Enrich` turns them into k8s.
obelisk-k8s has no Component model. So the smallest v1 is a new `obelisk-build` mode that runs `Enrich`
in-process (it's already on the classpath through `obelisk-builder`) and writes plain manifests. Each app unit
is then `generate` with no chart, and the v1 diff against `obelisk-local release template` is close to
identical by construction.

Our own apps don't need charts for v1. Charts stay for third-party software, and for small in-repo things
like the mocks server, where a chart is the easy path.

## v1.1 to v1.3 on paper

- **Instances (OBLSK/Q-27).** A target can say `instance: required` and use `${instance}` in its
  `namespace`. This is the only substitution the tool does, and only in target files. One instance is one
  namespace, so release names need no suffix, and `down` means uninstall every release in that namespace.
- **Global vs per-instance (OBLSK/I-96).** This is two targets, such as `ephemeral` and `ephemeral-global`,
  plus each unit's `targets:` and per-target values files. The tool has no global concept. Mocks are
  `targets: [ephemeral]`, so they can't reach prod.
- **Hooks (OBLSK/Q-24, v1.2).** `hooks.afterUp` / `beforeDown` in unit.yaml, each `{command, args}` and
  given the same context file. A sticking point: mission-control's app token is minted from internal after it
  comes up, then fed into mission-control's config. To keep the tool stateless, the hook should write a k8s
  Secret the app reads, rather than produce values for a later unit.
- **Whole cluster (v1.3).** `~/.obelisk-k8s/repos.yaml` maps repo names to paths. A cluster spec lists
  `{repo, target, instance?, units?}` entries and overrides kubeContext. The repo layout only has to provide
  a stable `repo.yaml` name, targets as files, and unit names. All three are in the v1 layout.

## Sizes: wrapping raw YAML is safe

Measured on homelab origin/main. Only the two upstream CRD bundles are large:

| unit | raw | gzip | largest doc |
|---|---|---|---|
| prometheus-operator-crds (URL, 10 CRDs) | 1.55 MB | 70 KB | 313 KB |
| gateway-api (URL, 6 CRDs) | 696 KB | 100 KB | 345 KB |
| monitoring-dashboards (4 ConfigMaps) | 135 KB | 9 KB | 71 KB |
| every other raw unit | under 12 KB | | |

- A Helm release Secret holds the gzipped manifest, base64-encoded: about 134 KB at most here, against a
  ~1 MiB cap. Every document is under the ~1.5 MiB object limit.
- The largest CRDs are over the 256 KiB `last-applied-configuration` limit, so client-side `kubectl apply`
  would fail on them. Helm doesn't use that annotation, and obelisk-local uses server-side apply. Whatever
  applies in v1 must not be client-side `kubectl apply`.
- The likely biggest releases are charts that haven't been rendered here yet: kube-prometheus-stack and gitlab.

## Not covered by the layout

- **Image builds.** These stay with `obelisk-local docker publish` (`images.yaml`) for now. k3d also needs
  images imported, which is v1.1 (probably a hook or a pre-step).
- **CRD upgrades (OBLSK/Q-28).** Wrapped `url` and `manifests` CRDs go in `templates/`, not `crds/`, so they
  upgrade. Charts that keep CRDs in `crds/` (envoy gateway-helm) are still open.
- **Gitignored local-only `secrets.yaml` units** (homelab's ai-secrets, gitlab-deps-secrets, and
  wordpress-mvd's extra values file). These should move to SSM refs rather than get a layout feature.
