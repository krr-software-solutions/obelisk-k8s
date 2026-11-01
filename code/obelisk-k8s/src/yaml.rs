//! All YAML goes through here: nothing outside this module names the YAML crate (serde-saphyr), so it
//! can be swapped in one place.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

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
    let text = std::fs::read_to_string(path).map_err(|source| ObeliskK8sError::Io {
        path: display.to_owned(),
        source,
    })?;
    serde_saphyr::from_str(&text).map_err(|error| {
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
    })
}
