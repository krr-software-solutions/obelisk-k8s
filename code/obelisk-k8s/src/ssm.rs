//! `${{obelisk:aws-ssm:<key>}}`: AWS SSM parameters, in the merged values and in `file` / `url`
//! templates' documents (see `config::values` and `config::manifests`).
//!
//! ```text
//! password: "${{obelisk:aws-ssm:/app/x/db/password}}"  →  password: "<the parameter's value>"
//! url: "postgres://${{obelisk:aws-ssm:/app/x/db/user}}@db"  (a ref can sit inside a string)
//! ```
//!
//! Only string values are looked at, never keys. `$${{obelisk:` writes a literal `${{obelisk:`; any
//! other `${{obelisk:…}}` in a values string is an error, and `${{` that isn't `obelisk:` (escaped
//! or not) is left alone (it may be someone else's syntax).
//!
//! Parameters are fetched (decrypted), the first time a ref is found, with credentials from
//! [`ACCESS_KEY_VAR`], [`SECRET_KEY_VAR`] and [`REGION_VAR`] if all three are set, else from the
//! profile [`PROFILE_VAR`] names (its region, unless [`REGION_VAR`] is set); neither is an error. Every
//! key in a tree is fetched at once (and kept for the run), so one that's missing fails naming all
//! of them. Resolved values are never printed: [`Ssm::redact`] masks them in anything shown.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::ObeliskK8sError;

/// The access key id SSM is read with.
pub const ACCESS_KEY_VAR: &str = "AWS_SSM_ACCESS_KEY";
/// The secret access key SSM is read with.
pub const SECRET_KEY_VAR: &str = "AWS_SSM_SECRET_KEY";
/// The region the parameters are in.
pub const REGION_VAR: &str = "AWS_SSM_REGION";
/// A named AWS profile, used when the three above aren't all set.
pub const PROFILE_VAR: &str = "AWS_SSM_PROFILE";

const OPEN: &str = "${{obelisk:";
const CLOSE: &str = "}}";
const AWS_SSM: &str = "aws-ssm:";
/// obelisk-local's (vals') ref prefix, refused so it can't slip through as a literal.
const LEGACY: &str = "ref+awsssm://";
/// The shortest line of a multi-line value masked on its own.
const MIN_LINE_MASK: usize = 8;
/// SSM's limit on names per `GetParameters`.
const BATCH: usize = 10;

/// What a fetch found: each key's value (a key that isn't there doesn't exist).
type Fetched = BTreeMap<String, String>;

type Fetch = dyn Fn(&[String]) -> Result<Fetched, ObeliskK8sError>;

/// Resolves refs for a run, and masks what it resolved.
pub struct Ssm {
    fetch: Box<Fetch>,
    resolved: RefCell<BTreeMap<String, String>>,
}

impl std::fmt::Debug for Ssm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The values stay out of it, and the fetch has nothing to show.
        let keys: Vec<String> = self.resolved.borrow().keys().cloned().collect();
        f.debug_struct("Ssm")
            .field("resolved", &keys)
            .finish_non_exhaustive()
    }
}

impl Ssm {
    /// Fetches from AWS, with credentials from the `AWS_SSM_*` variables (read when it first fetches):
    /// the access key, secret key and region if all are set, else [`PROFILE_VAR`]'s profile.
    #[must_use]
    pub fn from_env() -> Self {
        Self::with_fetch(aws_fetch())
    }

    pub(crate) fn with_fetch(fetch: Box<Fetch>) -> Self {
        Self {
            fetch,
            resolved: RefCell::new(BTreeMap::new()),
        }
    }

    /// Fetches every key in `keys` not fetched yet.
    ///
    /// # Errors
    ///
    /// [`ObeliskK8sError::Ssm`] if the credentials aren't set, a fetch fails, or keys don't exist
    /// (naming each).
    pub(crate) fn fetch(&self, keys: &BTreeSet<String>) -> Result<(), ObeliskK8sError> {
        let wanted: Vec<String> = {
            let resolved = self.resolved.borrow();
            keys.iter()
                .filter(|k| !resolved.contains_key(*k))
                .cloned()
                .collect()
        };
        if wanted.is_empty() {
            return Ok(());
        }
        let mut missing = Vec::new();
        for batch in wanted.chunks(BATCH) {
            let fetched = (self.fetch)(batch)?;
            let mut resolved = self.resolved.borrow_mut();
            resolved.extend(fetched);
            // Whatever didn't come back (not there, or under another name) is missing, never empty.
            missing.extend(batch.iter().filter(|k| !resolved.contains_key(*k)).cloned());
        }
        if missing.is_empty() {
            Ok(())
        } else {
            missing.sort();
            Err(ObeliskK8sError::Ssm(format!(
                "SSM parameters not found (or not readable with these credentials): {}",
                missing.join(", ")
            )))
        }
    }

