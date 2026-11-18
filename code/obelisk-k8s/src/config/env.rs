//! An env: a machine-local instance of a blueprint, `<global root>/envs/<name>.yaml`.
//!
//! ```yaml
//! env:
//!   repo: blueprints                   # the repo the blueprint is in (a name in repos.yaml)
//!   blueprint: krrss                   # its name there
//!   ref: origin/envs/abc|origin/HEAD   # which commit of that repo to read it at (a ref chain)
//!   ephemeral: eph-1                   # the copy's name: required if the blueprint is ephemeral,
//!                                      # else left out
//! ```
//!
//! An env is named by its file (a valid name). Resolving one ([`GlobalConfig::resolve_env`]) pins
//! every commit it needs (the blueprint repo's, then each unit's), checks each out (see
//! `checkout`), and checks the units against their repos' config at those commits: each target and
//! unit exists, the unit deploys to the target, every target is ephemeral exactly when the
//! blueprint is, and they all deploy to one cluster, which is in the global config.
//!
//! Each env has its own checkouts, `envs/target/<env>/<repo>/<sha>/`, so what's there is what it
//! has pinned. Resolving only adds them.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ObeliskK8sError;
use crate::checkout::{self, Fetcher, RefChain, Resolved};
use crate::config::blueprint::{Blueprint, UnitId};
use crate::config::global::{GlobalConfig, Repo};
use crate::config::repo::RepoConfig;
use crate::config::{NAME_RULE, check_name, config_error, is_valid_name};
use crate::yaml::{self, Spanned};

/// An env, as its file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    pub name: String,
    /// As the user should see it.
    pub file: PathBuf,
    /// The repo the blueprint is in.
    pub repo: String,
    pub blueprint: String,
    /// Which commit of `repo` to read the blueprint at.
    pub git_ref: RefChain,
    /// The copy's name, for an ephemeral blueprint.
    pub ephemeral: Option<String>,
}

/// Every `envs/<name>.yaml` under `root` (shown as `shown`), sorted by name. Anything else in
/// `envs/` (e.g. `target/`, the checkouts) is skipped quietly; a `.yaml` file with a bad name is a
/// warning.
pub(crate) fn load_envs(
    root: &Path,
    shown: &Path,
    warnings: &mut Vec<String>,
) -> Result<Vec<Env>, ObeliskK8sError> {
    let dir = root.join("envs");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let io_error = |source| ObeliskK8sError::Io {
        path: shown.join("envs"),
        source,
    };
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(io_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_error)?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "yaml"))
        .collect();
    paths.sort();
    let mut envs = Vec::new();
    for path in paths {
        let file_name = path.file_name().unwrap_or_default();
        let file = shown.join("envs").join(file_name);
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| is_valid_name(s))
        else {
            warnings.push(format!(
                "{}: ignored: an env's file name must be `<name>.yaml` ({NAME_RULE})",
                file.display()
            ));
            continue;
        };
        match load_env(&path, file.clone(), name) {
            Ok(env) => envs.push(env),
            // One broken env mustn't stop every command: it's an error only when it's asked for.
            Err(error) => warnings.push(format!("{}: skipped: {error}", file.display())),
        }
    }
    Ok(envs)
}

/// Env `name` from its file at `path` (shown as `file`).
fn load_env(path: &Path, file: PathBuf, name: &str) -> Result<Env, ObeliskK8sError> {
    let EnvFile { env } = yaml::read_file(path, &file)?;
    check_name(&file, &env.repo)?;
    check_name(&file, &env.blueprint)?;
    if let Some(ephemeral) = &env.ephemeral {
        check_name(&file, ephemeral)?;
    }
    let git_ref = RefChain::parse(&env.git_ref.value)
        .map_err(|m| config_error(&file, Some(env.git_ref.at), m))?;
    Ok(Env {
        name: name.to_owned(),
        repo: env.repo.value,
        blueprint: env.blueprint.value,
        git_ref,
        ephemeral: env.ephemeral.map(|e| e.value),
        file,
    })
}

