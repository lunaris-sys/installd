/// Transactional install with automatic rollback on failure.
///
/// Wraps the multi-step install process. Each step registers what it
/// created. If any subsequent step fails, `Drop` triggers a rollback
/// that undoes all completed steps in reverse order.
///
/// Call `commit()` after all steps succeed to prevent rollback.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::{self, InstallError, Manifest};

/// Tracks state across install steps for rollback.
pub struct InstallTransaction {
    temp_dir: PathBuf,
    manifest: Manifest,
    app_dir: Option<PathBuf>,
    installed_schemas: bool,
    installed_modules: Vec<String>,
    desktop_entry_path: Option<PathBuf>,
    committed: bool,
}

impl InstallTransaction {
    /// Begin a new transaction for the given extracted package.
    pub fn new(temp_dir: PathBuf, manifest: Manifest) -> Self {
        Self {
            temp_dir,
            manifest,
            app_dir: None,
            installed_schemas: false,
            installed_modules: Vec::new(),
            desktop_entry_path: None,
            committed: false,
        }
    }

    /// Check available disk space on the target partition.
    ///
    /// Requires 120% of the extracted package size (20% buffer).
    pub fn check_disk_space(&self) -> Result<(), InstallError> {
        let required = dir_size(&self.temp_dir).unwrap_or(0);
        let required_with_buffer = (required as f64 * 1.2) as u64;

        let target = install::user_apps_dir_pub();
        let available = available_space(&target);

        if available < required_with_buffer {
            return Err(InstallError::InsufficientDiskSpace {
                required: required_with_buffer,
                available,
            });
        }

        tracing::debug!(
            "disk space check: required={} available={}",
            required_with_buffer,
            available
        );
        Ok(())
    }

    /// Step: Install app files to user directory.
    pub fn install_files(&mut self) -> Result<(), InstallError> {
        let dest = install::install_to_user(&self.manifest, &self.temp_dir)?;
        self.app_dir = Some(dest);
        Ok(())
    }

    /// Step: Install GSettings schemas.
    pub fn install_schemas(&mut self) -> Result<(), InstallError> {
        install::install_schemas(&self.manifest, &self.temp_dir)?;
        if !self.manifest.schemas.files.is_empty() {
            self.installed_schemas = true;
        }
        Ok(())
    }

    /// Step: Install bundled modules.
    pub fn install_modules(&mut self) -> Result<(), InstallError> {
        let ids = install::install_modules(&self.manifest, &self.temp_dir)?;
        self.installed_modules = ids;
        Ok(())
    }

    /// Step: Create desktop entry.
    pub fn create_desktop_entry(&mut self) -> Result<(), InstallError> {
        let path = install::create_desktop_entry(&self.manifest)?;
        self.desktop_entry_path = Some(path);
        Ok(())
    }

    /// Mark the transaction as successful. Cleans up the temp directory.
    ///
    /// After this call, `Drop` will not roll back.
    pub fn commit(mut self) {
        self.committed = true;
        let _ = fs::remove_dir_all(&self.temp_dir);
        tracing::info!(
            "transaction committed for {}",
            self.manifest.package.id
        );
    }

    /// Explicitly roll back all completed steps.
    fn rollback(&mut self) {
        let app_id = &self.manifest.package.id;
        tracing::warn!("rolling back install for {app_id}");

        // Reverse order: desktop entry, modules, schemas, app files.

        if let Some(ref path) = self.desktop_entry_path {
            if path.exists() {
                let _ = fs::remove_file(path);
                let _ = Command::new("update-desktop-database")
                    .arg(path.parent().unwrap_or(Path::new(".")))
                    .status();
                tracing::debug!("rollback: removed desktop entry");
            }
        }

        if !self.installed_modules.is_empty() {
            let _ = install::remove_modules(&self.manifest);
            tracing::debug!("rollback: removed {} module(s)", self.installed_modules.len());
        }

        if self.installed_schemas {
            let _ = install::remove_schemas(&self.manifest);
            tracing::debug!("rollback: removed schemas");
        }

        if let Some(ref dir) = self.app_dir {
            if dir.exists() {
                let _ = fs::remove_dir_all(dir);
                tracing::debug!("rollback: removed app directory");
            }
        }

        // Always clean up temp.
        if self.temp_dir.exists() {
            let _ = fs::remove_dir_all(&self.temp_dir);
        }

        tracing::info!("rollback complete for {app_id}");
    }
}

impl Drop for InstallTransaction {
    fn drop(&mut self) {
        if !self.committed {
            self.rollback();
        }
    }
}

/// Calculate total size of a directory tree in bytes.
fn dir_size(path: &Path) -> Result<u64, std::io::Error> {
    let mut total = 0;
    if path.is_file() {
        return Ok(fs::metadata(path)?.len());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            total += dir_size(&p)?;
        } else {
            total += fs::metadata(&p)?.len();
        }
    }
    Ok(total)
}

