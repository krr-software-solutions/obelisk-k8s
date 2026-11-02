//! All YAML goes through here: nothing outside this module names the YAML crates, so they can be
//! swapped in one place. serde-saphyr reads and writes; yaml-edit changes a file people maintain by
//! hand, keeping its comments and formatting.

use std::path::Path;
use std::str::FromStr as _;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::ObeliskK8sError;
use crate::error::LineCol;

/// A value together with where it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    pub value: T,
    pub at: LineCol,
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Spanned<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spanned = serde_saphyr::Spanned::<T>::deserialize(deserializer)?;
        Ok(Self {
            value: spanned.value,
            at: LineCol {
                line: spanned.referenced.line(),
                column: spanned.referenced.column(),
            },
        })
    }
}

/// Reads `path` and deserializes it as one YAML document.
///
/// `display` is the path as the user should see it in errors.
///
/// # Errors
///
/// [`ObeliskK8sError::Io`] if the file can't be read, [`ObeliskK8sError::Yaml`] if it isn't a `T`.
pub fn read_file<T: DeserializeOwned>(path: &Path, display: &Path) -> Result<T, ObeliskK8sError> {
    read_with(path, display, serde_saphyr::Options::default())
}

/// Reads a values file (`path`, shown as `display`) into a generic tree.
///
/// Its consumers (generators, Helm) read YAML 1.1, so plain numbers with a leading `0` are octal
/// here too (`0755` is 493), not YAML 1.2 decimals; without that, serde-saphyr reads `0755` as the
/// float `755.0`. Other plain scalars still go through the generic tree's types: see
/// `config::values` for what that changes.
///
/// # Errors
///
/// As [`read_file`].
pub fn read_values(path: &Path, display: &Path) -> Result<serde_json::Value, ObeliskK8sError> {
    parse_values(&read_text(path, display)?, display)
}

/// How values are read: see [`read_values`].
fn values_options() -> serde_saphyr::Options {
    serde_saphyr::options! { legacy_octal_numbers: true }
}

/// Parses `text` (shown as `display`) like a values file: see [`read_values`].
///
/// # Errors
///
/// [`ObeliskK8sError::Yaml`] if it isn't valid YAML.
pub fn parse_values(text: &str, display: &Path) -> Result<serde_json::Value, ObeliskK8sError> {
    serde_saphyr::from_str_with_options(text, values_options())
        .map_err(|error| yaml_error(&error, display))
}

fn read_with<T: DeserializeOwned>(
    path: &Path,
    display: &Path,
    options: serde_saphyr::Options,
) -> Result<T, ObeliskK8sError> {
    serde_saphyr::from_str_with_options(&read_text(path, display)?, options)
        .map_err(|error| yaml_error(&error, display))
}

fn read_text(path: &Path, display: &Path) -> Result<String, ObeliskK8sError> {
    std::fs::read_to_string(path).map_err(|source| ObeliskK8sError::Io {
        path: display.to_owned(),
        source,
    })
}

fn yaml_error(error: &serde_saphyr::Error, display: &Path) -> ObeliskK8sError {
    let name = display.display().to_string();
    let rendered = error.render_with_options(serde_saphyr::render_options! {
        source_name: Some(name.as_str()),
    });
    // With a snippet, the rendering names the file itself (`--> <file>:<line>:<col>`); without
    // one (e.g. an empty file) it doesn't.
    let message = if rendered.contains(&name) {
        rendered
    } else {
        format!("{name}: {rendered}")
    };
    ObeliskK8sError::Yaml {
        path: display.to_owned(),
        message,
    }
}

/// Parses `text` (named `display` in errors) as a stream of values documents, read like a values file
/// ([`read_values`]). An empty stream (or one with only comments) is no documents.
///
/// # Errors
///
/// [`ObeliskK8sError::Yaml`] if it isn't valid YAML.
pub fn parse_values_stream(
    text: &str,
    display: &Path,
) -> Result<Vec<serde_json::Value>, ObeliskK8sError> {
    serde_saphyr::from_str_multiple_with_options(text, values_options())
        .map_err(|error| yaml_error(&error, display))
}

/// `value` as YAML. `display` names what it is, for the error.
///
/// # Errors
///
/// [`ObeliskK8sError::Yaml`] if it can't be serialized.
pub fn to_string<T: Serialize>(value: &T, display: &Path) -> Result<String, ObeliskK8sError> {
    serde_saphyr::to_string(value).map_err(|error| ObeliskK8sError::Yaml {
        path: display.to_owned(),
        message: format!("{}: {error}", display.display()),
    })
}