/// A ref chain pinned to a commit, and where that commit is checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub git_ref: RefChain,
    pub resolved: Resolved,
    /// The real path.
    pub checkout: PathBuf,
    /// Whether this run added it (else it was already there).
    pub new: bool,
}

/// One unit of an env, pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvUnit {
    pub id: UnitId,
    pub depends_on: Vec<UnitId>,
    pub at: Pinned,
}

/// An env, resolved: every commit pinned and checked out, every unit checked.
#[derive(Debug, Clone)]
pub struct EnvPlan {
    pub env: Env,
    pub blueprint: Blueprint,
    /// The blueprint repo's commit.
    pub blueprint_at: Pinned,
    /// The one cluster every unit's target deploys to; `None` with no units.
    pub cluster: Option<String>,
    /// Dependencies first.
    pub units: Vec<EnvUnit>,
    /// Each checkout's repo config, by its real path.
    pub configs: HashMap<PathBuf, RepoConfig>,
}

impl GlobalConfig {
    /// The env named `name`.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if there's none, naming the ones there are.
    pub fn env(&self, name: &str) -> Result<&Env, ObeliskK8sError> {
        if let Some(env) = self.envs.iter().find(|e| e.name == name) {
            return Ok(env);
        }
        // Its file may be there but broken (skipped at load, with a warning): say what's wrong.
        let path = self.dir.join("envs").join(format!("{name}.yaml"));
        if path.is_file() {
            let shown = self.root.join("envs").join(format!("{name}.yaml"));
            load_env(&path, shown, name)?;
        }
        Err(ObeliskK8sError::Usage(format!(
            "no env `{name}` in {} (there are: {})",
            self.root.join("envs").display(),
            known(self.envs.iter().map(|e| e.name.as_str()))
        )))
    }

    /// The repo named `name`.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if there's none, naming the ones there are.
    pub fn repo(&self, name: &str) -> Result<&Repo, ObeliskK8sError> {
        self.repos.iter().find(|r| r.name == name).ok_or_else(|| {
            ObeliskK8sError::Usage(format!(
                "no repo `{name}` in {} (repos: {}; `config global show` lists any that didn't load)",
                self.root.join("repos.yaml").display(),
                known(self.repos.iter().map(|r| r.name.as_str()))
            ))
        })
    }