/// Get available space on the filesystem containing `path`.
///
/// Uses `statvfs` on Linux. Returns `u64::MAX` if the call fails
/// (allows install to proceed on unsupported filesystems).
fn available_space(path: &Path) -> u64 {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;

        // Walk up to find an existing ancestor.
        let mut check = path.to_path_buf();
        while !check.exists() {
            if !check.pop() {
                return u64::MAX;
            }
        }

        let c_path = match CString::new(check.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => return u64::MAX,
        };

        unsafe {
            let mut stat: MaybeUninit<libc::statvfs> = MaybeUninit::uninit();
            if libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) == 0 {
                let stat = stat.assume_init();
                return stat.f_bavail as u64 * stat.f_frsize as u64;
            }
        }
        u64::MAX
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        u64::MAX
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{BinaryInfo, DesktopInfo, Manifest, ModuleInfo, PackageInfo, PermissionInfo, SchemaInfo};

    fn test_manifest() -> Manifest {
        Manifest {
            package: PackageInfo {
                id: "com.test.txn".into(),
                name: "Txn Test".into(),
                version: "1.0.0".into(),
                description: String::new(),
                author: String::new(),
            },
            binary: BinaryInfo {
                path: "bin/app".into(),
                args: vec![],
            },
            desktop: DesktopInfo::default(),
            permissions: PermissionInfo::default(),
            schemas: SchemaInfo::default(),
            modules: ModuleInfo::default(),
        }
    }

    #[test]
    fn test_commit_prevents_rollback() {
        let apps = tempfile::TempDir::new().unwrap();
        let desktop = tempfile::TempDir::new().unwrap();
        let temp = tempfile::TempDir::new().unwrap();

        std::env::set_var("LUNARIS_USER_APPS_DIR", apps.path());
        std::env::set_var("LUNARIS_USER_DESKTOP_DIR", desktop.path());

        // Create minimal extracted package.
        fs::create_dir_all(temp.path().join("bin")).unwrap();
        fs::write(temp.path().join("bin/app"), "#!/bin/sh").unwrap();
        fs::write(temp.path().join("manifest.toml"), "[package]\nid=\"com.test.txn\"\nname=\"T\"\nversion=\"1\"\n[binary]\npath=\"bin/app\"\n").unwrap();

        let manifest = test_manifest();
        let mut txn = InstallTransaction::new(temp.path().to_path_buf(), manifest);
        txn.install_files().unwrap();
        txn.create_desktop_entry().unwrap();

        let app_dir = txn.app_dir.clone().unwrap();
        let entry = txn.desktop_entry_path.clone().unwrap();

        txn.commit();

        // Files should still exist after commit.
        assert!(app_dir.exists());
        assert!(entry.exists());

        // Cleanup.
        std::env::remove_var("LUNARIS_USER_APPS_DIR");
        std::env::remove_var("LUNARIS_USER_DESKTOP_DIR");
    }

    #[test]
    fn test_drop_triggers_rollback() {
        let apps = tempfile::TempDir::new().unwrap();
        let desktop = tempfile::TempDir::new().unwrap();
        let temp = tempfile::TempDir::new().unwrap();

        std::env::set_var("LUNARIS_USER_APPS_DIR", apps.path());
        std::env::set_var("LUNARIS_USER_DESKTOP_DIR", desktop.path());

        fs::create_dir_all(temp.path().join("bin")).unwrap();
        fs::write(temp.path().join("bin/app"), "#!/bin/sh").unwrap();
        fs::write(temp.path().join("manifest.toml"), "[package]\nid=\"com.test.txn\"\nname=\"T\"\nversion=\"1\"\n[binary]\npath=\"bin/app\"\n").unwrap();

        let app_dir;
        let entry_path;

        {
            let manifest = test_manifest();
            let mut txn = InstallTransaction::new(temp.path().to_path_buf(), manifest);
            txn.install_files().unwrap();
            txn.create_desktop_entry().unwrap();

            app_dir = txn.app_dir.clone().unwrap();
            entry_path = txn.desktop_entry_path.clone().unwrap();

            assert!(app_dir.exists());
            assert!(entry_path.exists());

            // Drop without commit -> rollback.
        }

        assert!(!app_dir.exists(), "app dir should be removed by rollback");
        assert!(!entry_path.exists(), "desktop entry should be removed by rollback");

        std::env::remove_var("LUNARIS_USER_APPS_DIR");
        std::env::remove_var("LUNARIS_USER_DESKTOP_DIR");
    }

    #[test]
    fn test_disk_space_check_passes() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("small.txt"), "hello").unwrap();

        std::env::set_var("LUNARIS_USER_APPS_DIR", temp.path());

        let manifest = test_manifest();
        let txn = InstallTransaction::new(temp.path().to_path_buf(), manifest);
        // Should pass -- tempdir filesystem has plenty of space for 5 bytes.
        assert!(txn.check_disk_space().is_ok());

        std::env::remove_var("LUNARIS_USER_APPS_DIR");
    }

    #[test]
    fn test_dir_size() {
        let d = tempfile::TempDir::new().unwrap();
        fs::write(d.path().join("a"), "12345").unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("sub/b"), "678").unwrap();

        let size = dir_size(d.path()).unwrap();
        assert_eq!(size, 8);
    }
}
