//! Which config files a unit gets on a target, and in what order they merge.
//!
//! Every config path has a scope: the vars it uses, plus the one implied by the file that declares it
//! (anything in a `target.yaml` is target-specific, anything in a `unit.yaml` unit-specific). Paths merge
//! by scope, lowest priority first:
//!
//! | scope         | e.g.                                                               |
//! |---------------|--------------------------------------------------------------------|
//! | repo          | `repo.yaml`, no vars                                               |
//! | target        | `repo.yaml` with `target`; `target.yaml` with no vars              |
//! | unit          | `repo.yaml` with `unit`; `unit.yaml` with no vars                  |
//! | target + unit | `repo.yaml` with both; `target.yaml` with `unit`; `unit.yaml` with `target` |
//!
//! Within a scope, `repo.yaml` comes before `target.yaml` before `unit.yaml`; within a file, the order
//! written. A path that doesn't exist is skipped.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::ObeliskK8sError;
use crate::config::repo::{REPO_CONFIG_DIR, REPO_FILE, RepoConfig, Target, Unit};
use anstyle::{AnsiColor, Style};

use crate::interp::{InterpString, InterpolationParams, StringPart, Var};

/// Which file declared a config path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Repo,
    Target,
    Unit,
}

/// How specific a config path is. Later scopes win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Repo,
    Target,
    Unit,
    TargetUnit,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Repo => "repo",
            Self::Target => "target",
            Self::Unit => "unit",
            Self::TargetUnit => "target + unit",
        })
    }
}

/// One config path, resolved for a target and unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath<'a> {
    /// As written.
    pub written: &'a InterpString,
    /// The file that declares it, relative to the repo root.
    pub declared_in: &'a Path,
    pub scope: Scope,
    /// Relative to the repo root, e.g. `.obelisk-k8s/config/app/values.yaml`.
    pub path: PathBuf,
    /// Whether it's an existing file (if not, it's skipped).
    pub found: bool,
}