    /// Resolves env `name`: pins and checks out the blueprint repo, loads the blueprint, then pins,
    /// checks out and checks each unit (see the module docs). `progress` hears about each fetch and
    /// each new checkout.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] for an unknown env, repo, blueprint, target or unit, or an env
    /// that doesn't fit its blueprint; [`ObeliskK8sError::Git`] for a ref that won't resolve or a
    /// checkout that fails; and whatever loading a repo's config at a commit fails with.
    pub fn resolve_env(
        &self,
        name: &str,
        fetcher: &mut Fetcher,
        progress: &mut dyn FnMut(&str),
    ) -> Result<EnvPlan, ObeliskK8sError> {
        let env = self.env(name)?.clone();
        let mut configs: HashMap<PathBuf, RepoConfig> = HashMap::new();

        let blueprint_at = self.pin(&env.name, &env.repo, &env.git_ref, fetcher, progress)?;
        let config = load_at(&mut configs, &env.repo, &blueprint_at)?;
        let blueprint = config.blueprint(&env.blueprint)?.clone();
        let whose = |what: &str| format!("env `{}` ({}): {what}", env.name, env.file.display());
        match (blueprint.ephemeral, &env.ephemeral) {
            (true, None) => {
                return Err(ObeliskK8sError::Usage(whose(&format!(
                    "blueprint `{}/{}` is ephemeral, so the env needs `ephemeral: <name>`",
                    env.repo, blueprint.name
                ))));
            }
            (false, Some(_)) => {
                return Err(ObeliskK8sError::Usage(whose(&format!(
                    "blueprint `{}/{}` isn't ephemeral, so the env can't set `ephemeral`",
                    env.repo, blueprint.name
                ))));
            }
            _ => {}
        }

        let mut units = Vec::new();
        let mut cluster: Option<(String, UnitId)> = None;
        for unit in blueprint.ordered() {
            let id = &unit.id;
            let at = self.pin(&env.name, &id.repo, &unit.git_ref, fetcher, progress)?;
            let config = load_at(&mut configs, &id.repo, &at)?;
            let short = &at.resolved.sha[..12.min(at.resolved.sha.len())];
            let in_repo = |what: &str| {
                ObeliskK8sError::Usage(format!(
                    "blueprint `{}/{}`, unit `{id}`: {what} (repo `{}` at {short})",
                    env.repo, blueprint.name, id.repo
                ))
            };
            let target = config
                .targets
                .iter()
                .find(|t| t.name == id.target)
                .ok_or_else(|| in_repo(&format!("no target `{}`", id.target)))?;
            let repo_unit = config
                .units
                .iter()
                .find(|u| u.name == id.unit)
                .ok_or_else(|| in_repo(&format!("no unit `{}`", id.unit)))?;
            if !config
                .unit_target_names(repo_unit)
                .contains(&id.target.as_str())
            {
                return Err(in_repo(&format!(
                    "unit `{}` doesn't deploy to target `{}`",
                    id.unit, id.target
                )));
            }
            if target.ephemeral != blueprint.ephemeral {
                let (is, bp) = if target.ephemeral {
                    ("is", "isn't")
                } else {
                    ("isn't", "is")
                };
                return Err(in_repo(&format!(
                    "target `{}` {is} ephemeral, but the blueprint {bp}",
                    id.target
                )));
            }
            match &cluster {
                None => {
                    self.cluster(&target.cluster)
                        .map_err(|e| in_repo(&e.to_string()))?;
                    cluster = Some((target.cluster.clone(), id.clone()));
                }
                Some((first, first_id)) if *first != target.cluster => {
                    return Err(in_repo(&format!(
                        "target `{}` deploys to cluster `{}`, but `{first_id}`'s deploys to `{first}`: a blueprint deploys to one cluster",
                        id.target, target.cluster
                    )));
                }
                Some(_) => {}
            }
            units.push(EnvUnit {
                id: id.clone(),
                depends_on: unit.depends_on.clone(),
                at,
            });
        }

        Ok(EnvPlan {
            env,
            blueprint,
            blueprint_at,
            cluster: cluster.map(|(c, _)| c),
            units,
            configs,
        })
    }

    /// Resolves `git_ref` in repo `repo`, and checks the commit out.
    fn pin(
        &self,
        env: &str,
        repo: &str,
        git_ref: &RefChain,
        fetcher: &mut Fetcher,
        progress: &mut dyn FnMut(&str),
    ) -> Result<Pinned, ObeliskK8sError> {
        let found = self.repo(repo)?;
        let resolved = checkout::resolve(&found.path, repo, git_ref, fetcher, progress)?;
        let dir = checkout::checkout_dir(&self.dir, env, repo, &resolved.sha);
        let new = checkout::ensure_checkout(&found.path, repo, &resolved.sha, &dir)?;
        if new {
            progress(&format!(
                "checked out repo `{repo}` at {} ({})",
                resolved.sha, resolved.candidate
            ));
        }
        Ok(Pinned {
            git_ref: git_ref.clone(),
            resolved,
            checkout: dir,
            new,
        })
    }
}