    /// `value` with every ref in its strings resolved (all of its keys fetched at once).
    ///
    /// # Errors
    ///
    /// A bad ref (as [`check`], naming `what`); as [`Ssm::fetch`].
    #[cfg(test)]
    pub(crate) fn resolve(&self, value: &mut Value, what: &str) -> Result<(), ObeliskK8sError> {
        self.resolve_all([(what.to_owned(), value)]).map(|_| ())
    }

    /// `values` (each named by its `what`) with every ref in their strings resolved, and
    /// `$${{obelisk:` unescaped, all of their keys fetched at once (so a missing one fails naming
    /// all of them); whether any string changed.
    ///
    /// # Errors
    ///
    /// A bad ref (as [`check`], naming its `what`); as [`Ssm::fetch`].
    pub(crate) fn resolve_all<'v>(
        &self,
        values: impl IntoIterator<Item = (String, &'v mut Value)>,
    ) -> Result<bool, ObeliskK8sError> {
        let values: Vec<(String, &mut Value)> = values.into_iter().collect();
        let mut keys = BTreeSet::new();
        for (what, value) in &values {
            keys_in(value, &mut keys).map_err(|message| bad_ref(what, &message))?;
        }
        if !keys.is_empty() {
            self.fetch(&keys)?;
        }
        // Even without refs: a `$${{obelisk:` still loses its extra `$`.
        let resolved = self.resolved.borrow();
        let mut changed = false;
        for (_, value) in values {
            changed |= substitute(value, &resolved);
        }
        Ok(changed)
    }

