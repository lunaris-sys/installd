/// File system operations for system-wide app installation.
///
/// All operations target root-owned directories:
/// - `/usr/lib/lunaris/apps/{app_id}/` for app binaries and libraries
/// - `/usr/share/applications/` for desktop entries

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

const SYSTEM_APPS_DIR: &str = "/usr/lib/lunaris/apps";
const SYSTEM_DESKTOP_DIR: &str = "/usr/share/applications";

/// Errors from install operations.
#[derive(Debug, Error)]
pub enum InstallError {
    #[error("invalid app_id: {0}")]
    InvalidAppId(String),
    #[error("source path does not exist: {0}")]
    SourceNotFound(String),
    #[error("app already installed: {0}")]
    AlreadyInstalled(String),
    #[error("app not installed: {0}")]
    NotInstalled(String),
    #[error("invalid desktop entry: {0}")]
    InvalidDesktopEntry(String),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

/// Get the system app installation base directory.
fn apps_dir() -> PathBuf {
    std::env::var("LUNARIS_SYSTEM_APPS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(SYSTEM_APPS_DIR))
}

/// Get the system desktop entries directory.
fn desktop_dir() -> PathBuf {
    std::env::var("LUNARIS_SYSTEM_DESKTOP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(SYSTEM_DESKTOP_DIR))
}

/// Validate an app_id: reverse-domain notation, no path traversal.
pub fn validate_app_id(app_id: &str) -> Result<(), InstallError> {
    if app_id.is_empty()
        || app_id.contains('/')
        || app_id.contains("..")
        || app_id.contains('\0')
        || !app_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(InstallError::InvalidAppId(app_id.into()));
    }
    // Must contain at least one dot (reverse-domain).
    if !app_id.contains('.') {
        return Err(InstallError::InvalidAppId(format!(
            "{app_id}: must be reverse-domain notation (e.g. com.example.app)"
        )));
    }
    Ok(())
}

/// Install an app from a prepared source directory to the system-wide location.
///
/// Copies `source_path` (directory) to `/usr/lib/lunaris/apps/{app_id}/`.
/// The source directory should contain `bin/`, `lib/`, `share/` etc.
pub fn install_system(app_id: &str, source_path: &str) -> Result<PathBuf, InstallError> {
    validate_app_id(app_id)?;

    let source = Path::new(source_path);
    if !source.is_dir() {
        return Err(InstallError::SourceNotFound(source_path.into()));
    }

    let dest = apps_dir().join(app_id);
    if dest.exists() {
        return Err(InstallError::AlreadyInstalled(app_id.into()));
    }

    // Create parent directory if needed.
    let base = apps_dir();
    if !base.exists() {
        fs::create_dir_all(&base)?;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755))?;
    }

    // Copy recursively.
    copy_dir_recursive(source, &dest)?;

    // Set permissions: root-owned, world-readable, binaries executable.
    set_install_permissions(&dest)?;

    tracing::info!("installed {app_id} to {}", dest.display());
    Ok(dest)
}

/// Uninstall a system-wide app.
///
/// Removes `/usr/lib/lunaris/apps/{app_id}/` entirely.
pub fn uninstall_system(app_id: &str) -> Result<(), InstallError> {
    validate_app_id(app_id)?;

    let dest = apps_dir().join(app_id);
    if !dest.exists() {
        return Err(InstallError::NotInstalled(app_id.into()));
    }

    fs::remove_dir_all(&dest)?;

    // Also remove desktop entry if present.
    let desktop = desktop_dir().join(format!("{app_id}.desktop"));
    if desktop.exists() {
        fs::remove_file(&desktop)?;
        update_desktop_database();
    }

    tracing::info!("uninstalled {app_id}");
    Ok(())
}