/// The config of repo `repo` at `at`'s checkout (loaded once), which must call itself `repo`.
fn load_at<'a>(
    configs: &'a mut HashMap<PathBuf, RepoConfig>,
    repo: &str,
    at: &Pinned,
) -> Result<&'a RepoConfig, ObeliskK8sError> {
    if !configs.contains_key(&at.checkout) {
        let config = RepoConfig::load(&at.checkout).map_err(|e| {
            ObeliskK8sError::Usage(format!(
                "repo `{repo}` at {} ({}): {e}",
                at.resolved.sha,
                at.checkout.display()
            ))
        })?;
        if config.name != repo {
            return Err(ObeliskK8sError::Usage(format!(
                "repo `{repo}` at {} calls itself `{}` in its repo.yaml",
                at.resolved.sha, config.name
            )));
        }
        configs.insert(at.checkout.clone(), config);
    }
    Ok(&configs[&at.checkout])
}

fn known<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

impl EnvPlan {
    /// Every checkout it pins (the blueprint repo's, and each unit's).
    #[must_use]
    pub fn checkouts(&self) -> HashSet<PathBuf> {
        std::iter::once(&self.blueprint_at)
            .chain(self.units.iter().map(|u| &u.at))
            .map(|at| at.checkout.clone())
            .collect()
    }
}

/// What `env show` prints.
impl fmt::Display for EnvPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env = &self.env;
        writeln!(f, "env: {}  ({})", env.name, env.file.display())?;
        writeln!(
            f,
            "blueprint: {}/{}{}",
            env.repo,
            self.blueprint.name,
            if self.blueprint.ephemeral {
                "  (ephemeral)"
            } else {
                ""
            }
        )?;
        write_pinned(f, "  ", &self.blueprint_at)?;
        if let Some(copy) = &env.ephemeral {
            writeln!(f, "ephemeral: {copy}")?;
        }
        writeln!(
            f,
            "cluster: {}",
            self.cluster.as_deref().unwrap_or("(none)")
        )?;
        writeln!(
            f,
            "\nunits:{}",
            if self.units.is_empty() { " (none)" } else { "" }
        )?;
        for unit in &self.units {
            writeln!(f, "  {}", unit.id)?;
            write_pinned(f, "    ", &unit.at)?;
            for dep in &unit.depends_on {
                writeln!(f, "    after {dep}")?;
            }
        }
        Ok(())
    }
}

fn write_pinned(f: &mut fmt::Formatter<'_>, indent: &str, at: &Pinned) -> fmt::Result {
    writeln!(
        f,
        "{indent}ref: {} → {} ({})",
        at.git_ref, at.resolved.candidate, at.resolved.sha
    )?;
    writeln!(f, "{indent}checkout: {}", at.checkout.display())
}

