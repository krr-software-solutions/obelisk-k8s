//! Which (target, unit) cases a command covers, and which build file each case renders (`config repo
//! paths`).
//!
//! A unit's build file is its own `build` (in its `unit.yaml`), else the repo's (in `repo.yaml`),
//! interpolated for the case: `${{obelisk:target}}`, `${{obelisk:unit}}`, and `${{obelisk:path:unit}}`
//! (the unit's directory, `./units/<dir>`). A path that starts with `./` resolves from `.obelisk-k8s/`;
//! an absolute one is used as it is.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::ObeliskK8sError;
use crate::config::repo::{REPO_CONFIG_DIR, RepoConfig, Target, Unit};
use anstyle::{AnsiColor, Style};

use crate::interp::{InterpString, InterpolationParams, StringPart, Var};

/// A case's build file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPath<'a> {
    /// As written.
    pub written: &'a InterpString,
    /// The file that declares it (the unit's `unit.yaml`, or `repo.yaml`), relative to the repo root.
    pub declared_in: &'a Path,
    /// Relative to the repo root (`.obelisk-k8s/units/app/build.yaml.gotmpl`), or absolute.
    pub path: PathBuf,
    /// Whether it's an existing file.
    pub found: bool,
    /// What `${{obelisk:path:unit}}` was (`./units/app`; `None` if the directory isn't UTF-8).
    pub path_unit: Option<String>,
}

impl RepoConfig {
    /// The build file of `unit` on `target`.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Usage`] if there's no such target or unit, or the unit doesn't deploy to the
    /// target; [`ObeliskK8sError::Config`] if its path uses a var with no value here.
    pub fn build_path_for<'a>(
        &'a self,
        repo_root: &Path,
        target: &str,
        unit: &str,
    ) -> Result<BuildPath<'a>, ObeliskK8sError> {
        let selection = self.select(&[target.to_owned()], &[unit.to_owned()])?;
        let (target, unit) = selection.cases[0];
        self.build_path(repo_root, target, unit)
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

    /// `config repo paths`: the build file of every case in `selection`.
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
                    build: self.build_path(repo_root, target, unit)?,
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

    /// `unit`'s build file on `target`.
    pub(crate) fn build_path<'a>(
        &'a self,
        repo_root: &Path,
        target: &Target,
        unit: &'a Unit,
    ) -> Result<BuildPath<'a>, ObeliskK8sError> {
        let (written, declared_in) = self.build_of(unit);
        let path_unit = unit.path();
        let params = InterpolationParams {
            target: Some(&target.name),
            unit: Some(&unit.name),
            path_unit: path_unit.as_deref(),
        };
        let evaluated = written
            .interpolate(&params)
            .map_err(|var| ObeliskK8sError::Config {
                file: declared_in.to_owned(),
                at: None,
                message: format!(
                    "`{written}`: `${{{{obelisk:{}}}}}` has no value here",
                    var.name()
                ),
            })?;
        // Checked at load: `./…` (`${{obelisk:path:unit}}` is one), or absolute.
        let path = from_repo_config(&evaluated);
        Ok(BuildPath {
            found: repo_root.join(&path).is_file(),
            written,
            declared_in,
            path,
            path_unit,
        })
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

/// A path as written in obelisk-k8s's files: `./…` is relative to `.obelisk-k8s/` (and comes back
/// relative to the repo root); anything else (an absolute path) is used as it is.
pub(crate) fn from_repo_config(written: &str) -> PathBuf {
    match written.strip_prefix("./") {
        Some(rest) => Path::new(REPO_CONFIG_DIR).join(rest),
        None => PathBuf::from(written),
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

/// One (target, unit) case's build file.
#[derive(Debug)]
pub struct Case<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    pub build: BuildPath<'a>,
}

/// What `config repo paths` prints. For a single case: just its build file, or with `explain`, its
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

pub(crate) const CASE_HEADER: Style = AnsiColor::Magenta.on_default().bold();

/// Writes several cases' YAML as one stream: each under a magenta `# [ target=…, unit=… ]` comment,
/// with `between` written between them (`single`: just the one case's text).
pub(crate) fn write_yaml_cases<'a>(
    f: &mut fmt::Formatter<'_>,
    single: bool,
    between: &str,
    cases: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
) -> fmt::Result {
    for (i, (target, unit, text)) in cases.into_iter().enumerate() {
        if !single {
            if i > 0 {
                f.write_str(between)?;
            }
            writeln!(
                f,
                "{CASE_HEADER}# [ target={target}, unit={unit} ]{CASE_HEADER:#}"
            )?;
        }
        f.write_str(text)?;
    }
    Ok(())
}

