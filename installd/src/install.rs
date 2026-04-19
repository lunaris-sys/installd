/// Install and uninstall logic for user-level apps.
///
/// Handles .lunpkg extraction, manifest parsing, file installation to
/// `~/.local/share/lunaris/apps/{app_id}/`, and desktop entry creation.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use thiserror::Error;

/// Errors from install operations.
#[derive(Debug, Error)]
pub enum InstallError {
    #[error("invalid app_id: {0}")]
    InvalidAppId(String),
    #[error("package not found: {0}")]
    PackageNotFound(String),
    #[error("manifest not found in package")]
    ManifestNotFound,
    #[error("signature.sig not found in package")]
    SignatureNotFound,
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("app already installed: {0}")]
    AlreadyInstalled(String),
    #[error("app not installed: {0}")]
    NotInstalled(String),
    #[error("insufficient disk space: need {required} bytes, have {available} bytes")]
    InsufficientDiskSpace { required: u64, available: u64 },
    #[error("flatpak operation failed: {0}")]
    FlatpakFailed(String),
    #[error("trash operation failed: {0}")]
    TrashFailed(String),
    #[error("signature verification failed: {0}")]
    SignatureVerificationFailed(String),
    #[error("schema compilation failed: {0}")]
    SchemaCompileFailed(String),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

/// Package manifest (manifest.toml).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Manifest {
    pub package: PackageInfo,
    pub binary: BinaryInfo,
    #[serde(default)]
    pub desktop: DesktopInfo,
    #[serde(default)]
    pub permissions: PermissionInfo,
    #[serde(default)]
    pub schemas: SchemaInfo,
    #[serde(default)]
    pub modules: ModuleInfo,
    /// Static keybindings the package ships. One fragment file per
    /// package is written to
    /// `~/.config/lunaris/compositor.d/keybindings.d/<package.id>.toml`
    /// and removed on uninstall. Empty by default; packages that do
    /// not ship shortcuts leave the section out entirely.
    #[serde(default, rename = "keybinding")]
    pub keybindings: Vec<KeybindingEntry>,
}

/// [schemas] section.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct SchemaInfo {
    /// Relative paths to GSettings schema files inside the package.
    #[serde(default)]
    pub files: Vec<String>,
}

/// [modules] section.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct ModuleInfo {
    /// Relative paths to bundled module directories inside the package.
    #[serde(default)]
    pub bundled: Vec<String>,
}

/// [package] section.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PackageInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
}

/// [binary] section.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct BinaryInfo {
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// [desktop] section.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct DesktopInfo {
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub mime_types: Vec<String>,
}

/// [permissions] section.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct PermissionInfo {
    #[serde(default)]
    pub graph_read: Vec<String>,
    #[serde(default)]
    pub graph_write: Vec<String>,
    #[serde(default)]
    pub filesystem: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub notifications: bool,
    #[serde(default)]
    pub clipboard: bool,
    /// Input subsystem permission requests. Known values:
    /// `"register_focused_bindings"`, `"register_global_bindings"`.
    /// Entries are matched case-sensitively.
    #[serde(default)]
    pub input: Vec<String>,
}

impl PermissionInfo {
    /// True iff `permissions.input` declares `"register_global_bindings"`.
    pub fn can_register_global_bindings(&self) -> bool {
        self.input.iter().any(|p| p == "register_global_bindings")
    }
}

/// One entry of the `[[keybinding]]` array in a package manifest.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct KeybindingEntry {
    pub id: String,
    pub label: String,
    pub default_binding: String,
    /// Optional pre-composed action. If absent, the install daemon
    /// synthesises `module:<package.id>:<id>`.
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `"global"` (default) or `"focused"`. See the manifest in
    /// `sdk/modules` for the semantic contract.
    #[serde(default = "default_keybinding_scope")]
    pub scope: String,
}

fn default_keybinding_scope() -> String {
    "global".into()
}

impl KeybindingEntry {
    /// Resolve the action string that should be written to the
    /// compositor fragment for this entry.
    pub fn effective_action(&self, package_id: &str) -> String {
        self.action
            .clone()
            .unwrap_or_else(|| format!("module:{package_id}:{}", self.id))
    }
}