// The file, as written.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvFile {
    env: EnvSection,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvSection {
    repo: Spanned<String>,
    blueprint: Spanned<String>,
    #[serde(rename = "ref")]
    git_ref: Spanned<String>,
    ephemeral: Option<Spanned<String>>,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::checkout::Fetcher;
    use crate::checkout::tests::{commit, run, upstream_and_bare};
    use crate::config::global::GlobalConfig;
    use crate::config::repo::RepoConfig;

    const TARGET: &str = "target:\n  cluster: c\n  ephemeral: true\n  release: { name: \"${{obelisk:unit}}\", namespace: ns, version: \"1.0.0\" }\n";

    /// A global root on cluster `c` listing `repos`, with `envs`.
    fn global(dir: &Path, repos: &[&Path], envs: &[(&str, &str)]) -> GlobalConfig {
        let root = dir.join("global");
        for cluster in ["c", "c2"] {
            std::fs::create_dir_all(root.join("clusters").join(cluster)).unwrap();
            std::fs::write(
                root.join("clusters").join(cluster).join("cluster.yaml"),
                "cluster:\n  k8s: k3d\n  location: local\n  managed: true\n",
            )
            .unwrap();
        }
        let listed: Vec<String> = repos
            .iter()
            .map(|r| format!("  - {}\n", r.display()))
            .collect();
        std::fs::write(
            root.join("repos.yaml"),
            format!("repos:\n{}", listed.concat()),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("envs")).unwrap();
        for (name, text) in envs {
            std::fs::write(root.join("envs").join(format!("{name}.yaml")), text).unwrap();
        }
        GlobalConfig::load(&root, None).unwrap()
    }

    /// An app repo `app` (units `a` and `b` on ephemeral target `local`) and a blueprint repo `bps`
    /// (blueprint `bp`: `b` after `a`, `a` pinned to `main`, `b` by the default `feature|origin/main`),
    /// each upstream with a bare clone.
    fn fixture(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let unit = "unit:\n  build: ./build.yaml\n";
        let (_, app) = upstream_and_bare(
            &dir.join("app"),
            &[
                (".obelisk-k8s/repo.yaml", "repo:\n  name: app\n"),
                (".obelisk-k8s/targets/local/target.yaml", TARGET),
                (".obelisk-k8s/units/a/unit.yaml", unit),
                (".obelisk-k8s/units/b/unit.yaml", unit),
                // For what doesn't fit: a target that isn't ephemeral, one on another cluster,
                // one on a cluster that isn't there, and a unit only on `plain`.
                (
                    ".obelisk-k8s/targets/plain/target.yaml",
                    "target:\n  cluster: c\n",
                ),
                (
                    ".obelisk-k8s/targets/far/target.yaml",
                    "target:\n  cluster: c2\n",
                ),
                (
                    ".obelisk-k8s/targets/lost/target.yaml",
                    "target:\n  cluster: nope\n",
                ),
                (
                    ".obelisk-k8s/units/c/unit.yaml",
                    "unit:\n  build: ./build.yaml\n  targets: [plain]\n",
                ),
                // Every unit renders one ConfigMap, named after it. On `same`, every unit is the
                // one release `x`.
                (
                    ".obelisk-k8s/build.yaml",
                    "build:\n  phases:\n    - name: out\n      manifests: [{ type: gotmpl, path: ./cm.yaml }]\n",
                ),
                (
                    ".obelisk-k8s/cm.yaml",
                    "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ .Values.obelisk.unit }}\ndata:\n  env: {{ .Values.obelisk.env.name | quote }}\n",
                ),
                (
                    ".obelisk-k8s/targets/same/target.yaml",
                    "target:\n  cluster: c\n  ephemeral: true\n  release: { name: x, namespace: ns, version: \"1.0.0\" }\n",
                ),
            ],
        );
        let (_, bps) = upstream_and_bare(
            &dir.join("bps"),
            &[
                (".obelisk-k8s/repo.yaml", "repo:\n  name: bps\n"),
                (
                    ".obelisk-k8s/blueprints/bp/blueprint.yaml",
                    "blueprint:\n  ephemeral: true\n  defaults: { ref: feature|origin/main }\n  units:\n    - { repo: app, target: local, unit: b, dependsOn: [app/local/a] }\n    - { repo: app, target: local, unit: a, ref: main }\n",
                ),
                (
                    ".obelisk-k8s/blueprints/off-target/blueprint.yaml",
                    "blueprint:\n  ephemeral: true\n  defaults: { ref: main }\n  units: [{ repo: app, target: local, unit: c }]\n",
                ),
                (
                    ".obelisk-k8s/blueprints/not-ephemeral/blueprint.yaml",
                    "blueprint:\n  defaults: { ref: main }\n  units: [{ repo: app, target: local, unit: a }]\n",
                ),
                (
                    ".obelisk-k8s/blueprints/two-clusters/blueprint.yaml",
                    "blueprint:\n  defaults: { ref: main }\n  units: [{ repo: app, target: plain, unit: a }, { repo: app, target: far, unit: a }]\n",
                ),
                (
                    ".obelisk-k8s/blueprints/lost/blueprint.yaml",
                    "blueprint:\n  defaults: { ref: main }\n  units: [{ repo: app, target: lost, unit: a }]\n",
                ),
                (
                    ".obelisk-k8s/blueprints/no-target/blueprint.yaml",
                    "blueprint:\n  defaults: { ref: main }\n  units: [{ repo: app, target: gone, unit: a }]\n",
                ),
                (
                    ".obelisk-k8s/blueprints/dup/blueprint.yaml",
                    "blueprint:\n  ephemeral: true\n  defaults: { ref: main }\n  units: [{ repo: app, target: same, unit: a }, { repo: app, target: same, unit: b }]\n",
                ),
                (
                    ".obelisk-k8s/blueprints/no-unit/blueprint.yaml",
                    "blueprint:\n  defaults: { ref: main }\n  units: [{ repo: app, target: plain, unit: gone }]\n",
                ),
            ],
        );
        (app, bps)
    }

    #[test]
    fn resolves_pins_and_checks_out_an_env() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let (app, bps) = fixture(d);
        let global = global(
            d,
            &[&app, &bps],
            &[(
                "e1",
                "env:\n  repo: bps\n  blueprint: bp\n  ref: origin/HEAD|main\n  ephemeral: e1\n",
            )],
        );
        assert!(
            global.repos.iter().all(|r| r.from_git),
            "{:?}",
            global.repos
        );
        let sha = run(&app, &["rev-parse", "main"]);

        let mut steps = Vec::new();
        let plan = global
            .resolve_env("e1", &mut Fetcher::new(true), &mut |s| {
                steps.push(s.to_owned());
            })
            .unwrap();
        assert_eq!(plan.cluster.as_deref(), Some("c"));
        let ids: Vec<String> = plan.units.iter().map(|u| u.id.to_string()).collect();
        assert_eq!(ids, ["app/local/a", "app/local/b"]);
        assert_eq!(plan.units[0].at.resolved.candidate, "main");
        assert_eq!(plan.units[1].at.resolved.candidate, "origin/main");
        assert_eq!(plan.units[0].at.resolved.sha, sha);
        // Both units are on the same commit: one checkout, made once.
        assert_eq!(plan.units[0].at.checkout, plan.units[1].at.checkout);
        assert!(plan.units[0].at.new);
        assert!(!plan.units[1].at.new);
        assert_eq!(
            plan.units[0].at.checkout,
            global.dir.join("envs/target/e1/app").join(&sha)
        );
        assert!(
            plan.units[0]
                .at
                .checkout
                .join(".obelisk-k8s/units/a/unit.yaml")
                .is_file()
        );
        // One fetch per repo, however many chains reach a remote candidate.
        let fetches: Vec<&String> = steps.iter().filter(|s| s.starts_with("fetching")).collect();
        assert_eq!(
            fetches,
            ["fetching repo `bps`...", "fetching repo `app`..."]
        );

        // Again: everything's already checked out.
        let again = global
            .resolve_env("e1", &mut Fetcher::new(false), &mut |_| {})
            .unwrap();
        assert!(!again.blueprint_at.new && again.units.iter().all(|u| !u.at.new));
        // The checkouts don't make `envs/` look like it has more envs.
        let reloaded = GlobalConfig::load(&global.dir, None).unwrap();
        assert_eq!(reloaded.envs.len(), 1);
    }

    #[test]
    fn each_env_has_its_own_checkouts_and_only_drops_them_on_purpose() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let (app, bps) = fixture(d);
        let env = "env:\n  repo: bps\n  blueprint: bp\n  ref: main\n  ephemeral: x\n";
        let global = global(d, &[&app, &bps], &[("e1", env), ("e2", env)]);
        let resolve = |name: &str| {
            global
                .resolve_env(name, &mut Fetcher::new(true), &mut |_| {})
                .unwrap()
        };
        let old = resolve("e1").units[0].at.checkout.clone();
        let e2 = resolve("e2").units[0].at.checkout.clone();
        assert_ne!(old, e2, "envs don't share checkouts");
        assert!(old.starts_with(global.dir.join("envs/target/e1/app")));

        // `app` moves on, upstream and in the bare repo's own `main`: e1 drops the old commit.
        commit(&d.join("app/upstream"), &[("x", "1")], "two");
        run(&app, &["fetch", "-q", "origin", "main:main"]);
        let moved = resolve("e1");
        assert_ne!(moved.units[0].at.checkout, old);
        // Resolving (`env show` / `template` / `diff`) never removes one.
        assert!(old.is_dir());
    }

    #[test]
    fn a_bare_repo_loads_its_whole_config_at_origin_head() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let (_, bps) = fixture(d);
        // Upstream renames the repo and adds a blueprint; the bare repo fetches it, but its own
        // `main` (its HEAD) stays where it was.
        let upstream = d.join("bps/upstream");
        commit(
            &upstream,
            &[
                (".obelisk-k8s/repo.yaml", "repo:\n  name: bps2\n"),
                (
                    ".obelisk-k8s/blueprints/more/blueprint.yaml",
                    "blueprint:\n  defaults: { ref: main }\n",
                ),
            ],
            "two",
        );
        run(&bps, &["fetch", "-q", "origin"]);
        run(&bps, &["remote", "set-head", "origin", "main"]);
        let (config, rev) = RepoConfig::load_from_git(&bps).unwrap();
        assert_eq!(rev, "origin/HEAD");
        assert_eq!(config.name, "bps2");
        let names: Vec<&str> = config.blueprints.iter().map(|b| b.name.as_str()).collect();
        assert!(
            names.contains(&"bp") && names.contains(&"more"),
            "{names:?}"
        );
        // In the registry too: targets, units and blueprints, not just a name.
        let registry = global(d, &[&bps], &[]);
        let [repo] = &registry.repos[..] else {
            panic!("{:?}", registry.repos)
        };
        assert!(repo.from_git);
        assert_eq!(repo.name, "bps2");
        assert!(
            repo.blueprints.contains(&"more".to_owned()),
            "{:?}",
            repo.blueprints
        );

        // An env pinning a commit where the repo had another name is refused.
        let old = "env:\n  repo: bps2\n  blueprint: bp\n  ref: HEAD\n  ephemeral: x\n";
        let with_old = global(d, &[&bps], &[("old", old)]);
        let error = with_old
            .resolve_env("old", &mut Fetcher::new(false), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(error.contains("calls itself `bps`"), "{error}");

        // Without `origin/HEAD`, it's read at HEAD (the old name).
        run(&bps, &["remote", "set-head", "origin", "-d"]);
        let (config, rev) = RepoConfig::load_from_git(&bps).unwrap();
        assert_eq!((rev, config.name.as_str()), ("HEAD", "bps"));
    }

    #[test]
    fn a_checkout_of_a_branch_without_the_config_still_loads_from_git() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fixture(d);
        // A normal clone of `app`, checked out on a branch that has no `.obelisk-k8s/`.
        let work = d.join("work");
        run(
            d,
            &[
                "clone",
                "-q",
                d.join("app/upstream").to_str().unwrap(),
                "work",
            ],
        );
        run(&work, &["checkout", "-q", "--orphan", "old"]);
        run(&work, &["rm", "-rq", "--cached", "."]);
        std::fs::remove_dir_all(work.join(".obelisk-k8s")).unwrap();
        commit(&work, &[("README", "old\n")], "old");
        assert!(!work.join(".obelisk-k8s").exists());

        let global = global(d, &[&work], &[]);
        let [repo] = &global.repos[..] else {
            panic!("{:?} {:?}", global.repos, global.warnings)
        };
        assert_eq!(repo.name, "app");
        assert!(repo.from_git);
        assert!(repo.units.contains(&"a".to_owned()), "{:?}", repo.units);
    }

    #[test]
    fn a_broken_env_file_is_a_warning_until_it_is_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let (app, bps) = fixture(d);
        let good = "env:\n  repo: bps\n  blueprint: bp\n  ref: main\n  ephemeral: x\n";
        let global = global(
            d,
            &[&app, &bps],
            &[("good", good), ("bad", "env:\n  repo: bps\n  typo: 1\n")],
        );
        let names: Vec<&str> = global.envs.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["good"]);
        assert!(
            global
                .warnings
                .iter()
                .any(|w| w.contains("envs/bad.yaml: skipped")),
            "{:?}",
            global.warnings
        );
        let error = global.env("bad").unwrap_err().to_string();
        assert!(error.contains("typo"), "{error}");
        assert!(
            global
                .env("nope")
                .unwrap_err()
                .to_string()
                .starts_with("no env `nope`")
        );
    }

    #[test]
    fn a_local_branch_wins_over_the_remote() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let (app, bps) = fixture(d);
        // A local commit on `feature`, not pushed anywhere.
        let work = d.join("work");
        run(d, &["clone", "-q", app.to_str().unwrap(), "work"]);
        let feature = commit(&work, &[("x", "1")], "feature");
        run(&work, &["push", "-q", "origin", "HEAD:refs/heads/feature"]);
        let global = global(
            d,
            &[&app, &bps],
            &[(
                "e1",
                "env:\n  repo: bps\n  blueprint: bp\n  ref: main\n  ephemeral: e1\n",
            )],
        );
        let plan = global
            .resolve_env("e1", &mut Fetcher::new(true), &mut |_| {})
            .unwrap();
        let b = plan.units.iter().find(|u| u.id.unit == "b").unwrap();
        assert_eq!(b.at.resolved.candidate, "feature");
        assert_eq!(b.at.resolved.sha, feature);
    }

    #[test]
    fn says_what_doesnt_fit() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let (app, bps) = fixture(d);
        let cases = [
            (
                "env:\n  repo: bps\n  blueprint: bp\n  ref: main\n",
                "needs `ephemeral: <name>`",
            ),
            (
                "env:\n  repo: bps\n  blueprint: nope\n  ref: main\n  ephemeral: e\n",
                "has no blueprint `nope`",
            ),
            (
                "env:\n  repo: nope\n  blueprint: bp\n  ref: main\n  ephemeral: e\n",
                "no repo `nope`",
            ),
            (
                "env:\n  repo: bps\n  blueprint: bp\n  ref: nope\n  ephemeral: e\n",
                "no commit for ref `nope`",
            ),
            (
                "env:\n  repo: bps\n  blueprint: not-ephemeral\n  ref: main\n  ephemeral: e\n",
                "isn't ephemeral, so the env can't set `ephemeral`",
            ),
            (
                "env:\n  repo: bps\n  blueprint: off-target\n  ref: main\n  ephemeral: e\n",
                "unit `c` doesn't deploy to target `local`",
            ),
            (
                "env:\n  repo: bps\n  blueprint: not-ephemeral\n  ref: main\n",
                "target `local` is ephemeral, but the blueprint isn't",
            ),
            (
                "env:\n  repo: bps\n  blueprint: two-clusters\n  ref: main\n",
                "a blueprint deploys to one cluster",
            ),
            (
                "env:\n  repo: bps\n  blueprint: lost\n  ref: main\n",
                "no cluster `nope`",
            ),
            (
                "env:\n  repo: bps\n  blueprint: no-target\n  ref: main\n",
                "no target `gone`",
            ),
            (
                "env:\n  repo: bps\n  blueprint: no-unit\n  ref: main\n",
                "no unit `gone`",
            ),
        ];
        for (text, expected) in cases {
            let global = global(d, &[&app, &bps], &[("e", text)]);
            let error = global
                .resolve_env("e", &mut Fetcher::new(false), &mut |_| {})
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{text}\n→ {error}");
        }
    }
}