    /// `text` with every value resolved so far masked as `<aws-ssm:KEY>` (as written, escaped
    /// inside a JSON or YAML double-quoted string, folded over lines by YAML, and base64-encoded,
    /// alone or inside a longer encoded string, as in a Secret's `data`).
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let resolved = self.resolved.borrow();
        let mut masks: BTreeMap<String, String> = BTreeMap::new();
        for (key, value) in resolved.iter().filter(|(_, v)| !v.is_empty()) {
            let mask = format!("<aws-ssm:{key}>");
            for form in forms(value) {
                masks.entry(form).or_insert_with(|| mask.clone());
            }
        }
        if masks.is_empty() {
            return text.to_owned();
        }
        // Longest first, so a value that contains another is masked whole.
        let mut masks: Vec<(String, String)> = masks.into_iter().collect();
        masks.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        // The bytes a mask can start at: most of `text` is copied without trying any.
        let mut starts = [false; 256];
        for (form, _) in &masks {
            starts[usize::from(form.as_bytes()[0])] = true;
            if form.starts_with(' ') {
                starts[usize::from(b'\n')] = true;
            }
        }
        // One pass over `text`, so a mask already put in is never masked again.
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(c) = rest.chars().next() {
            let found = if starts[usize::from(rest.as_bytes()[0])] {
                masks
                    .iter()
                    .find_map(|(form, mask)| match_len(rest, form).map(|len| (len, mask)))
            } else {
                None
            };
            if let Some((len, mask)) = found {
                out.push_str(mask);
                rest = &rest[len..];
            } else {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
        out
    }

    /// `error` with what it says [redacted](Ssm::redact).
    #[must_use]
    pub fn redact_error(&self, error: ObeliskK8sError) -> ObeliskK8sError {
        use ObeliskK8sError as E;
        let r = |s: String| self.redact(&s);
        match error {
            E::Helm(s) => E::Helm(r(s)),
            E::K3d(s) => E::K3d(r(s)),
            E::Render(s) => E::Render(r(s)),
            E::Kubectl(s) => E::Kubectl(r(s)),
            E::Obeliskd(s) => E::Obeliskd(r(s)),
            E::Usage(s) => E::Usage(r(s)),
            E::Ssm(s) => E::Ssm(r(s)),
            E::Yaml { path, message } => E::Yaml {
                path,
                message: r(message),
            },
            E::Config { file, at, message } => E::Config {
                file,
                at,
                message: r(message),
            },
            other => other,
        }
    }
}

/// Checks the refs in `value`'s strings, without resolving them.
///
/// # Errors
///
/// [`ObeliskK8sError::Ssm`] (naming `what`) for an unclosed `${{obelisk:`, an empty key, or another
/// `obelisk:` var.
pub(crate) fn check(value: &Value, what: &str) -> Result<(), ObeliskK8sError> {
    keys_in(value, &mut BTreeSet::new()).map_err(|message| bad_ref(what, &message))
}

/// `text` (a `file` or `url` template's documents, named `shown`) with its refs resolved, or only
/// checked without `ssm`. Text with refs (or `$${{obelisk:`) is re-written, every document, from
/// its parsed form (comments and formatting go); other text is kept as it is.
///
/// # Errors
///
/// [`ObeliskK8sError::Yaml`] if it has refs and isn't valid YAML; as [`Ssm::resolve_all`] / [`check`].
pub(crate) fn in_manifests(
    ssm: Option<&Ssm>,
    text: &str,
    shown: &std::path::Path,
) -> Result<String, ObeliskK8sError> {
    if !has_refs(text) {
        return Ok(text.to_owned());
    }
    let what = shown.display().to_string();
    let mut documents = crate::yaml::parse_values_stream(text, shown)?;
    if !resolve_or_check(ssm, documents.iter_mut().map(|d| (what.clone(), d)))? {
        return Ok(text.to_owned());
    }
    let mut out = String::new();
    for document in &documents {
        out.push_str("---\n");
        out.push_str(&crate::yaml::to_string(document, shown)?);
    }
    Ok(out)
}

/// Whether `text` may hold refs (or obelisk-local's, to refuse): worth parsing for them.
pub(crate) fn has_refs(text: &str) -> bool {
    text.contains(OPEN) || text.contains(LEGACY)
}

/// Refuses refs in an unpinned `url` source (`what`): what it fetches would pick which parameters
/// are decrypted, and where they go.
///
/// # Errors
///
/// [`ObeliskK8sError::Ssm`] if `text` has refs and `sha256` is `None`.
pub(crate) fn check_pinned(
    text: &str,
    sha256: Option<&str>,
    what: &std::path::Path,
) -> Result<(), ObeliskK8sError> {
    if sha256.is_none() && has_refs(text) {
        return Err(ObeliskK8sError::Ssm(format!(
            "{}: a `url` with SSM refs needs a `sha256`, so what it fetches can't choose which secrets it gets",
            what.display()
        )));
    }
    Ok(())
}

/// `values` resolved with `ssm` (as [`Ssm::resolve_all`]), or only [checked](check) without it (and
/// left as written); whether any string changed.
///
/// # Errors
///
/// As [`Ssm::resolve_all`] / [`check`].
pub(crate) fn resolve_or_check<'v>(
    ssm: Option<&Ssm>,
    values: impl IntoIterator<Item = (String, &'v mut Value)>,
) -> Result<bool, ObeliskK8sError> {
    let Some(ssm) = ssm else {
        for (what, value) in values {
            check(value, &what)?;
        }
        return Ok(false);
    };
    ssm.resolve_all(values)
}

/// The shortest piece of a longer base64 string that's masked: shorter ones match by chance.
const MIN_BASE64_PIECE: usize = 4;

/// The ways `value` (not empty) may show in printed text, for [`Ssm::redact`].
fn forms(value: &str) -> Vec<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};

    let mut forms = vec![value.to_owned()];
    // A JSON string, quotes and all: its inside is how the value looks inside a JSON string.
    let quoted = serde_json::to_string(value).unwrap_or_default();
    forms.push(quoted[1..quoted.len() - 1].to_owned());
    // How YAML may print it: double-quoted (its escapes aren't JSON's: `\x7F`, `\e`, `\0`),
    // single-quoted, or (several lines) a block scalar, line by line.
    let yaml = crate::yaml::to_string(&Value::String(value.to_owned()), std::path::Path::new(""))
        .unwrap_or_default();
    if let Some(inner) = yaml
        .trim_end_matches('\n')
        .strip_prefix('"')
        .and_then(|y| y.strip_suffix('"'))
    {
        forms.push(inner.to_owned());
    }
    forms.push(value.replace('\'', "''"));
    // Lines shorter than a few characters (`{`, `}`) would mask unrelated text everywhere.
    if value.contains('\n') {
        forms.extend(
            value
                .lines()
                .map(str::trim)
                .filter(|l| l.len() >= MIN_LINE_MASK)
                .map(str::to_owned),
        );
    }
    // Base64: alone (padded), and inside a longer encoded string (`b64enc` of a URL holding it),
    // at each of the three byte offsets it can start at. There, the characters it shares with
    // its neighbours (2 or 3 before, 1 after unless it ends a 3-byte group) are left out.
    forms.push(STANDARD.encode(value));
    for offset in 0..3 {
        let mut bytes = vec![0; offset];
        bytes.extend_from_slice(value.as_bytes());
        let encoded = STANDARD_NO_PAD.encode(&bytes);
        let skip = [0, 2, 3][offset];
        let drop = usize::from(bytes.len() % 3 != 0);
        if let Some(piece) = encoded.get(skip..encoded.len().saturating_sub(drop))
            && piece.len() >= MIN_BASE64_PIECE
        {
            forms.push(piece.to_owned());
        }
    }
    forms.retain(|f| !f.is_empty());
    forms
}