impl RepoConfig {
    /// Every config path for `unit` on `target`, in merge order (lowest priority first), including the
    /// ones that don't exist.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if there's no such target or unit, or the unit doesn't deploy to the
    /// target; [`ObeliskK8sError::Config`] if a path uses a var with no value here.
    pub fn config_paths_for<'a>(
        &'a self,
        repo_root: &Path,
        target: &str,
        unit: &str,
    ) -> Result<Vec<ResolvedPath<'a>>, ObeliskK8sError> {
        let selection = self.select(&[target.to_owned()], &[unit.to_owned()])?;
        let (target, unit) = selection.cases[0];
        self.resolve(repo_root, target, unit)
    }

    /// The (target, unit) cases a `config repo paths` selection covers. No names means all of them.
    ///
    /// Exactly one target and one unit (asked for, or the only ones) is a single case, which must deploy.
    /// Otherwise it's
    /// every selected target × every selected unit (targets first, each in the order asked for, else
    /// the repo's), leaving out the pairs where the unit doesn't deploy to the target.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] for an unknown name, a single case that doesn't deploy, or a selection
    /// with no cases at all.
    pub fn select(
        &self,
        targets: &[String],
        units: &[String],
    ) -> Result<Selection<'_>, ObeliskK8sError> {
        let targets = pick(targets, &self.targets, |n| self.target(n))?;
        let units = pick(units, &self.units, |n| self.unit(n))?;
        if let ([target], [unit]) = (targets.as_slice(), units.as_slice())
            && !self.deploys(target, unit)
        {
            return Err(ObeliskK8sError::Usage(format!(
                "unit `{}` doesn't deploy to target `{}` (it deploys to: {})",
                unit.name,
                target.name,
                or_none(&self.unit_target_names(unit))
            )));
        }
        let single = targets.len() == 1 && units.len() == 1;
        let cases: Vec<_> = targets
            .iter()
            .flat_map(|t| units.iter().map(move |u| (*t, *u)))
            .filter(|(t, u)| self.deploys(t, u))
            .collect();
        if cases.is_empty() {
            return Err(ObeliskK8sError::Usage(
                "nothing to show: none of the selected units deploy to the selected targets"
                    .to_owned(),
            ));
        }
        Ok(Selection { cases, single })
    }

    /// `config repo paths`: the config paths of every case in `selection`.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Config`] if a path uses a var with no value here.
    pub fn paths_report<'a>(
        &'a self,
        repo_root: &Path,
        selection: &Selection<'a>,
        explain: bool,
    ) -> Result<PathsReport<'a>, ObeliskK8sError> {
        let cases = selection
            .cases
            .iter()
            .map(|(target, unit)| {
                Ok(Case {
                    target: &target.name,
                    unit: &unit.name,
                    paths: self.resolve(repo_root, target, unit)?,
                })
            })
            .collect::<Result<_, ObeliskK8sError>>()?;
        Ok(PathsReport {
            cases,
            single: selection.single,
            explain,
            shown_root: None,
        })
    }

    fn deploys(&self, target: &Target, unit: &Unit) -> bool {
        self.unit_target_names(unit).contains(&target.name.as_str())
    }

    fn resolve<'a>(
        &'a self,
        repo_root: &Path,
        target: &'a Target,
        unit: &'a Unit,
    ) -> Result<Vec<ResolvedPath<'a>>, ObeliskK8sError> {
        let params = InterpolationParams {
            target: Some(&target.name),
            unit: Some(&unit.name),
        };
        let declared = [
            (Level::Repo, Path::new(REPO_FILE), &self.config_paths),
            (Level::Target, target.file.as_path(), &target.config_paths),
            (Level::Unit, unit.file.as_path(), &unit.config_paths),
        ];

        let mut resolved = Vec::new();
        for (level, declared_in, paths) in declared {
            for written in paths {
                let interpolated =
                    written
                        .interpolate(&params)
                        .map_err(|var| ObeliskK8sError::Config {
                            file: declared_in.to_owned(),
                            at: None,
                            message: format!(
                                "`{written}`: `${{{{obelisk:{}}}}}` has no value here",
                                var.name()
                            ),
                        })?;
                // Every config path starts with `./` (checked at load), relative to `.obelisk-k8s/`.
                let relative = interpolated.strip_prefix("./").unwrap_or(&interpolated);
                let path = Path::new(REPO_CONFIG_DIR).join(relative);
                resolved.push(ResolvedPath {
                    found: repo_root.join(&path).is_file(),
                    written,
                    declared_in,
                    scope: scope(level, written),
                    path,
                });
            }
        }
        // Stable: `declared` is already in level order, and each file in the order written.
        resolved.sort_by_key(|r| r.scope);
        Ok(resolved)
    }

    fn target(&self, name: &str) -> Result<&Target, ObeliskK8sError> {
        self.targets.iter().find(|t| t.name == name).ok_or_else(|| {
            let known: Vec<_> = self.targets.iter().map(|t| t.name.as_str()).collect();
            ObeliskK8sError::Usage(format!(
                "no target `{name}` (the repo has: {})",
                or_none(&known)
            ))
        })
    }

    fn unit(&self, name: &str) -> Result<&Unit, ObeliskK8sError> {
        self.units.iter().find(|u| u.name == name).ok_or_else(|| {
            let known: Vec<_> = self.units.iter().map(|u| u.name.as_str()).collect();
            ObeliskK8sError::Usage(format!(
                "no unit `{name}` (the repo has: {})",
                or_none(&known)
            ))
        })
    }
}

fn scope(level: Level, path: &InterpString) -> Scope {
    let vars = path.vars();
    let target = level == Level::Target || vars.contains(&Var::Target);
    let unit = level == Level::Unit || vars.contains(&Var::Unit);
    match (target, unit) {
        (false, false) => Scope::Repo,
        (true, false) => Scope::Target,
        (false, true) => Scope::Unit,
        (true, true) => Scope::TargetUnit,
    }
}

/// The named items, deduplicated, in the order asked for; or all of them if none are named.
fn pick<'a, T>(
    names: &[String],
    all: &'a [T],
    find: impl Fn(&str) -> Result<&'a T, ObeliskK8sError>,
) -> Result<Vec<&'a T>, ObeliskK8sError> {
    if names.is_empty() {
        return Ok(all.iter().collect());
    }
    let mut picked: Vec<&T> = Vec::new();
    for name in names {
        let item = find(name)?;
        if !picked.iter().any(|p| std::ptr::eq(*p, item)) {
            picked.push(item);
        }
    }
    Ok(picked)
}

/// Which (target, unit) cases to show; see [`RepoConfig::select`].
#[derive(Debug)]
pub struct Selection<'a> {
    pub cases: Vec<(&'a Target, &'a Unit)>,
    /// Exactly one target and one unit: shown without a case header.
    pub single: bool,
}