/// Get the user apps install directory (public for transaction disk check).
pub fn user_apps_dir_pub() -> PathBuf {
    user_apps_dir()
}

/// Get the user apps install directory.
fn user_apps_dir() -> PathBuf {
    std::env::var("LUNARIS_USER_APPS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("~/.local/share"))
                .join("lunaris/apps")
        })
}

/// Get the user desktop entries directory.
fn user_desktop_dir() -> PathBuf {
    std::env::var("LUNARIS_USER_DESKTOP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("~/.local/share"))
                .join("applications")
        })
}

/// Directory for compositor keybinding fragments written by the
/// install daemon on behalf of module packages. The compositor watches
/// this path and merges every `*.toml` into its static binding set.
///
/// Overrideable via `LUNARIS_USER_KEYBINDINGS_DIR` for tests.
pub fn user_keybindings_fragment_dir() -> PathBuf {
    std::env::var("LUNARIS_USER_KEYBINDINGS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("~/.config"))
                .join("lunaris/compositor.d/keybindings.d")
        })
}

/// Get the user GSettings schemas directory.
fn user_schemas_dir() -> PathBuf {
    std::env::var("LUNARIS_USER_SCHEMAS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("~/.local/share"))
                .join("glib-2.0/schemas")
        })
}

/// Get the user modules directory.
fn user_modules_dir() -> PathBuf {
    std::env::var("LUNARIS_USER_MODULES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("~/.local/share"))
                .join("lunaris/modules")
        })
}

/// Validate an app_id.
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
    if !app_id.contains('.') {
        return Err(InstallError::InvalidAppId(format!(
            "{app_id}: must be reverse-domain"
        )));
    }
    Ok(())
}

/// Extract a .lunpkg archive (tar.zst) to a temporary directory.
///
/// Returns the path to the extracted directory.
pub fn extract_package(path: &str) -> Result<PathBuf, InstallError> {
    let pkg_path = Path::new(path);
    if !pkg_path.exists() {
        return Err(InstallError::PackageNotFound(path.into()));
    }

    let temp_dir = std::env::temp_dir().join(format!(
        "lunaris-install-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp_dir)?;

    let file = fs::File::open(pkg_path)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&temp_dir)?;

    Ok(temp_dir)
}

/// Load and parse manifest.toml from an extracted package directory.
pub fn load_manifest(extracted_dir: &Path) -> Result<Manifest, InstallError> {
    let manifest_path = extracted_dir.join("manifest.toml");
    if !manifest_path.exists() {
        return Err(InstallError::ManifestNotFound);
    }

    let content = fs::read_to_string(&manifest_path)?;
    let manifest: Manifest = toml::from_str(&content)
        .map_err(|e| InstallError::InvalidManifest(e.to_string()))?;

    Ok(manifest)
}

/// Validate the manifest contents.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), InstallError> {
    validate_app_id(&manifest.package.id)?;

    if manifest.package.name.is_empty() {
        return Err(InstallError::InvalidManifest("empty package name".into()));
    }
    if manifest.package.version.is_empty() {
        return Err(InstallError::InvalidManifest("empty version".into()));
    }
    if manifest.binary.path.is_empty() {
        return Err(InstallError::InvalidManifest("empty binary path".into()));
    }

    // Check for path traversal in binary path.
    if manifest.binary.path.contains("..") {
        return Err(InstallError::InvalidManifest(
            "binary path contains '..'".into(),
        ));
    }

    Ok(())
}

/// Validate the extracted package structure.
///
/// Checks that `signature.sig` exists. Actual cryptographic verification
/// is deferred to #6.
pub fn validate_package_structure(extracted_dir: &Path) -> Result<(), InstallError> {
    let sig = extracted_dir.join("signature.sig");
    if !sig.exists() {
        return Err(InstallError::SignatureNotFound);
    }
    Ok(())
}

