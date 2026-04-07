/// Profile writing and validation logic.
///
/// Writes permission profiles to `/var/lib/lunaris/permissions/{uid}/{app_id}.toml`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use thiserror::Error;

const DEFAULT_BASE: &str = "/var/lib/lunaris/permissions";

/// Errors from profile operations.
#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("invalid app_id: {0}")]
    InvalidAppId(String),
    #[error("invalid TOML: {0}")]
    InvalidToml(String),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

/// Get the base directory.
fn base_dir() -> PathBuf {
    std::env::var("LUNARIS_PERMISSIONS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_BASE))
}

/// Get the profile file path for an app.
pub fn profile_path(uid: u32, app_id: &str) -> PathBuf {
    profile_path_in(&base_dir(), uid, app_id)
}

/// Profile path with explicit base directory.
pub fn profile_path_in(base: &Path, uid: u32, app_id: &str) -> PathBuf {
    base.join(uid.to_string()).join(format!("{app_id}.toml"))
}

/// Validate an app_id: reverse-domain notation, no path traversal.
pub fn validate_app_id(app_id: &str) -> Result<(), ProfileError> {
    if app_id.is_empty()
        || app_id.contains('/')
        || app_id.contains("..")
        || app_id.contains('\0')
        || !app_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(ProfileError::InvalidAppId(app_id.into()));
    }
    Ok(())
}

/// Validate that the content is parseable TOML with an [info] section.
pub fn validate_toml(content: &str) -> Result<(), ProfileError> {
    let value: toml::Value =
        toml::from_str(content).map_err(|e| ProfileError::InvalidToml(e.to_string()))?;
    let table = value
        .as_table()
        .ok_or_else(|| ProfileError::InvalidToml("expected table at root".into()))?;
    if !table.contains_key("info") {
        return Err(ProfileError::InvalidToml("missing [info] section".into()));
    }
    Ok(())
}

/// Write a permission profile to the default location.
pub fn write_profile(uid: u32, app_id: &str, content: &str) -> Result<PathBuf, ProfileError> {
    write_profile_in(&base_dir(), uid, app_id, content)
}

/// Write a permission profile to an explicit base directory.
pub fn write_profile_in(
    base: &Path,
    uid: u32,
    app_id: &str,
    content: &str,
) -> Result<PathBuf, ProfileError> {
    validate_app_id(app_id)?;
    validate_toml(content)?;

    let path = profile_path_in(base, uid, app_id);
    let dir = path.parent().unwrap();

    if !dir.exists() {
        fs::create_dir_all(dir)?;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    // Atomic write: temp file then rename.
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)?;
    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644));
    fs::rename(&tmp, &path)?;

    Ok(path)
}

/// Delete a permission profile from the default location.
pub fn delete_profile(uid: u32, app_id: &str) -> Result<(), ProfileError> {
    delete_profile_in(&base_dir(), uid, app_id)
}

/// Delete a permission profile from an explicit base directory.
pub fn delete_profile_in(base: &Path, uid: u32, app_id: &str) -> Result<(), ProfileError> {
    validate_app_id(app_id)?;
    let path = profile_path_in(base, uid, app_id);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Check if a profile exists at the default location.
pub fn profile_exists(uid: u32, app_id: &str) -> bool {
    profile_path(uid, app_id).exists()
}

/// Check if a profile exists at an explicit base directory.
pub fn profile_exists_in(base: &Path, uid: u32, app_id: &str) -> bool {
    profile_path_in(base, uid, app_id).exists()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_PROFILE: &str = r#"
[info]
app_id = "com.test"
tier = "third-party"

[graph]
read = ["com.test.*"]

[filesystem]
documents = true
"#;

    #[test]
    fn test_validate_app_id_valid() {
        assert!(validate_app_id("com.example.app").is_ok());
        assert!(validate_app_id("org.lunaris.contacts").is_ok());
        assert!(validate_app_id("my-app_v2").is_ok());
    }

    #[test]
    fn test_validate_app_id_invalid() {
        assert!(validate_app_id("").is_err());
        assert!(validate_app_id("../evil").is_err());
        assert!(validate_app_id("path/traversal").is_err());
        assert!(validate_app_id("has spaces").is_err());
    }

    #[test]
    fn test_validate_toml_valid() {
        assert!(validate_toml(VALID_PROFILE).is_ok());
    }

    #[test]
    fn test_validate_toml_invalid() {
        assert!(validate_toml("not valid toml {{{{").is_err());
    }

    #[test]
    fn test_validate_toml_missing_info() {
        assert!(validate_toml("[graph]\nread = []").is_err());
    }

    #[test]
    fn test_write_and_read_profile() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_profile_in(dir.path(), 1000, "com.test", VALID_PROFILE).unwrap();
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("com.test"));
    }

    #[test]
    fn test_delete_profile() {
        let dir = tempfile::TempDir::new().unwrap();
        write_profile_in(dir.path(), 1000, "com.test", VALID_PROFILE).unwrap();
        assert!(profile_exists_in(dir.path(), 1000, "com.test"));

        delete_profile_in(dir.path(), 1000, "com.test").unwrap();
        assert!(!profile_exists_in(dir.path(), 1000, "com.test"));

        // Deleting non-existent is OK.
        delete_profile_in(dir.path(), 1000, "com.test").unwrap();
    }

    #[test]
    fn test_profile_path_format() {
        let base = Path::new("/var/lib/lunaris/permissions");
        let p = profile_path_in(base, 1000, "com.app");
        assert_eq!(
            p,
            PathBuf::from("/var/lib/lunaris/permissions/1000/com.app.toml")
        );
    }

    #[test]
    fn test_write_rejects_invalid_app_id() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(write_profile_in(dir.path(), 1000, "../evil", VALID_PROFILE).is_err());
        assert!(write_profile_in(dir.path(), 1000, "", VALID_PROFILE).is_err());
    }
}