/// How long the match of `form` at the start of `text` is, if there is one. A space in `form`
/// also matches a line break and the next line's indentation: where YAML folds a long string.
fn match_len(text: &str, form: &str) -> Option<usize> {
    let (text, form) = (text.as_bytes(), form.as_bytes());
    let (mut i, mut j) = (0, 0);
    while j < form.len() {
        if text.get(i) == Some(&form[j]) {
            i += 1;
        } else if form[j] == b' ' && text.get(i) == Some(&b'\n') {
            i += 1;
            while text.get(i) == Some(&b' ') {
                i += 1;
            }
        } else {
            return None;
        }
        j += 1;
    }
    Some(i)
}

fn bad_ref(what: &str, message: &str) -> ObeliskK8sError {
    ObeliskK8sError::Ssm(format!("{what}: {message}"))
}

/// A string, cut at its refs.
#[derive(Debug, PartialEq, Eq)]
enum Part<'a> {
    Text(&'a str),
    Ref(&'a str),
}

/// `s`'s parts. A message (without `s`, which may already hold secrets) if a ref is bad.
fn parse(s: &str) -> Result<Vec<Part<'_>>, String> {
    let mut parts = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(OPEN) {
        if rest[..i].ends_with('$') {
            // `$${{obelisk:` is a literal `${{obelisk:`: drop the extra `$`.
            parts.push(Part::Text(&rest[..i - 1]));
            parts.push(Part::Text(&rest[i..i + OPEN.len()]));
            rest = &rest[i + OPEN.len()..];
            continue;
        }
        parts.push(Part::Text(&rest[..i]));
        let after = &rest[i + OPEN.len()..];
        let end = after
            .find(CLOSE)
            .ok_or_else(|| "a `${{obelisk:` with no closing `}}`".to_owned())?;
        let inner = &after[..end];
        let key = inner.strip_prefix(AWS_SSM).ok_or_else(|| {
            format!("`${{{{obelisk:{inner}}}}}` isn't available here: only `${{{{obelisk:aws-ssm:<key>}}}}`")
        })?;
        // SSM names have no spaces: one left in would fail the whole batch it's fetched in.
        let key = key.trim();
        if key.is_empty() {
            return Err("`${{obelisk:aws-ssm:}}` needs a key".to_owned());
        }
        parts.push(Part::Ref(key));
        rest = &after[end + CLOSE.len()..];
    }
    parts.push(Part::Text(rest));
    parts.retain(|p| *p != Part::Text(""));
    Ok(parts)
}

fn has_open(s: &str) -> bool {
    s.contains(OPEN)
}