/// Install GSettings schemas from the package.
///
/// Copies `*.gschema.xml` files listed in `[schemas].files` to
/// `~/.local/share/glib-2.0/schemas/` and runs `glib-compile-schemas`.
pub fn install_schemas(
    manifest: &Manifest,
    extracted_dir: &Path,
) -> Result<(), InstallError> {
    if manifest.schemas.files.is_empty() {
        return Ok(());
    }

    let dest = user_schemas_dir();
    fs::create_dir_all(&dest)?;

    for rel_path in &manifest.schemas.files {
        // Guard against path traversal.
        if rel_path.contains("..") {
            return Err(InstallError::InvalidManifest(format!(
                "schema path contains '..': {rel_path}"
            )));
        }

        let src = extracted_dir.join(rel_path);
        if !src.exists() {
            tracing::warn!("schema file not found in package: {rel_path}");
            continue;
        }

        let file_name = src
            .file_name()
            .ok_or_else(|| InstallError::InvalidManifest("invalid schema path".into()))?;
        fs::copy(&src, dest.join(file_name))?;
    }

    // Compile schemas.
    let output = Command::new("glib-compile-schemas")
        .arg(&dest)
        .output();

    match output {
        Ok(o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("glib-compile-schemas failed: {stderr}");
            // Non-fatal: app can still work without compiled schemas.
        }
        Err(e) => {
            tracing::warn!("glib-compile-schemas not found: {e}");
        }
        Ok(_) => {
            tracing::info!("compiled GSettings schemas");
        }
    }

    Ok(())
}

/// Remove GSettings schemas installed by an app.
pub fn remove_schemas(manifest: &Manifest) -> Result<(), InstallError> {
    if manifest.schemas.files.is_empty() {
        return Ok(());
    }

    let dest = user_schemas_dir();

    for rel_path in &manifest.schemas.files {
        let file_name = Path::new(rel_path)
            .file_name()
            .map(|n| dest.join(n));
        if let Some(path) = file_name {
            if path.exists() {
                fs::remove_file(&path)?;
            }
        }
    }

    // Recompile.
    let _ = Command::new("glib-compile-schemas").arg(&dest).status();
    Ok(())
}

/// Install bundled Lunaris modules from the package.
///
/// Copies each module directory listed in `[modules].bundled` to
/// `~/.local/share/lunaris/modules/{module_id}/`.
pub fn install_modules(
    manifest: &Manifest,
    extracted_dir: &Path,
) -> Result<Vec<String>, InstallError> {
    if manifest.modules.bundled.is_empty() {
        return Ok(vec![]);
    }

    let dest_base = user_modules_dir();
    fs::create_dir_all(&dest_base)?;

    let mut installed = Vec::new();

    for rel_path in &manifest.modules.bundled {
        if rel_path.contains("..") {
            return Err(InstallError::InvalidManifest(format!(
                "module path contains '..': {rel_path}"
            )));
        }

        let src = extracted_dir.join(rel_path);
        if !src.is_dir() {
            tracing::warn!("module directory not found in package: {rel_path}");
            continue;
        }

        // Module ID is the directory name.
        let module_id = src
            .file_name()
            .ok_or_else(|| InstallError::InvalidManifest("invalid module path".into()))?
            .to_string_lossy()
            .to_string();

        let dest = dest_base.join(&module_id);
        if dest.exists() {
            tracing::warn!("module {module_id} already exists, overwriting");
            fs::remove_dir_all(&dest)?;
        }

        copy_dir_recursive(&src, &dest)?;
        installed.push(module_id);
    }

    if !installed.is_empty() {
        tracing::info!("installed {} bundled module(s)", installed.len());
    }

    Ok(installed)
}

/// Remove bundled modules installed by an app.
pub fn remove_modules(manifest: &Manifest) -> Result<(), InstallError> {
    if manifest.modules.bundled.is_empty() {
        return Ok(());
    }

    let dest_base = user_modules_dir();

    for rel_path in &manifest.modules.bundled {
        let module_id = Path::new(rel_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string());
        if let Some(id) = module_id {
            let dest = dest_base.join(&id);
            if dest.exists() {
                fs::remove_dir_all(&dest)?;
                tracing::info!("removed module {id}");
            }
        }
    }

    Ok(())
}