/// One (target, unit) case's config paths.
#[derive(Debug)]
pub struct Case<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    pub paths: Vec<ResolvedPath<'a>>,
}

/// What `config repo paths` prints. For a single case: just its found paths, or with `explain`, its
/// table. For several: each under a magenta `[ target=…, unit=… ]` header, a blank line between.
#[derive(Debug)]
pub struct PathsReport<'a> {
    pub cases: Vec<Case<'a>>,
    pub single: bool,
    pub explain: bool,
    /// Where the repo is, as the user should see it, when that isn't the current directory: paths are
    /// shown under it. `None` shows them relative to the repo root (`.obelisk-k8s/…`).
    pub shown_root: Option<PathBuf>,
}

const CASE_HEADER: Style = AnsiColor::Magenta.on_default().bold();

impl fmt::Display for PathsReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, case) in self.cases.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            let explain = Explain {
                target: case.target,
                unit: case.unit,
                paths: &case.paths,
                shown_root: self.shown_root.as_deref(),
            };
            if !self.single {
                writeln!(
                    f,
                    "{CASE_HEADER}[ target={}, unit={} ]{CASE_HEADER:#}",
                    case.target, case.unit
                )?;
            }
            match (self.explain, self.single) {
                (true, true) => write!(f, "{explain}")?,
                (true, false) => explain.write_table(f)?,
                (false, _) => {
                    for path in case.paths.iter().filter(|p| p.found) {
                        writeln!(
                            f,
                            "{}",
                            shown(self.shown_root.as_deref(), &path.path).display()
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// `path` (relative to the repo root) as the user should see it.
fn shown(shown_root: Option<&Path>, path: &Path) -> PathBuf {
    shown_root.map_or_else(|| path.to_owned(), |root| root.join(path))
}

fn or_none(names: &[&str]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// `config repo paths --explain`: every config path in merge order, as a table of the path as written,
/// as evaluated, and whether it was found. Vars and their values are highlighted; print it through
/// `anstream`, which drops the colors when they aren't wanted.
pub struct Explain<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    pub paths: &'a [ResolvedPath<'a>],
    /// As [`PathsReport::shown_root`].
    pub shown_root: Option<&'a Path>,
}

const HEADER: Style = Style::new().bold().underline();
const VAR: Style = AnsiColor::Cyan.on_default();
const FOUND: Style = AnsiColor::Green.on_default().bold();
const SKIPPED: Style = AnsiColor::Yellow.on_default();

/// A cell's text, plain (to measure) and styled (to print).
struct Cell {
    plain: String,
    styled: String,
}

impl Cell {
    fn styled(style: Style, text: &str) -> Self {
        Self {
            plain: text.to_owned(),
            styled: format!("{style}{text}{style:#}"),
        }
    }

    /// `path`'s parts, with each var rendered by `var` and highlighted.
    fn from_parts(path: &InterpString, var: impl Fn(Var) -> String) -> Self {
        let mut cell = Self {
            plain: String::new(),
            styled: String::new(),
        };
        for part in path.parts() {
            match part {
                StringPart::Text(text) => {
                    cell.plain.push_str(text);
                    cell.styled.push_str(text);
                }
                StringPart::Var(v) => {
                    let value = var(*v);
                    cell.plain.push_str(&value);
                    // Writing to a String can't fail.
                    let _ = write!(cell.styled, "{VAR}{value}{VAR:#}");
                }
            }
        }
        cell
    }

    fn write(&self, f: &mut fmt::Formatter<'_>, width: usize) -> fmt::Result {
        let pad = width.saturating_sub(self.plain.chars().count());
        write!(f, "{}{:pad$}", self.styled, "")
    }
}

impl Explain<'_> {
    fn raw(path: &ResolvedPath<'_>) -> Cell {
        let cell = Cell::from_parts(path.written, |v| format!("${{{{obelisk:{}}}}}", v.name()));
        if cell.plain == path.written.raw() {
            cell
        } else {
            // An escaped `$${{`: rebuilding from the parts would hide the escape, so show it as written.
            let raw = path.written.raw().to_owned();
            Cell {
                styled: raw.clone(),
                plain: raw,
            }
        }
    }

    fn evaluated(&self, path: &ResolvedPath<'_>) -> Cell {
        let mut cell = Cell::from_parts(path.written, |v| {
            match v {
                Var::Target => self.target,
                Var::Unit => self.unit,
            }
            .to_owned()
        });
        // Show it as `path` is shown: under the repo, without the `./` (every config path starts with one).
        let base = shown(self.shown_root, Path::new(REPO_CONFIG_DIR));
        for text in [&mut cell.plain, &mut cell.styled] {
            let rest = text.strip_prefix("./").unwrap_or(text);
            *text = format!("{}/{rest}", base.display());
        }
        cell
    }
}

impl fmt::Display for Explain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (target, unit) = (self.target, self.unit);
        writeln!(
            f,
            "config paths for unit `{unit}` on target `{target}`, lowest priority first:\n"
        )?;
        self.write_table(f)
    }
}