/// Serializes `value` and writes it to `path`, which must not exist yet.
///
/// # Errors
///
/// [`ObeliskK8sError::Yaml`] if `value` can't be serialized, [`ObeliskK8sError::Io`] if the file
/// exists or can't be written.
pub fn write_new_file<T: Serialize>(
    path: &Path,
    display: &Path,
    value: &T,
) -> Result<(), ObeliskK8sError> {
    let text = serde_saphyr::to_string(value).map_err(|error| ObeliskK8sError::Yaml {
        path: display.to_owned(),
        message: format!("{}: {error}", display.display()),
    })?;
    let io_error = |source| ObeliskK8sError::Io {
        path: display.to_owned(),
        source,
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error)?;
    std::io::Write::write_all(&mut file, text.as_bytes()).map_err(io_error)
}

/// `text` with `value` appended to its top-level `key` list (created if it's missing or `null`).
/// Comments and formatting are kept.
///
/// # Errors
///
/// [`ObeliskK8sError::Yaml`] (naming `display`) if `text` isn't a YAML mapping, or `key` holds
/// something other than a list.
pub fn push_to_list(
    text: &str,
    display: &Path,
    key: &str,
    value: &str,
) -> Result<String, ObeliskK8sError> {
    let error = edit_error(display);
    let file = yaml_edit::YamlFile::from_str(text).map_err(|e| error(e.to_string()))?;
    let Some(document) = file.documents().next() else {
        // Nothing but comments (or nothing at all): add the key at the end.
        let fresh = yaml_edit::Document::from_str(&format!("{key}:\n"))
            .map_err(|e| error(e.to_string()))?;
        push(&fresh, key, value).map_err(&error)?;
        let sep = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        return Ok(format!("{text}{sep}{fresh}"));
    };
    push(&document, key, value).map_err(&error)?;
    Ok(file.to_string())
}

fn push(document: &yaml_edit::Document, key: &str, value: &str) -> Result<(), String> {
    let root = document
        .as_mapping()
        .ok_or("the file isn't a YAML mapping")?;
    if let Some(list) = root.get_sequence(key) {
        list.push(value);
    } else if root
        .get(key)
        .is_none_or(|v| v.as_scalar().is_some_and(yaml_edit::Scalar::is_null))
    {
        let list = yaml_edit::Sequence::new();
        list.push(value);
        root.set(key, list);
    } else {
        return Err(format!("`{key}` isn't a list"));
    }
    Ok(())
}

/// `text` with item `index` removed from its top-level `key` list of strings, which starts on line
/// `line` (1-based). Everything else is kept as it was.
///
/// A block item (`- …`) also takes the comment lines directly above it, at its indent and with no
/// blank line between. That's done on the text, and used only if it parses to the same lists as
/// yaml-edit's own removal; otherwise (e.g. a multi-line item) the comments stay.
///
/// # Errors
///
/// [`ObeliskK8sError::Yaml`] (naming `display`) if there's no such list or item.
pub fn remove_from_list(
    text: &str,
    display: &Path,
    key: &str,
    index: usize,
    line: u64,
) -> Result<String, ObeliskK8sError> {
    let error = edit_error(display);
    let file = yaml_edit::YamlFile::from_str(text).map_err(|e| error(e.to_string()))?;
    let document = file
        .documents()
        .next()
        .ok_or_else(|| error("the file is empty".to_owned()))?;
    let root = document
        .as_mapping()
        .ok_or_else(|| error("the file isn't a YAML mapping".to_owned()))?;
    let list = root
        .get_sequence(key)
        .ok_or_else(|| error(format!("no `{key}` list")))?;
    list.remove(index)
        .ok_or_else(|| error(format!("`{key}` has no item {index}")))?;
    let edited = file.to_string();
    let same = |a: &str, b: &str| lists(a).is_some_and(|a| Some(a) == lists(b));
    Ok(match without_item_and_comment(text, line) {
        Some(candidate) if same(&candidate, &edited) => candidate,
        _ => edited,
    })
}

fn edit_error(display: &Path) -> impl Fn(String) -> ObeliskK8sError + '_ {
    move |message| ObeliskK8sError::Yaml {
        path: display.to_owned(),
        message: format!("{}: {message}", display.display()),
    }
}