/// Install extracted package files to the user directory.
pub fn install_to_user(
    manifest: &Manifest,
    extracted_dir: &Path,
) -> Result<PathBuf, InstallError> {
    let dest = user_apps_dir().join(&manifest.package.id);
    if dest.exists() {
        return Err(InstallError::AlreadyInstalled(
            manifest.package.id.clone(),
        ));
    }

    fs::create_dir_all(&dest)?;

    // Copy bin/.
    let src_bin = extracted_dir.join("bin");
    if src_bin.exists() {
        copy_dir_recursive(&src_bin, &dest.join("bin"))?;
        // Make binaries executable.
        if let Ok(entries) = fs::read_dir(dest.join("bin")) {
            for entry in entries.flatten() {
                let _ = fs::set_permissions(
                    entry.path(),
                    fs::Permissions::from_mode(0o755),
                );
            }
        }
    }

    // Copy lib/.
    let src_lib = extracted_dir.join("lib");
    if src_lib.exists() {
        copy_dir_recursive(&src_lib, &dest.join("lib"))?;
    }

    // Copy share/.
    let src_share = extracted_dir.join("share");
    if src_share.exists() {
        copy_dir_recursive(&src_share, &dest.join("share"))?;
    }

    // Write manifest and signature to install directory for later reference.
    let manifest_src = extracted_dir.join("manifest.toml");
    if manifest_src.exists() {
        fs::copy(&manifest_src, dest.join("manifest.toml"))?;
    }
    let sig_src = extracted_dir.join("signature.sig");
    if sig_src.exists() {
        fs::copy(&sig_src, dest.join("signature.sig"))?;
    }

    tracing::info!(
        "installed {} v{} to {}",
        manifest.package.id,
        manifest.package.version,
        dest.display()
    );

    Ok(dest)
}

/// Uninstall a user-level app.
///
/// Loads the manifest first to clean up schemas and modules, then
/// removes the app directory.
pub fn uninstall_user(app_id: &str) -> Result<(), InstallError> {
    validate_app_id(app_id)?;

    let dest = user_apps_dir().join(app_id);
    if !dest.exists() {
        return Err(InstallError::NotInstalled(app_id.into()));
    }

    // Try to load manifest for cleanup. Non-fatal if missing.
    if let Ok(manifest) = load_manifest(&dest) {
        let _ = remove_schemas(&manifest);
        let _ = remove_modules(&manifest);
        // Fragment cleanup is best-effort: a missing fragment on
        // uninstall is normal for packages that never shipped one.
        let _ = remove_keybindings_fragment(&manifest.package.id);
    }

    fs::remove_dir_all(&dest)?;
    tracing::info!("uninstalled {app_id}");
    Ok(())
}

/// Write a keybinding fragment for this package.
///
/// Produces `~/.config/lunaris/compositor.d/keybindings.d/<id>.toml`
/// containing a `[keybindings]` table of `"accelerator" = "action"`
/// entries, one per manifest-declared binding. The compositor watcher
/// picks it up on the next inotify tick.
///
/// * No-op if the manifest has no `[[keybinding]]` entries.
/// * Global-scope entries without the
///   `permissions.input = ["register_global_bindings"]` grant are
///   skipped with a warning; focused-scope entries are always honoured.
/// * Atomic: writes to `<file>.toml.tmp` then renames.
pub fn write_keybindings_fragment(manifest: &Manifest) -> Result<Option<PathBuf>, InstallError> {
    if manifest.keybindings.is_empty() {
        return Ok(None);
    }
    let package_id = &manifest.package.id;
    let dir = user_keybindings_fragment_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{package_id}.toml"));

    let mut content = String::new();
    content.push_str(&format!(
        "# Auto-generated keybindings for {package_id}\n",
    ));
    content.push_str("# Managed by installd — do not edit manually.\n\n");
    content.push_str("[keybindings]\n");

    let can_global = manifest.permissions.can_register_global_bindings();
    let mut written = 0u32;
    for kb in &manifest.keybindings {
        if kb.scope == "global" && !can_global {
            tracing::warn!(
                "installd: {package_id} declares global binding {:?} but has no \
                 register_global_bindings permission — skipping",
                kb.id
            );
            continue;
        }
        let action = kb.effective_action(package_id);
        // Escape only double quotes; accelerator grammar allows no other
        // special TOML characters today.
        let binding = kb.default_binding.replace('"', "\\\"");
        let action_escaped = action.replace('"', "\\\"");
        content.push_str(&format!(
            "\"{binding}\" = \"{action_escaped}\"  # {}\n",
            kb.label
        ));
        written += 1;
    }

    if written == 0 {
        // Nothing to persist — avoid creating an empty fragment that
        // the compositor would re-read for no reason.
        return Ok(None);
    }

    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, &content)?;
    fs::rename(&tmp, &path)?;
    tracing::info!("installd: wrote keybinding fragment {:?}", path);
    Ok(Some(path))
}