/// Adds the keys of the refs in `value`'s strings to `keys`.
fn keys_in(value: &Value, keys: &mut BTreeSet<String>) -> Result<(), String> {
    match value {
        // obelisk-local's syntax: left as is, it would deploy as the literal string.
        Value::String(s) if s.starts_with(LEGACY) => {
            let key = &s[LEGACY.len()..];
            return Err(format!(
                "`{LEGACY}…` is obelisk-local's syntax: write `${{{{obelisk:aws-ssm:{key}}}}}`"
            ));
        }
        Value::String(s) if has_open(s) => {
            for part in parse(s)? {
                if let Part::Ref(key) = part {
                    keys.insert(key.to_owned());
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                keys_in(item, keys)?;
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                keys_in(item, keys)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Replaces the refs (all already checked, and fetched) in `value`'s strings; whether any changed.
fn substitute(value: &mut Value, resolved: &BTreeMap<String, String>) -> bool {
    match value {
        Value::String(s) if has_open(s) => {
            let parts = parse(s).unwrap_or_default();
            let mut out = String::with_capacity(s.len());
            for part in parts {
                match part {
                    Part::Text(text) => out.push_str(text),
                    Part::Ref(key) => out.push_str(resolved.get(key).map_or("", String::as_str)),
                }
            }
            let changed = out != *s;
            *s = out;
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, i| substitute(i, resolved) | changed),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |changed, i| substitute(i, resolved) | changed),
        _ => false,
    }
}

/// Where the credentials come from.
#[derive(Debug, PartialEq, Eq)]
enum Credentials {
    /// [`ACCESS_KEY_VAR`], [`SECRET_KEY_VAR`] and [`REGION_VAR`], all set.
    Keys {
        access: String,
        secret: String,
        region: String,
    },
    /// [`PROFILE_VAR`]'s profile; its region, unless [`REGION_VAR`] is set.
    Profile {
        name: String,
        region: Option<String>,
    },
}

/// The three variables if they're all set, else the profile; `var` reads one (unset or empty is
/// `None`).
fn credentials(var: impl Fn(&str) -> Option<String>) -> Result<Credentials, ObeliskK8sError> {
    let (access, secret, region) = (var(ACCESS_KEY_VAR), var(SECRET_KEY_VAR), var(REGION_VAR));
    match (access, secret, region, var(PROFILE_VAR)) {
        (Some(access), Some(secret), Some(region), _) => Ok(Credentials::Keys {
            access,
            secret,
            region,
        }),
        (_, _, region, Some(name)) => Ok(Credentials::Profile { name, region }),
        (access, secret, region, None) => {
            let unset: Vec<&str> = [
                (ACCESS_KEY_VAR, access.is_none()),
                (SECRET_KEY_VAR, secret.is_none()),
                (REGION_VAR, region.is_none()),
            ]
            .into_iter()
            .filter_map(|(name, unset)| unset.then_some(name))
            .collect();
            Err(ObeliskK8sError::Ssm(format!(
                "`${{{{obelisk:aws-ssm:…}}}}` refs need {ACCESS_KEY_VAR}, {SECRET_KEY_VAR} and {REGION_VAR} set (unset: {}), or {PROFILE_VAR}",
                unset.join(", ")
            )))
        }
    }
}

/// A client for `credentials`, and the runtime it runs on.
fn client(
    credentials: Credentials,
) -> Result<(tokio::runtime::Runtime, aws_sdk_ssm::Client), ObeliskK8sError> {
    use aws_config::BehaviorVersion;
    use aws_config::profile::{ProfileFileCredentialsProvider, ProfileFileRegionProvider};
    use aws_sdk_ssm::config::Region;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| ObeliskK8sError::Ssm(format!("can't start the AWS client: {error}")))?;
    let config = match credentials {
        Credentials::Keys {
            access,
            secret,
            region,
        } => aws_sdk_ssm::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region))
            .credentials_provider(aws_sdk_ssm::config::Credentials::new(
                access,
                secret,
                None,
                None,
                "obelisk-k8s",
            ))
            .build(),
        Credentials::Profile { name, region } => {
            // Only the profile: not the default chain, which would take AWS_ACCESS_KEY_ID & co. first.
            // The profile's other settings too (endpoint, retries), not `AWS_PROFILE`'s.
            let mut loader = aws_config::defaults(BehaviorVersion::latest())
                .profile_name(&name)
                .credentials_provider(
                    ProfileFileCredentialsProvider::builder()
                        .profile_name(&name)
                        .build(),
                );
            loader = match region {
                Some(region) => loader.region(Region::new(region)),
                None => loader.region(
                    ProfileFileRegionProvider::builder()
                        .profile_name(&name)
                        .build(),
                ),
            };
            let shared = runtime.block_on(loader.load());
            if shared.region().is_none() {
                return Err(ObeliskK8sError::Ssm(format!(
                    "{PROFILE_VAR} profile `{name}` has no region (or isn't in ~/.aws/config): set one in it, or {REGION_VAR}"
                )));
            }
            aws_sdk_ssm::Config::from(&shared)
        }
    };
    Ok((runtime, aws_sdk_ssm::Client::from_conf(config)))
}

/// Fetches from AWS (see [`Ssm::from_env`]): the client is made on the first fetch, and kept.
fn aws_fetch() -> Box<Fetch> {
    let client = RefCell::new(None);
    Box::new(move |keys| {
        let mut client = client.borrow_mut();
        if client.is_none() {
            let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
            *client = Some(self::client(credentials(var)?)?);
        }
        let (runtime, client) = client.as_ref().expect("made above");
        fetch_with(runtime, client, keys)
    })
}

/// `error` and its sources, each once (a source's message often repeats its cause's).
fn chain(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        let message = error.to_string();
        if !out.contains(&message) {
            out.push_str(": ");
            out.push_str(&message);
        }
        source = error.source();
    }
    out
}

