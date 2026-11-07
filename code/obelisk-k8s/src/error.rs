use std::fmt;
use std::path::PathBuf;

/// Every error obelisk-k8s can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ObeliskK8sError {
    /// Running `helm` failed, or it said no.
    #[error("{0}")]
    Helm(String),

    /// Running `k3d` (or `docker`, to check a k3d cluster) failed, or it said no.
    #[error("{0}")]
    K3d(String),

    /// An image didn't build: its `preBuild` or `docker build` couldn't run, or failed.
    #[error("{0}")]
    Image(String),

    /// A values or manifests source couldn't run: a URL that can't be fetched or doesn't match its
    /// `sha256`, or a command that can't run or fails.
    #[error("{0}")]
    Render(String),

    /// Running `kubectl` failed, or what it waited for didn't happen.
    #[error("{0}")]
    Kubectl(String),

    /// Running `obeliskd` failed, or it said no.
    #[error("{0}")]
    Obeliskd(String),

    /// An SSM ref couldn't be resolved: bad syntax, no credentials, a failed fetch, or a missing
    /// parameter.
    #[error("{0}")]
    Ssm(String),

    /// The command was asked for something that isn't there (e.g. an unknown target).
    #[error("{0}")]
    Usage(String),

    #[error(
        "no `.obelisk-k8s` directory here: run obelisk-k8s from the repo root, or pass `--repo <name>`"
    )]
    NoRepoConfig,

    #[error(
        "no global config directory at {}: run `obelisk-k8s config global init`, or point `--global-root` / `OBELISK_K8S_GLOBAL_ROOT` elsewhere",
        .0.display()
    )]
    NoGlobalConfig(PathBuf),

    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    /// The file isn't valid YAML, or doesn't have the expected shape. `message` is the YAML crate's
    /// rendering, which already names the file (and the line and column, with a snippet, when known).
    #[error("{message}")]
    Yaml { path: PathBuf, message: String },

    /// The file parsed, but what it says is wrong (a bad name, an unknown target, ...).
    #[error("{}{}: {message}", .file.display(), .at.map(|at| format!(":{at}")).unwrap_or_default())]
    Config {
        file: PathBuf,
        at: Option<LineCol>,
        message: String,
    },
}

/// A 1-based position in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    pub line: u64,
    pub column: u64,
}

impl fmt::Display for LineCol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}