/// Remove a previously written keybinding fragment for `package_id`.
/// No-op if the fragment does not exist.
pub fn remove_keybindings_fragment(package_id: &str) -> Result<(), InstallError> {
    let path = user_keybindings_fragment_dir().join(format!("{package_id}.toml"));
    if path.exists() {
        fs::remove_file(&path)?;
        tracing::info!("installd: removed keybinding fragment {:?}", path);
    }
    Ok(())
}

/// Create a desktop entry for a user-level app.
pub fn create_desktop_entry(manifest: &Manifest) -> Result<PathBuf, InstallError> {
    let install_dir = user_apps_dir().join(&manifest.package.id);
    let binary_path = install_dir.join(&manifest.binary.path);

    let categories = if manifest.desktop.categories.is_empty() {
        String::new()
    } else {
        format!(
            "Categories={}\n",
            manifest.desktop.categories.join(";")
        )
    };

    let keywords = if manifest.desktop.keywords.is_empty() {
        String::new()
    } else {
        format!(
            "Keywords={}\n",
            manifest.desktop.keywords.join(";")
        )
    };

    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Comment={description}\n\
         Exec={exec}\n\
         Icon={icon}\n\
         Terminal=false\n\
         StartupWMClass={app_id}\n\
         {categories}\
         {keywords}",
        name = manifest.package.name,
        description = manifest.package.description,
        exec = binary_path.display(),
        icon = manifest.package.id,
        app_id = manifest.package.id,
        categories = categories,
        keywords = keywords,
    );

    let dir = user_desktop_dir();
    fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{}.desktop", manifest.package.id));
    let tmp = path.with_extension("desktop.tmp");
    fs::write(&tmp, &entry)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
    fs::rename(&tmp, &path)?;

    let _ = Command::new("update-desktop-database")
        .arg(&dir)
        .status();

    Ok(path)
}

/// Remove a desktop entry for a user-level app.
pub fn remove_desktop_entry(app_id: &str) -> Result<(), InstallError> {
    validate_app_id(app_id)?;
    let path = user_desktop_dir().join(format!("{app_id}.desktop"));
    if path.exists() {
        fs::remove_file(&path)?;
        let _ = Command::new("update-desktop-database")
            .arg(user_desktop_dir())
            .status();
    }
    Ok(())
}

/// List installed user-level apps.
///
/// Returns `Vec<(app_id, name, version, source)>`.
pub fn list_installed() -> Vec<(String, String, String, String)> {
    let dir = user_apps_dir();
    let mut apps = Vec::new();

    let Ok(entries) = fs::read_dir(&dir) else {
        return apps;
    };

    for entry in entries.flatten() {
        let app_id = entry.file_name().to_string_lossy().to_string();
        let manifest_path = entry.path().join("manifest.toml");

        if let Ok(content) = fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = toml::from_str::<Manifest>(&content) {
                apps.push((
                    manifest.package.id,
                    manifest.package.name,
                    manifest.package.version,
                    "lunpkg".into(),
                ));
                continue;
            }
        }

        // No manifest: bare entry.
        apps.push((app_id, String::new(), String::new(), "unknown".into()));
    }

    apps
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Tests use env vars for directory overrides. Run with --test-threads=1
// to avoid races: cargo test -p lunaris-installd -- --test-threads=1
#[cfg(test)]
mod tests {
    use super::*;