/// Fetches `keys` (at most [`BATCH`]) with `client`, decrypted.
fn fetch_with(
    runtime: &tokio::runtime::Runtime,
    client: &aws_sdk_ssm::Client,
    keys: &[String],
) -> Result<Fetched, ObeliskK8sError> {
    let output = runtime
        .block_on(
            client
                .get_parameters()
                .set_names(Some(keys.to_vec()))
                .with_decryption(true)
                .send(),
        )
        .map_err(|error| {
            use aws_sdk_ssm::error::ProvideErrorMetadata as _;
            // AWS's own code and message when it answered; else the whole chain (e.g. no network,
            // or a profile whose credentials can't be loaded).
            let why = match (error.code(), error.message()) {
                (Some(code), Some(message)) => format!("{code}: {message}"),
                _ => chain(&error),
            };
            ObeliskK8sError::Ssm(format!(
                "fetching SSM parameters {} failed: {why}",
                keys.join(", ")
            ))
        })?;
    let mut fetched = Fetched::new();
    for parameter in output.parameters() {
        if let (Some(name), Some(value)) = (parameter.name(), parameter.value()) {
            // `name:3` / `name:label` comes back as `name`, with its selector apart.
            let key = match parameter.selector() {
                Some(selector) => format!("{name}:{}", selector.trim_start_matches(':')),
                None => name.to_owned(),
            };
            fetched.insert(key, value.to_owned());
        }
    }
    Ok(fetched)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// An [`Ssm`] over `params`; the batches it was asked for are kept in `asked`.
    pub(crate) fn fake(params: &[(&str, &str)]) -> (Ssm, std::rc::Rc<RefCell<Vec<Vec<String>>>>) {
        let params: BTreeMap<String, String> = params
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let asked = std::rc::Rc::new(RefCell::new(Vec::new()));
        let log = asked.clone();
        let ssm = Ssm::with_fetch(Box::new(move |keys| {
            log.borrow_mut().push(keys.to_vec());
            Ok(keys
                .iter()
                .filter_map(|k| params.get(k).map(|v| (k.clone(), v.clone())))
                .collect())
        }));
        (ssm, asked)
    }

    #[test]
    fn parses_refs_inside_strings_and_escapes() {
        assert_eq!(
            parse("a${{obelisk:aws-ssm:/x/y}}b").unwrap(),
            [Part::Text("a"), Part::Ref("/x/y"), Part::Text("b")]
        );
        assert_eq!(
            parse("$${{obelisk:aws-ssm:/x}}").unwrap(),
            [Part::Text("${{obelisk:"), Part::Text("aws-ssm:/x}}")]
        );
        assert_eq!(
            parse("${{ github.sha }}").unwrap(),
            [Part::Text("${{ github.sha }}")]
        );
        assert_eq!(
            parse("${{obelisk:target}}").unwrap_err(),
            "`${{obelisk:target}}` isn't available here: only `${{obelisk:aws-ssm:<key>}}`"
        );
        assert_eq!(
            parse("${{obelisk:aws-ssm:/x").unwrap_err(),
            "a `${{obelisk:` with no closing `}}`"
        );
        assert_eq!(
            parse("${{obelisk:aws-ssm:}}").unwrap_err(),
            "`${{obelisk:aws-ssm:}}` needs a key"
        );
    }

    #[test]
    fn resolves_every_string_fetching_each_key_once() {
        let (ssm, asked) = fake(&[("/a", "A"), ("/b", "B")]);
        let mut value = json!({
            "x": "${{obelisk:aws-ssm:/a}}",
            "list": ["pre-${{obelisk:aws-ssm:/b}}-${{obelisk:aws-ssm:/a}}", 3],
            "${{obelisk:aws-ssm:/a}}": "keys are left alone",
        });
        ssm.resolve(&mut value, "values").unwrap();
        assert_eq!(
            value,
            json!({ "x": "A", "list": ["pre-B-A", 3], "${{obelisk:aws-ssm:/a}}": "keys are left alone" })
        );
        ssm.resolve(&mut json!({ "y": "${{obelisk:aws-ssm:/a}}" }), "values")
            .unwrap();
        assert_eq!(*asked.borrow(), [vec!["/a".to_owned(), "/b".to_owned()]]);
    }

    #[test]
    fn names_every_missing_key() {
        let (ssm, _) = fake(&[("/a", "A")]);
        let mut value = json!([
            "${{obelisk:aws-ssm:/z}}",
            "${{obelisk:aws-ssm:/a}}",
            "${{obelisk:aws-ssm:/y}}"
        ]);
        assert_eq!(
            ssm.resolve(&mut value, "values").unwrap_err().to_string(),
            "SSM parameters not found (or not readable with these credentials): /y, /z"
        );
    }

    #[test]
    fn fetches_in_batches_of_ten() {
        let names: Vec<String> = (0..12).map(|i| format!("/k{i:02}")).collect();
        let params: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "v")).collect();
        let (ssm, asked) = fake(&params);
        ssm.fetch(&names.iter().cloned().collect()).unwrap();
        let sizes: Vec<usize> = asked.borrow().iter().map(Vec::len).collect();
        assert_eq!(sizes, [10, 2]);
    }

    #[test]
    fn redacts_values_as_written_and_json_escaped() {
        let (ssm, _) = fake(&[("/p", "pa\"ss"), ("/q", "pa\"ss-longer"), ("/e", "")]);
        ssm.resolve(
            &mut json!([
                "${{obelisk:aws-ssm:/p}}",
                "${{obelisk:aws-ssm:/q}}",
                "${{obelisk:aws-ssm:/e}}"
            ]),
            "values",
        )
        .unwrap();
        assert_eq!(
            ssm.redact("a pa\"ss-longer b {\"x\":\"pa\\\"ss\"} c"),
            "a <aws-ssm:/q> b {\"x\":\"<aws-ssm:/p>\"} c"
        );
        let error = ssm.redact_error(ObeliskK8sError::Helm("bad: pa\"ss".to_owned()));
        assert_eq!(error.to_string(), "bad: <aws-ssm:/p>");
    }

    #[test]
    fn escapes_are_unescaped_without_refs_too() {
        let ssm = Ssm::with_fetch(Box::new(|_| panic!("fetched")));
        let mut value = json!({ "a": "$${{obelisk:aws-ssm:/x}}" });
        ssm.resolve(&mut value, "values").unwrap();
        assert_eq!(value, json!({ "a": "${{obelisk:aws-ssm:/x}}" }));
    }

    #[test]
    fn a_key_that_comes_back_under_another_name_is_missing() {
        // e.g. `/p:3`, which SSM answers as `/p` (with a selector).
        let ssm = Ssm::with_fetch(Box::new(|_| {
            Ok(Fetched::from([("/p".to_owned(), "v".to_owned())]))
        }));
        assert_eq!(
            ssm.resolve(&mut json!("${{obelisk:aws-ssm:/p:3}}"), "values")
                .unwrap_err()
                .to_string(),
            "SSM parameters not found (or not readable with these credentials): /p:3"
        );
    }

    #[test]
    fn redacts_base64_and_never_masks_a_mask() {
        let (ssm, _) = fake(&[("/long", "s3cret-value"), ("/short", "ssm")]);
        ssm.resolve(
            &mut json!(["${{obelisk:aws-ssm:/long}}", "${{obelisk:aws-ssm:/short}}"]),
            "values",
        )
        .unwrap();
        assert_eq!(
            ssm.redact("data:\n  pw: czNjcmV0LXZhbHVl\nraw: s3cret-value ssm\n"),
            "data:\n  pw: <aws-ssm:/long>\nraw: <aws-ssm:/long> <aws-ssm:/short>\n"
        );
    }

    #[test]
    fn redacts_a_long_value_yaml_folds_over_lines() {
        let long = "word ".repeat(40) + "end";
        let (ssm, _) = fake(&[("/l", long.as_str())]);
        let mut value = json!({ "a": "x ${{obelisk:aws-ssm:/l}}" });
        ssm.resolve(&mut value, "values").unwrap();
        let yaml = crate::yaml::to_string(&value, std::path::Path::new("x")).unwrap();
        assert!(yaml.contains(">-\n"), "{yaml}");
        assert_eq!(ssm.redact(&yaml), "a: >-\n  x <aws-ssm:/l>\n");
    }

    #[test]
    fn redacts_yaml_escapes() {
        let (ssm, _) = fake(&[("/c", "p\tass\u{7f}")]);
        let mut value = json!({ "a": "${{obelisk:aws-ssm:/c}}" });
        ssm.resolve(&mut value, "values").unwrap();
        let yaml = crate::yaml::to_string(&value, std::path::Path::new("x")).unwrap();
        assert_eq!(ssm.redact(&yaml), "a: \"<aws-ssm:/c>\"\n", "{yaml}");
    }

    #[test]
    fn redacts_base64_inside_a_longer_encoded_string() {
        use base64::Engine as _;
        let (ssm, _) = fake(&[("/p", "s3cret-pw")]);
        ssm.resolve(&mut json!("${{obelisk:aws-ssm:/p}}"), "values")
            .unwrap();
        for prefix in ["", "u:", "pg://u:"] {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{prefix}s3cret-pw@h"));
            let shown = ssm.redact(&encoded);
            assert!(shown.contains("<aws-ssm:/p>"), "{prefix}: {shown}");
        }
    }

    #[test]
    fn a_keys_surrounding_spaces_are_dropped() {
        assert_eq!(
            parse("${{obelisk:aws-ssm: /x }}").unwrap(),
            [Part::Ref("/x")]
        );
    }

    #[test]
    fn redacts_a_multi_line_value_line_by_line() {
        let (ssm, _) = fake(&[(
            "/k",
            "{\n-----BEGIN KEY-----\nabc123def456\n-----END KEY-----\n}\n",
        )]);
        ssm.resolve(&mut json!("${{obelisk:aws-ssm:/k}}"), "values")
            .unwrap();
        let shown = ssm.redact(
            "key: |\n  {\n  -----BEGIN KEY-----\n  abc123def456\n  -----END KEY-----\n  }\nother: {}\n",
        );
        assert!(!shown.contains("abc123def456"), "{shown}");
        // Too short to mask alone: it'd mask every brace.
        assert!(shown.ends_with("other: {}\n"), "{shown}");
    }

    #[test]
    fn nothing_is_fetched_without_refs() {
        let ssm = Ssm::with_fetch(Box::new(|_| panic!("fetched")));
        ssm.resolve(&mut json!({ "a": "plain ${{ x }}" }), "values")
            .unwrap();
    }

    #[test]
    fn the_three_variables_win_then_the_profile() {
        const ALL: &[(&str, &str)] = &[
            (ACCESS_KEY_VAR, "a"),
            (SECRET_KEY_VAR, "s"),
            (REGION_VAR, "r"),
            (PROFILE_VAR, "p"),
        ];
        let env = |set: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                set.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(
            credentials(env(ALL)).unwrap(),
            Credentials::Keys {
                access: "a".to_owned(),
                secret: "s".to_owned(),
                region: "r".to_owned()
            }
        );
        assert_eq!(
            credentials(env(&[
                (ACCESS_KEY_VAR, "a"),
                (REGION_VAR, "r"),
                (PROFILE_VAR, "p")
            ]))
            .unwrap(),
            Credentials::Profile {
                name: "p".to_owned(),
                region: Some("r".to_owned())
            }
        );
        assert_eq!(
            credentials(env(&[(PROFILE_VAR, "p")])).unwrap(),
            Credentials::Profile {
                name: "p".to_owned(),
                region: None
            }
        );
        assert_eq!(
            credentials(env(&[(SECRET_KEY_VAR, "s")]))
                .unwrap_err()
                .to_string(),
            "`${{obelisk:aws-ssm:…}}` refs need AWS_SSM_ACCESS_KEY, AWS_SSM_SECRET_KEY and AWS_SSM_REGION set (unset: AWS_SSM_ACCESS_KEY, AWS_SSM_REGION), or AWS_SSM_PROFILE"
        );
    }

    #[test]
    fn obelisk_locals_refs_are_refused() {
        assert_eq!(
            check(&json!({ "pw": "ref+awsssm:///app/x" }), "values")
                .unwrap_err()
                .to_string(),
            "values: `ref+awsssm://…` is obelisk-local's syntax: write `${{obelisk:aws-ssm:/app/x}}`"
        );
    }

    #[test]
    fn an_unpinned_url_may_not_hold_refs() {
        let what = std::path::Path::new("https://x/y.yaml");
        let refs = "a: ${{obelisk:aws-ssm:/p}}\n";
        assert!(check_pinned(refs, Some("abc"), what).is_ok());
        assert!(check_pinned("a: b\n", None, what).is_ok());
        assert_eq!(
            check_pinned(refs, None, what).unwrap_err().to_string(),
            "https://x/y.yaml: a `url` with SSM refs needs a `sha256`, so what it fetches can't choose which secrets it gets"
        );
    }
}