impl fmt::Display for PathsReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, case) in self.cases.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            let explain = Explain {
                target: case.target,
                unit: case.unit,
                build: &case.build,
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
                (false, _) => writeln!(
                    f,
                    "{}",
                    shown(self.shown_root.as_deref(), &case.build.path).display()
                )?,
            }
        }
        Ok(())
    }
}

/// `path` (relative to the repo root, or absolute) as the user should see it.
fn shown(shown_root: Option<&Path>, path: &Path) -> PathBuf {
    crate::config::values::shown_under(shown_root, path)
}

fn or_none(names: &[&str]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// `config repo paths --explain`: a case's build file, as a table of the path as written, the file
/// that declares it, the path as evaluated, and whether it's there. Vars and their values are
/// highlighted; print it through `anstream`, which drops the colors when they aren't wanted.
pub struct Explain<'a> {
    pub target: &'a str,
    pub unit: &'a str,
    pub build: &'a BuildPath<'a>,
    /// As [`PathsReport::shown_root`].
    pub shown_root: Option<&'a Path>,
}

const HEADER: Style = Style::new().bold().underline();
const VAR: Style = AnsiColor::Cyan.on_default();
const FOUND: Style = AnsiColor::Green.on_default().bold();
const MISSING: Style = AnsiColor::Yellow.on_default();

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

    fn plain(text: &str) -> Self {
        Self {
            plain: text.to_owned(),
            styled: text.to_owned(),
        }
    }

    /// `path`'s parts, with each var rendered by `var` and highlighted, and a leading `./` (in the
    /// first part) replaced by `dot` (when given).
    fn from_parts(path: &InterpString, var: impl Fn(Var) -> String, dot: Option<&str>) -> Self {
        let mut cell = Self::plain("");
        for (i, part) in path.parts().iter().enumerate() {
            let (text, is_var) = match part {
                StringPart::Text(text) => (text.clone(), false),
                StringPart::Var(v) => (var(*v), true),
            };
            let text = match (i, dot, text.strip_prefix("./")) {
                (0, Some(dot), Some(rest)) => format!("{dot}/{rest}"),
                _ => text,
            };
            cell.plain.push_str(&text);
            if is_var {
                // Writing to a String can't fail.
                let _ = write!(cell.styled, "{VAR}{text}{VAR:#}");
            } else {
                cell.styled.push_str(&text);
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
    fn raw(&self) -> Cell {
        let written = self.build.written;
        let cell = Cell::from_parts(written, |v| format!("${{{{obelisk:{}}}}}", v.name()), None);
        if cell.plain == written.raw() {
            cell
        } else {
            // An escaped `$${{`: rebuilding from the parts would hide the escape, so show it as written.
            Cell::plain(written.raw())
        }
    }

    fn evaluated(&self) -> Cell {
        // Shown as `path` is: a `./…` under the repo's `.obelisk-k8s/`.
        let base = shown(self.shown_root, Path::new(REPO_CONFIG_DIR));
        Cell::from_parts(
            self.build.written,
            |v| match v {
                Var::Target => self.target.to_owned(),
                Var::Unit => self.unit.to_owned(),
                // Has a value, or the build path wouldn't have resolved.
                Var::PathUnit => self.build.path_unit.clone().unwrap_or_default(),
            },
            Some(&base.display().to_string()),
        )
    }

    /// The table alone, without the title line.
    fn write_table(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers = ["RAW", "FROM", "EVALUATED", "STATUS"].map(|h| Cell::styled(HEADER, h));
        let status = if self.build.found {
            Cell::styled(FOUND, "found")
        } else {
            Cell::styled(MISSING, "missing")
        };
        let from = shown(self.shown_root, self.build.declared_in);
        let row = [
            self.raw(),
            Cell::plain(&from.display().to_string()),
            self.evaluated(),
            status,
        ];
        let widths: Vec<usize> = (0..3)
            .map(|col| {
                [&headers, &row]
                    .iter()
                    .map(|r| r[col].plain.chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        for row in [&headers, &row] {
            for (col, cell) in row.iter().enumerate() {
                write!(f, "  ")?;
                cell.write(f, widths.get(col).copied().unwrap_or(0))?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

impl fmt::Display for Explain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (target, unit) = (self.target, self.unit);
        writeln!(f, "build file for unit `{unit}` on target `{target}`:\n")?;
        self.write_table(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::write_repo;

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        write_repo(files)
    }

    /// Repo build `./builds/${{obelisk:target}}.gotmpl`; unit `web` has its own in its directory.
    fn builds_repo() -> tempfile::TempDir {
        repo(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  build: \"./builds/${{obelisk:target}}.gotmpl\"\n",
            ),
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("units/app/unit.yaml", "unit: {}\n"),
            (
                "units/web/unit.yaml",
                "unit:\n  build: \"${{obelisk:path:unit}}/${{obelisk:target}}.gotmpl\"\n",
            ),
            ("builds/prod.gotmpl", ""),
        ])
    }

    #[test]
    fn a_units_build_file_overrides_the_repos() {
        let dir = builds_repo();
        let config = RepoConfig::load(dir.path()).unwrap();
        let app = config.build_path_for(dir.path(), "prod", "app").unwrap();
        assert_eq!(app.path, Path::new(".obelisk-k8s/builds/prod.gotmpl"));
        assert_eq!(app.declared_in, Path::new(".obelisk-k8s/repo.yaml"));
        assert!(app.found);
        let web = config.build_path_for(dir.path(), "prod", "web").unwrap();
        // `${{obelisk:path:unit}}` is the unit's directory, from `.obelisk-k8s/`.
        assert_eq!(web.path, Path::new(".obelisk-k8s/units/web/prod.gotmpl"));
        assert_eq!(
            web.declared_in,
            Path::new(".obelisk-k8s/units/web/unit.yaml")
        );
        assert!(!web.found);
    }

    #[test]
    fn a_repo_build_file_can_use_the_units_path() {
        let dir = repo(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  build: \"${{obelisk:path:unit}}/build.yaml.gotmpl\"\n",
            ),
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("units/a/unit.yaml", "unit: {}\n"),
            ("units/b/unit.yaml", "unit: {}\n"),
            ("units/b/build.yaml.gotmpl", ""),
        ]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let paths: Vec<_> = ["a", "b"]
            .map(|u| config.build_path_for(dir.path(), "prod", u).unwrap())
            .into_iter()
            .map(|b| (b.path.display().to_string(), b.found))
            .collect();
        assert_eq!(
            paths,
            [
                (".obelisk-k8s/units/a/build.yaml.gotmpl".to_owned(), false),
                (".obelisk-k8s/units/b/build.yaml.gotmpl".to_owned(), true),
            ]
        );
    }

    #[test]
    fn an_absolute_build_path_is_used_as_it_is() {
        let dir = repo(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  build: /abs/build.gotmpl\n",
            ),
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("units/app/unit.yaml", "unit: {}\n"),
        ]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let build = config.build_path_for(dir.path(), "prod", "app").unwrap();
        assert_eq!(build.path, Path::new("/abs/build.gotmpl"));
        let selection = config.select(&[], &[]).unwrap();
        let mut report = config.paths_report(dir.path(), &selection, false).unwrap();
        report.shown_root = Some("~/r".into());
        assert_eq!(report.to_string(), "/abs/build.gotmpl\n");
    }

    #[test]
    fn explains_as_a_table() {
        let dir = builds_repo();
        let config = RepoConfig::load(dir.path()).unwrap();
        let build = config.build_path_for(dir.path(), "prod", "web").unwrap();
        let styled = Explain {
            target: "prod",
            unit: "web",
            build: &build,
            shown_root: None,
        }
        .to_string();
        assert_eq!(
            anstream::adapter::strip_str(&styled).to_string(),
            "build file for unit `web` on target `prod`:\n\n\
             \x20 RAW                                                FROM                              EVALUATED                           STATUS\n\
             \x20 ${{obelisk:path:unit}}/${{obelisk:target}}.gotmpl  .obelisk-k8s/units/web/unit.yaml  .obelisk-k8s/units/web/prod.gotmpl  missing\n"
        );
        // Vars and their values are highlighted, and the status colored.
        assert!(
            styled.contains(&format!("{VAR}${{{{obelisk:target}}}}{VAR:#}")),
            "{styled}"
        );
        assert!(
            styled.contains(&format!("{VAR}.obelisk-k8s/units/web{VAR:#}")),
            "{styled}"
        );
        assert!(
            styled.contains(&format!("{MISSING}missing{MISSING:#}")),
            "{styled}"
        );
    }

    #[test]
    fn explain_shows_an_escape_as_written() {
        let dir = repo(&[
            (
                "repo.yaml",
                "repo:\n  name: r\n  build: \"./a$${{b}}.yaml\"\n",
            ),
            ("targets/t/target.yaml", "target:\n  cluster: c\n"),
            ("units/u/unit.yaml", "unit: {}\n"),
        ]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let build = config.build_path_for(dir.path(), "t", "u").unwrap();
        let text = Explain {
            target: "t",
            unit: "u",
            build: &build,
            shown_root: None,
        }
        .to_string();
        let text = anstream::adapter::strip_str(&text).to_string();
        assert!(
            text.contains("./a$${{b}}.yaml  .obelisk-k8s/repo.yaml  .obelisk-k8s/a${{b}}.yaml"),
            "{text}"
        );
    }

    #[test]
    fn refuses_unknown_or_undeployed_names() {
        let dir = repo(&[
            ("repo.yaml", "repo:\n  name: r\n  build: ./b\n"),
            ("targets/prod/target.yaml", "target:\n  cluster: c\n"),
            ("targets/dev/target.yaml", "target:\n  cluster: c\n"),
            ("units/app/unit.yaml", "unit:\n  targets: [dev]\n"),
        ]);
        let config = RepoConfig::load(dir.path()).unwrap();
        let error = |t, u| {
            config
                .build_path_for(dir.path(), t, u)
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
                "repo:\n  name: r\n  build: \"./${{obelisk:unit}}.gotmpl\"\n",
            ),
            ("targets/t1/target.yaml", "target:\n  cluster: c\n"),
            ("targets/t2/target.yaml", "target:\n  cluster: c\n"),
            ("units/u1/unit.yaml", "unit: {}\n"),
            ("units/u2/unit.yaml", "unit:\n  targets: [t2]\n"),
            ("u1.gotmpl", ""),
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
        assert_eq!(plain, ".obelisk-k8s/u1.gotmpl\n");
        let explain = config
            .paths_report(dir.path(), &selection, true)
            .unwrap()
            .to_string();
        assert!(
            explain.starts_with("build file for unit `u1` on target `t1`"),
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
        // A missing build file is shown too: rendering it is the error.
        assert_eq!(
            plain,
            "[ target=t1, unit=u1 ]\n.obelisk-k8s/u1.gotmpl\n\n[ target=t2, unit=u1 ]\n.obelisk-k8s/u1.gotmpl\n\n[ target=t2, unit=u2 ]\n.obelisk-k8s/u2.gotmpl\n"
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
        assert!(!explain.contains("build file for"), "{explain}");
        assert!(
            explain.starts_with("[ target=t1, unit=u1 ]\n  RAW"),
            "{explain}"
        );
        assert!(
            explain.contains("found\n\n[ target=t2, unit=u2 ]\n  RAW"),
            "{explain}"
        );
        assert!(explain.ends_with("missing\n"), "{explain}");
    }
}