    fn make_manifest() -> Manifest {
        Manifest {
            package: PackageInfo {
                id: "com.test.app".into(),
                name: "Test App".into(),
                version: "1.0.0".into(),
                description: "A test app".into(),
                author: "Test".into(),
            },
            binary: BinaryInfo {
                path: "bin/testapp".into(),
                args: vec![],
            },
            desktop: DesktopInfo {
                categories: vec!["Utility".into()],
                keywords: vec!["test".into()],
                mime_types: vec![],
            },
            permissions: PermissionInfo::default(),
            schemas: SchemaInfo::default(),
            modules: ModuleInfo::default(),
            keybindings: Vec::new(),
        }
    }

    #[test]
    fn test_validate_app_id() {
        assert!(validate_app_id("com.example.app").is_ok());
        assert!(validate_app_id("nodots").is_err());
        assert!(validate_app_id("../evil").is_err());
        assert!(validate_app_id("").is_err());
    }

    #[test]
    fn test_validate_manifest() {
        let m = make_manifest();
        assert!(validate_manifest(&m).is_ok());
    }

    #[test]
    fn test_validate_manifest_empty_name() {
        let mut m = make_manifest();
        m.package.name = String::new();
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn test_validate_manifest_traversal() {
        let mut m = make_manifest();
        m.binary.path = "../../../bin/evil".into();
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn test_install_and_uninstall_user() {
        let base = tempfile::TempDir::new().unwrap();
        let extracted = tempfile::TempDir::new().unwrap();

        // Create a minimal extracted package.
        fs::create_dir_all(extracted.path().join("bin")).unwrap();
        fs::write(extracted.path().join("bin/testapp"), "#!/bin/sh\necho hi").unwrap();

        let manifest_toml = r#"
[package]
id = "com.test.app"
name = "Test App"
version = "1.0.0"

[binary]
path = "bin/testapp"
"#;
        fs::write(extracted.path().join("manifest.toml"), manifest_toml).unwrap();

        std::env::set_var("LUNARIS_USER_APPS_DIR", base.path());

        let manifest = load_manifest(extracted.path()).unwrap();
        validate_manifest(&manifest).unwrap();

        let dest = install_to_user(&manifest, extracted.path()).unwrap();
        assert!(dest.join("bin/testapp").exists());
        assert!(dest.join("manifest.toml").exists());

        // Double install fails.
        assert!(install_to_user(&manifest, extracted.path()).is_err());

        // List includes the app.
        let installed = list_installed();
        assert!(installed.iter().any(|(id, ..)| id == "com.test.app"));

        // Uninstall.
        uninstall_user("com.test.app").unwrap();
        assert!(!dest.exists());

        std::env::remove_var("LUNARIS_USER_APPS_DIR");
    }

    #[test]
    fn test_list_empty() {
        let base = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_APPS_DIR", base.path());
        let apps = list_installed();
        assert!(apps.is_empty());
        std::env::remove_var("LUNARIS_USER_APPS_DIR");
    }

    #[test]
    fn test_validate_package_structure_missing_sig() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("manifest.toml"), "").unwrap();
        assert!(validate_package_structure(dir.path()).is_err());
    }