impl Explain<'_> {
    /// The table alone, without the title line.
    fn write_table(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers = ["RAW", "EVALUATED", "STATUS"].map(|h| Cell::styled(HEADER, h));
        let rows: Vec<[Cell; 3]> = self
            .paths
            .iter()
            .map(|p| {
                let status = if p.found {
                    Cell::styled(FOUND, "found")
                } else {
                    Cell::styled(SKIPPED, "skipped")
                };
                [Self::raw(p), self.evaluated(p), status]
            })
            .collect();
        let width = |col: usize| {
            rows.iter()
                .chain([&headers])
                .map(|row| row[col].plain.chars().count())
                .max()
                .unwrap_or(0)
        };
        let widths = [width(0), width(1)];

        for row in [&headers].into_iter().chain(&rows) {
            write!(f, "  ")?;
            row[0].write(f, widths[0])?;
            write!(f, "  ")?;
            row[1].write(f, widths[1])?;
            write!(f, "  ")?;
            row[2].write(f, 0)?;
            writeln!(f)?;
        }
        if rows.is_empty() {
            writeln!(f, "  (none)")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = dir.path().join(REPO_CONFIG_DIR).join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    /// Every combination of declaring file and vars.
    fn full_repo() -> tempfile::TempDir {
        repo(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths:\n    - ./r/t-u/${{obelisk:target}}-${{obelisk:unit}}.yaml\n    - ./r/u/${{obelisk:unit}}.yaml\n    - ./r/t/${{obelisk:target}}.yaml\n    - ./r/plain.yaml\n",
            ),
            (
                "targets/prod/target.yaml",
                "target:\n  cluster: c\n  configPaths:\n    - ./t/u/${{obelisk:unit}}.yaml\n    - ./t/plain.yaml\n",
            ),
            (
                "units/app/unit.yaml",
                "unit:\n  configPaths:\n    - ./u/t/${{obelisk:target}}.yaml\n    - ./u/plain.yaml\n",
            ),
            ("r/plain.yaml", "a: 1\n"),
            ("u/t/prod.yaml", "a: 2\n"),
        ])
    }

    #[test]
    fn merges_by_scope_then_file_then_written_order() {
        let dir = full_repo();
        let config = RepoConfig::load(dir.path()).unwrap();
        let paths = config.config_paths_for(dir.path(), "prod", "app").unwrap();
        let order: Vec<_> = paths
            .iter()
            .map(|p| {
                (
                    p.scope,
                    p.path.to_str().unwrap().trim_start_matches(".obelisk-k8s/"),
                )
            })
            .collect();
        assert_eq!(
            order,
            [
                (Scope::Repo, "r/plain.yaml"),
                (Scope::Target, "r/t/prod.yaml"),
                (Scope::Target, "t/plain.yaml"),
                (Scope::Unit, "r/u/app.yaml"),
                (Scope::Unit, "u/plain.yaml"),
                (Scope::TargetUnit, "r/t-u/prod-app.yaml"),
                (Scope::TargetUnit, "t/u/app.yaml"),
                (Scope::TargetUnit, "u/t/prod.yaml"),
            ]
        );
        let found: Vec<_> = paths
            .iter()
            .filter(|p| p.found)
            .map(|p| p.path.clone())
            .collect();
        assert_eq!(
            found,
            [
                PathBuf::from(".obelisk-k8s/r/plain.yaml"),
                PathBuf::from(".obelisk-k8s/u/t/prod.yaml")
            ]
        );
        assert_eq!(paths[5].declared_in, Path::new(".obelisk-k8s/repo.yaml"));
        assert_eq!(
            paths[7].declared_in,
            Path::new(".obelisk-k8s/units/app/unit.yaml")
        );
        assert_eq!(paths[7].written.raw(), "./u/t/${{obelisk:target}}.yaml");
    }

    #[test]
    fn explains_as_a_table() {
        let dir = full_repo();
        let config = RepoConfig::load(dir.path()).unwrap();
        let paths = config.config_paths_for(dir.path(), "prod", "app").unwrap();
        let styled = Explain {
            target: "prod",
            unit: "app",
            paths: &paths[..3],
            shown_root: None,
        }
        .to_string();
        assert_eq!(
            anstream::adapter::strip_str(&styled).to_string(),
            "config paths for unit `app` on target `prod`, lowest priority first:\n\n\
             \x20 RAW                             EVALUATED                   STATUS\n\
             \x20 ./r/plain.yaml                  .obelisk-k8s/r/plain.yaml   found\n\
             \x20 ./r/t/${{obelisk:target}}.yaml  .obelisk-k8s/r/t/prod.yaml  skipped\n\
             \x20 ./t/plain.yaml                  .obelisk-k8s/t/plain.yaml   skipped\n"
        );
        // Vars and their values are highlighted, and the statuses colored.
        assert!(
            styled.contains(&format!("{VAR}${{{{obelisk:target}}}}{VAR:#}")),
            "{styled}"
        );
        assert!(styled.contains(&format!("{VAR}prod{VAR:#}")), "{styled}");
        assert!(
            styled.contains(&format!("{FOUND}found{FOUND:#}")),
            "{styled}"
        );

        let none = Explain {
            target: "prod",
            unit: "app",
            paths: &[],
            shown_root: None,
        }
        .to_string();
        assert!(
            anstream::adapter::strip_str(&none)
                .to_string()
                .ends_with("STATUS\n  (none)\n"),
            "{none}"
        );
    }

    #[test]
    fn explain_shows_an_escape_as_written() {
        let dir = repo(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [\"./a$${{b}}.yaml\"]\n",
            ),
            ("targets/t/target.yaml", "target:\n  cluster: c\n"),
            ("units/u/unit.yaml", "unit: {}\n"),
        ]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let paths = config.config_paths_for(dir.path(), "t", "u").unwrap();
        let text = Explain {
            target: "t",
            unit: "u",
            paths: &paths,
            shown_root: None,
        }
        .to_string();
        let text = anstream::adapter::strip_str(&text).to_string();
        assert!(
            text.contains("./a$${{b}}.yaml  .obelisk-k8s/a${{b}}.yaml"),
            "{text}"
        );
    }

    #[test]
    fn refuses_unknown_or_undeployed_names() {
        let dir = repo(&[
            ("repo.yaml", "repo:\n  name: r\n"),
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("targets/dev/target.yaml", "target:\n  cluster: c\n"),
            ("units/app/unit.yaml", "unit:\n  targets: [dev]\n"),
        ]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let error = |t, u| {
            config
                .config_paths_for(dir.path(), t, u)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            error("nope", "app"),
            "no target `nope` (the repo has: dev, prod)"
        );
        assert_eq!(error("prod", "nope"), "no unit `nope` (the repo has: app)");
        assert_eq!(
            error("prod", "app"),
            "unit `app` doesn't deploy to target `prod` (it deploys to: dev)"
        );
    }

    /// t1, t2; u1 deploys everywhere, u2 only to t2.
    fn two_by_two() -> tempfile::TempDir {
        repo(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  configPaths: [\"./${{obelisk:unit}}.yaml\"]\n",
            ),
            ("targets/t1/target.yaml", "target:\n  cluster: c\n"),
            ("targets/t2/target.yaml", "target:\n  cluster: c\n"),
            ("units/u1/unit.yaml", "unit: {}\n"),
            ("units/u2/unit.yaml", "unit:\n  targets: [t2]\n"),
            ("u1.yaml", ""),
        ])
    }

    fn cases(config: &RepoConfig, targets: &[&str], units: &[&str]) -> (Vec<String>, bool) {
        let owned = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        let selection = config.select(&owned(targets), &owned(units)).unwrap();
        let cases = selection
            .cases
            .iter()
            .map(|(t, u)| format!("{}/{}", t.name, u.name))
            .collect();
        (cases, selection.single)
    }

    #[test]
    fn selects_cases() {
        let dir = two_by_two();
        let config = RepoConfig::load(dir.path()).unwrap();
        let c = |t: &[&str], u: &[&str]| cases(&config, t, u);
        // All targets × all units, leaving out u2 on t1 (it doesn't deploy there).
        assert_eq!(
            c(&[], &[]),
            (vec!["t1/u1".into(), "t2/u1".into(), "t2/u2".into()], false)
        );
        assert_eq!(c(&["t1"], &[]), (vec!["t1/u1".into()], false));
        assert_eq!(
            c(&["t2", "t1"], &[]),
            (vec!["t2/u1".into(), "t2/u2".into(), "t1/u1".into()], false)
        );
        assert_eq!(c(&[], &["u2"]), (vec!["t2/u2".into()], false));
        assert_eq!(
            c(&[], &["u1", "u2"]),
            (vec!["t1/u1".into(), "t2/u1".into(), "t2/u2".into()], false)
        );
        assert_eq!(c(&["t2"], &["u2"]), (vec!["t2/u2".into()], true));
        assert_eq!(
            c(&["t1", "t2"], &["u1", "u2"]),
            (vec!["t1/u1".into(), "t2/u1".into(), "t2/u2".into()], false)
        );
        assert_eq!(c(&["t1", "t1"], &["u1"]), (vec!["t1/u1".into()], true));
    }

    #[test]
    fn selection_errors() {
        let dir = two_by_two();
        let config = RepoConfig::load(dir.path()).unwrap();
        let error = |t: &[&str], u: &[&str]| {
            let owned = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
            config.select(&owned(t), &owned(u)).unwrap_err().to_string()
        };
        assert_eq!(
            error(&["t1"], &["u2"]),
            "unit `u2` doesn't deploy to target `t1` (it deploys to: t2)"
        );
        assert_eq!(error(&["t3"], &[]), "no target `t3` (the repo has: t1, t2)");
        assert_eq!(error(&[], &["u3"]), "no unit `u3` (the repo has: u1, u2)");
        assert!(error(&["t1"], &["u2", "u2"]).contains("doesn't deploy"));
    }

    #[test]
    fn reports_one_case_without_a_header() {
        let dir = two_by_two();
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&["t1".into()], &["u1".into()]).unwrap();
        let plain = config
            .paths_report(dir.path(), &selection, false)
            .unwrap()
            .to_string();
        assert_eq!(plain, ".obelisk-k8s/u1.yaml\n");
        let explain = config
            .paths_report(dir.path(), &selection, true)
            .unwrap()
            .to_string();
        assert!(
            explain.starts_with("config paths for unit `u1` on target `t1`"),
            "{explain}"
        );
    }

    #[test]
    fn reports_several_cases_under_headers() {
        let dir = two_by_two();
        let config = RepoConfig::load(dir.path()).unwrap();
        let selection = config.select(&[], &[]).unwrap();
        let strip = |s: String| anstream::adapter::strip_str(&s).to_string();
        let plain = strip(
            config
                .paths_report(dir.path(), &selection, false)
                .unwrap()
                .to_string(),
        );
        assert_eq!(
            plain,
            "[ target=t1, unit=u1 ]\n.obelisk-k8s/u1.yaml\n\n[ target=t2, unit=u1 ]\n.obelisk-k8s/u1.yaml\n\n[ target=t2, unit=u2 ]\n"
        );
        let styled = config
            .paths_report(dir.path(), &selection, true)
            .unwrap()
            .to_string();
        assert!(
            styled.contains(&format!(
                "{CASE_HEADER}[ target=t2, unit=u2 ]{CASE_HEADER:#}"
            )),
            "{styled}"
        );
        let explain = strip(styled);
        assert!(!explain.contains("config paths for"), "{explain}");
        assert!(
            explain.starts_with("[ target=t1, unit=u1 ]\n  RAW"),
            "{explain}"
        );
        assert!(
            explain.contains("found\n\n[ target=t2, unit=u2 ]\n  RAW"),
            "{explain}"
        );
    }

    #[test]
    fn bad_interpolation_fails_the_load_with_a_location() {
        let dir = repo(&[(
            "repo.yaml",
            "repo:\n  name: r\n  configPaths:\n    - ./${{obelisk:untis}}.yaml\n",
        )]);
        let error = RepoConfig::load(dir.path()).unwrap_err().to_string();
        assert!(error.contains(".obelisk-k8s/repo.yaml:4:"), "{error}");
        assert!(error.contains("unknown var `obelisk:untis`"), "{error}");
    }
}