/// The top-level lists of strings in `text` (`null` and missing read as empty), or `None` if it's
/// anything else.
fn lists(text: &str) -> Option<Vec<(String, Vec<String>)>> {
    let map: indexmap::IndexMap<String, Option<Vec<String>>> = serde_saphyr::from_str(text).ok()?;
    Some(
        map.into_iter()
            .map(|(k, v)| (k, v.unwrap_or_default()))
            .collect(),
    )
}

/// `text` without line `line` (1-based), if that's a block list item (`- …`), and without the comment
/// lines directly above it at the same indent.
fn without_item_and_comment(text: &str, line: u64) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let at = usize::try_from(line).ok()?.checked_sub(1)?;
    let indent = |l: &str| l.len() - l.trim_start_matches(' ').len();
    let item = lines.get(at)?;
    if !item.trim_start_matches(' ').starts_with("- ") {
        return None;
    }
    let mut start = at;
    while start > 0
        && indent(lines[start - 1]) == indent(item)
        && lines[start - 1].trim_start().starts_with('#')
    {
        start -= 1;
    }
    let mut kept = lines;
    kept.drain(start..=at);
    Some(kept.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: &str = "repos.yaml";

    fn push(text: &str, value: &str) -> String {
        push_to_list(text, Path::new(F), "repos", value).unwrap()
    }

    fn remove(text: &str, index: usize, line: u64) -> String {
        remove_from_list(text, Path::new(F), "repos", index, line).unwrap()
    }

    #[test]
    fn pushes_and_quotes() {
        assert_eq!(push("repos:\n  - a\n", "*x"), "repos:\n  - a\n  - '*x'\n");
        assert_eq!(push("repos: [a]\n", "yes"), "repos: [a, 'yes']\n");
        assert_eq!(push("repos:\n", "a"), "repos:\n  - a\n");
        assert_eq!(push("", "a"), "repos:\n  - a\n");
        assert_eq!(push("# c", "a"), "# c\nrepos:\n  - a\n");
        let error = push_to_list("repos: 1\n", Path::new(F), "repos", "a").unwrap_err();
        assert_eq!(error.to_string(), "repos.yaml: `repos` isn't a list");
    }

    #[test]
    fn removes_the_comment_directly_above() {
        let text = "repos:\n  # a\n  # (more)\n  - a\n  # b\n  - b\n";
        assert_eq!(remove(text, 0, 4), "repos:\n  # b\n  - b\n");
        // The last item too.
        assert_eq!(remove(text, 1, 6), "repos:\n  # a\n  # (more)\n  - a\n");
        // The only item: the list reads as empty.
        assert_eq!(
            remove("# top\nrepos:\n  # a\n  - a\n# end\n", 0, 4),
            "# top\nrepos:\n# end\n"
        );
        // Items at indent 0.
        assert_eq!(remove("repos:\n# a\n- a\n- b\n", 0, 3), "repos:\n- b\n");
        // CRLF.
        assert_eq!(
            remove("repos:\r\n  # a\r\n  - a\r\n  - b\r\n", 0, 3),
            "repos:\r\n  - b\r\n"
        );
    }

    #[test]
    fn keeps_comments_that_arent_directly_above() {
        // A blank line between.
        assert_eq!(
            remove("repos:\n  # a\n\n  - a\n  - b\n", 0, 4),
            "repos:\n  # a\n\n  - b\n"
        );
        // Another indent.
        assert_eq!(
            remove("repos:\n# top\n  - a\n  - b\n", 0, 3),
            "repos:\n# top\n  - b\n"
        );
        // A trailing comment stays on its own item.
        assert_eq!(
            remove("repos:\n  - a # x\n  - b # y\n", 1, 3),
            "repos:\n  - a # x\n"
        );
    }

    #[test]
    fn falls_back_to_yaml_edit_when_the_text_edit_doesnt_match() {
        // A multi-line item: dropping one line wouldn't remove it, so yaml-edit's removal is used.
        let text = "repos:\n  # a\n  - \"a\n    b\"\n  - c\n";
        let out = remove(text, 0, 3);
        assert_eq!(
            lists(&out),
            Some(vec![("repos".to_owned(), vec!["c".to_owned()])]),
            "{out}"
        );
        // Flow lists: there's no line to drop.
        assert_eq!(remove("repos: [a, b] # c\n", 1, 1), "repos: [a] # c\n");
    }

    #[test]
    fn removal_errors_name_the_file() {
        let error = remove_from_list("repos: []\n", Path::new(F), "repos", 0, 1).unwrap_err();
        assert_eq!(error.to_string(), "repos.yaml: `repos` has no item 0");
    }
}
