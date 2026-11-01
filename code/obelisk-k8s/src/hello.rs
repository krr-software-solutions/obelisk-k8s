use crate::ObeliskK8sError;

/// The greeting for `name`.
///
/// # Errors
///
/// [`ObeliskK8sError::Invalid`] if `name` is blank.
pub fn greet(name: &str) -> Result<String, ObeliskK8sError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ObeliskK8sError::Invalid(
            "name must not be blank".to_owned(),
        ));
    }
    Ok(format!("Hello, {name}!"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets_by_name() {
        assert_eq!(greet("world").unwrap(), "Hello, world!");
    }

    #[test]
    fn trims_the_name() {
        assert_eq!(greet("  k3d \n").unwrap(), "Hello, k3d!");
    }

    #[test]
    fn refuses_a_blank_name() {
        assert!(matches!(greet("  "), Err(ObeliskK8sError::Invalid(_))));
    }
}