    #[test]
    fn test_validate_package_structure_with_sig() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("signature.sig"), b"fake-sig").unwrap();
        assert!(validate_package_structure(dir.path()).is_ok());
    }

    #[test]
    fn test_install_schemas() {
        let schemas_dir = tempfile::TempDir::new().unwrap();
        let extracted = tempfile::TempDir::new().unwrap();

        std::env::set_var("LUNARIS_USER_SCHEMAS_DIR", schemas_dir.path());

        // Create a schema file in the package.
        fs::create_dir_all(extracted.path().join("schemas")).unwrap();
        fs::write(
            extracted.path().join("schemas/com.test.app.gschema.xml"),
            "<schemalist></schemalist>",
        )
        .unwrap();

        let mut manifest = make_manifest();
        manifest.schemas.files = vec!["schemas/com.test.app.gschema.xml".into()];

        install_schemas(&manifest, extracted.path()).unwrap();

        assert!(schemas_dir
            .path()
            .join("com.test.app.gschema.xml")
            .exists());

        // Remove schemas.
        remove_schemas(&manifest).unwrap();
        assert!(!schemas_dir
            .path()
            .join("com.test.app.gschema.xml")
            .exists());

        std::env::remove_var("LUNARIS_USER_SCHEMAS_DIR");
    }

    #[test]
    fn test_install_schemas_path_traversal() {
        let schemas_dir = tempfile::TempDir::new().unwrap();
        let extracted = tempfile::TempDir::new().unwrap();

        std::env::set_var("LUNARIS_USER_SCHEMAS_DIR", schemas_dir.path());

        let mut manifest = make_manifest();
        manifest.schemas.files = vec!["../../etc/passwd".into()];

        assert!(install_schemas(&manifest, extracted.path()).is_err());

        std::env::remove_var("LUNARIS_USER_SCHEMAS_DIR");
    }

    #[test]
    fn test_install_modules() {
        let modules_dir = tempfile::TempDir::new().unwrap();
        let extracted = tempfile::TempDir::new().unwrap();

        std::env::set_var("LUNARIS_USER_MODULES_DIR", modules_dir.path());

        // Create a module directory in the package.
        let mod_dir = extracted
            .path()
            .join("modules/com.test.app.waypointer");
        fs::create_dir_all(&mod_dir).unwrap();
        fs::write(
            mod_dir.join("manifest.toml"),
            "[module]\nid = \"com.test.app.waypointer\"\n",
        )
        .unwrap();
        fs::create_dir_all(mod_dir.join("dist")).unwrap();
        fs::write(mod_dir.join("dist/index.js"), "console.log('hi')").unwrap();

        let mut manifest = make_manifest();
        manifest.modules.bundled = vec!["modules/com.test.app.waypointer".into()];

        let installed = install_modules(&manifest, extracted.path()).unwrap();
        assert_eq!(installed, vec!["com.test.app.waypointer"]);
        assert!(modules_dir
            .path()
            .join("com.test.app.waypointer/manifest.toml")
            .exists());
        assert!(modules_dir
            .path()
            .join("com.test.app.waypointer/dist/index.js")
            .exists());

        // Remove modules.
        remove_modules(&manifest).unwrap();
        assert!(!modules_dir
            .path()
            .join("com.test.app.waypointer")
            .exists());

        std::env::remove_var("LUNARIS_USER_MODULES_DIR");
    }

    #[test]
    fn test_install_modules_path_traversal() {
        let modules_dir = tempfile::TempDir::new().unwrap();
        let extracted = tempfile::TempDir::new().unwrap();

        std::env::set_var("LUNARIS_USER_MODULES_DIR", modules_dir.path());

        let mut manifest = make_manifest();
        manifest.modules.bundled = vec!["../../etc".into()];

        assert!(install_modules(&manifest, extracted.path()).is_err());

        std::env::remove_var("LUNARIS_USER_MODULES_DIR");
    }

    #[test]
    fn test_manifest_with_schemas_and_modules() {
        let toml_str = r#"
[package]
id = "com.test.full"
name = "Full App"
version = "2.0.0"

[binary]
path = "bin/app"

[schemas]
files = ["schemas/com.test.full.gschema.xml"]

[modules]
bundled = ["modules/com.test.full.waypointer"]
"#;
        let manifest: Manifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.schemas.files.len(), 1);
        assert_eq!(manifest.modules.bundled.len(), 1);
        assert!(validate_manifest(&manifest).is_ok());
    }

    // ── Keybinding fragments ───────────────────────────────────────────

    fn manifest_with_bindings(
        package_id: &str,
        grant_global: bool,
        bindings: Vec<KeybindingEntry>,
    ) -> Manifest {
        let mut m = make_manifest();
        m.package.id = package_id.into();
        if grant_global {
            m.permissions.input.push("register_global_bindings".into());
        }
        m.keybindings = bindings;
        m
    }

    #[test]
    fn manifest_parses_keybinding_section() {
        let toml_str = r#"
[package]
id = "com.test.kb"
name = "KB"
version = "1.0.0"

[binary]
path = "bin/kb"

[permissions]
input = ["register_global_bindings"]

[[keybinding]]
id = "open"
label = "Open"
default_binding = "Super+O"
"#;
        let m: Manifest = toml::from_str(toml_str).unwrap();
        assert_eq!(m.keybindings.len(), 1);
        assert_eq!(m.keybindings[0].scope, "global");
        assert!(m.permissions.can_register_global_bindings());
    }

    #[test]
    fn write_keybindings_fragment_writes_and_can_be_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYBINDINGS_DIR", dir.path());

        let m = manifest_with_bindings(
            "com.test.kb",
            true,
            vec![KeybindingEntry {
                id: "open".into(),
                label: "Open".into(),
                default_binding: "Super+O".into(),
                action: None,
                description: None,
                scope: "global".into(),
            }],
        );
        let path = write_keybindings_fragment(&m).unwrap().unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Super+O"));
        assert!(content.contains("module:com.test.kb:open"));

        remove_keybindings_fragment("com.test.kb").unwrap();
        assert!(!path.exists());

        std::env::remove_var("LUNARIS_USER_KEYBINDINGS_DIR");
    }

    #[test]
    fn write_keybindings_fragment_skips_global_without_permission() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYBINDINGS_DIR", dir.path());

        let m = manifest_with_bindings(
            "com.untrusted",
            false, // no permission
            vec![
                KeybindingEntry {
                    id: "global_action".into(),
                    label: "Global".into(),
                    default_binding: "Super+G".into(),
                    action: None,
                    description: None,
                    scope: "global".into(),
                },
                KeybindingEntry {
                    id: "focused_action".into(),
                    label: "Focused".into(),
                    default_binding: "Ctrl+F".into(),
                    action: None,
                    description: None,
                    scope: "focused".into(),
                },
            ],
        );
        let path = write_keybindings_fragment(&m).unwrap().unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        // Global binding dropped, focused kept.
        assert!(!content.contains("Super+G"));
        assert!(content.contains("Ctrl+F"));

        std::env::remove_var("LUNARIS_USER_KEYBINDINGS_DIR");
    }

    #[test]
    fn write_keybindings_fragment_no_bindings_is_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYBINDINGS_DIR", dir.path());

        let m = manifest_with_bindings("com.noop", true, Vec::new());
        assert!(write_keybindings_fragment(&m).unwrap().is_none());
        assert!(!dir.path().join("com.noop.toml").exists());

        std::env::remove_var("LUNARIS_USER_KEYBINDINGS_DIR");
    }

    #[test]
    fn write_keybindings_fragment_all_filtered_is_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYBINDINGS_DIR", dir.path());

        let m = manifest_with_bindings(
            "com.untrusted.global_only",
            false,
            vec![KeybindingEntry {
                id: "only_global".into(),
                label: "Global".into(),
                default_binding: "Super+G".into(),
                action: None,
                description: None,
                scope: "global".into(),
            }],
        );
        // Every entry filtered out → no file should be written.
        assert!(write_keybindings_fragment(&m).unwrap().is_none());

        std::env::remove_var("LUNARIS_USER_KEYBINDINGS_DIR");
    }

    #[test]
    fn remove_keybindings_fragment_missing_is_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYBINDINGS_DIR", dir.path());
        remove_keybindings_fragment("com.does-not-exist").unwrap();
        std::env::remove_var("LUNARIS_USER_KEYBINDINGS_DIR");
    }

    #[test]
    fn keybinding_entry_effective_action_respects_override() {
        let kb = KeybindingEntry {
            id: "x".into(),
            label: "X".into(),
            default_binding: "Super+X".into(),
            action: Some("spawn:foot".into()),
            description: None,
            scope: "global".into(),
        };
        assert_eq!(kb.effective_action("anything"), "spawn:foot");
    }
}