/// Write a desktop entry to `/usr/share/applications/{app_id}.desktop`.
///
/// `entry_content` must be valid desktop entry format (starts with
/// `[Desktop Entry]`). The app_id is validated and the file name is
/// derived from it.
pub fn create_desktop_entry(
    app_id: &str,
    entry_content: &str,
) -> Result<PathBuf, InstallError> {
    validate_app_id(app_id)?;

    if !entry_content.contains("[Desktop Entry]") {
        return Err(InstallError::InvalidDesktopEntry(
            "missing [Desktop Entry] section".into(),
        ));
    }

    let dir = desktop_dir();
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }

    let path = dir.join(format!("{app_id}.desktop"));

    // Atomic write.
    let tmp = path.with_extension("desktop.tmp");
    fs::write(&tmp, entry_content)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
    fs::rename(&tmp, &path)?;

    update_desktop_database();

    tracing::info!("created desktop entry for {app_id}");
    Ok(path)
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), InstallError> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

/// Set correct permissions on an installed app directory.
///
/// Directories: 0o755. Regular files: 0o644. Files in `bin/`: 0o755.
fn set_install_permissions(dir: &Path) -> Result<(), InstallError> {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            set_install_permissions(&path)?;
        } else {
            // Files in bin/ are executable.
            let in_bin = path
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "bin");
            let mode = if in_bin { 0o755 } else { 0o644 };
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        }
    }

    Ok(())
}

/// Run update-desktop-database. Non-fatal if it fails.
fn update_desktop_database() {
    let _ = Command::new("update-desktop-database")
        .arg(desktop_dir())
        .status();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_app_id_valid() {
        assert!(validate_app_id("com.example.app").is_ok());
        assert!(validate_app_id("org.lunaris.contacts").is_ok());
        assert!(validate_app_id("io.github.user.my-app_v2").is_ok());
    }

    #[test]
    fn test_validate_app_id_invalid() {
        assert!(validate_app_id("").is_err());
        assert!(validate_app_id("../evil").is_err());
        assert!(validate_app_id("path/traversal").is_err());
        assert!(validate_app_id("has spaces").is_err());
        assert!(validate_app_id("nodots").is_err());
    }

    #[test]
    fn test_install_and_uninstall() {
        let base = tempfile::TempDir::new().unwrap();
        let src = tempfile::TempDir::new().unwrap();

        // Create a minimal app structure.
        fs::create_dir_all(src.path().join("bin")).unwrap();
        fs::write(src.path().join("bin/myapp"), "#!/bin/sh\necho hi").unwrap();
        fs::create_dir_all(src.path().join("share")).unwrap();
        fs::write(src.path().join("share/readme.txt"), "hello").unwrap();

        // Override base directory.
        std::env::set_var("LUNARIS_SYSTEM_APPS_DIR", base.path());

        let dest = install_system("com.test.app", src.path().to_str().unwrap()).unwrap();
        assert!(dest.join("bin/myapp").exists());
        assert!(dest.join("share/readme.txt").exists());

        // Double install should fail.
        assert!(install_system("com.test.app", src.path().to_str().unwrap()).is_err());

        // Uninstall.
        uninstall_system("com.test.app").unwrap();
        assert!(!dest.exists());

        // Uninstall non-existent should fail.
        assert!(uninstall_system("com.test.app").is_err());

        std::env::remove_var("LUNARIS_SYSTEM_APPS_DIR");
    }

    #[test]
    fn test_create_desktop_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_SYSTEM_DESKTOP_DIR", dir.path());

        let content = "[Desktop Entry]\nType=Application\nName=Test\nExec=/usr/bin/test\n";
        let path = create_desktop_entry("com.test.app", content).unwrap();
        assert!(path.exists());

        let read = fs::read_to_string(&path).unwrap();
        assert!(read.contains("[Desktop Entry]"));
        assert!(read.contains("Name=Test"));

        std::env::remove_var("LUNARIS_SYSTEM_DESKTOP_DIR");
    }

    #[test]
    fn test_create_desktop_entry_invalid() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_SYSTEM_DESKTOP_DIR", dir.path());

        assert!(create_desktop_entry("com.test.app", "not a desktop entry").is_err());

        std::env::remove_var("LUNARIS_SYSTEM_DESKTOP_DIR");
    }
}
