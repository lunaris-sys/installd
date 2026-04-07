/// Flatpak integration for the install daemon.
///
/// Installs, uninstalls, and lists Flatpak applications via the `flatpak`
/// CLI. After installation, a default Lunaris permission profile is created
/// so the app participates in the Knowledge Graph and Event Bus permission
/// system alongside native .lunpkg apps.

use std::process::Command;

use thiserror::Error;

/// Errors from Flatpak operations.
#[derive(Debug, Error)]
pub enum FlatpakError {
    #[error("flatpak command not found")]
    NotFound,
    #[error("flatpak install failed: {0}")]
    InstallFailed(String),
    #[error("flatpak uninstall failed: {0}")]
    UninstallFailed(String),
    #[error("flatpak info failed: {0}")]
    InfoFailed(String),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

/// Metadata about an installed Flatpak app.
#[derive(Debug, Clone)]
pub struct FlatpakInfo {
    pub app_id: String,
    pub name: String,
    pub version: String,
}

/// Install a Flatpak app for the current user.
///
/// Uses `flatpak install --user --noninteractive`. The `remote` defaults
/// to "flathub" if empty.
pub fn install_flatpak(app_id: &str, remote: &str) -> Result<(), FlatpakError> {
    check_flatpak_available()?;

    let remote = if remote.is_empty() { "flathub" } else { remote };

    let output = Command::new("flatpak")
        .args(["install", "--user", "--noninteractive", remote, app_id])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(FlatpakError::InstallFailed(stderr));
    }

    tracing::info!("flatpak: installed {app_id} from {remote}");
    Ok(())
}

/// Uninstall a Flatpak app for the current user.
pub fn uninstall_flatpak(app_id: &str) -> Result<(), FlatpakError> {
    check_flatpak_available()?;

    let output = Command::new("flatpak")
        .args(["uninstall", "--user", "--noninteractive", app_id])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(FlatpakError::UninstallFailed(stderr));
    }

    tracing::info!("flatpak: uninstalled {app_id}");
    Ok(())
}

/// Get metadata for an installed Flatpak app.
pub fn get_flatpak_info(app_id: &str) -> Result<FlatpakInfo, FlatpakError> {
    let output = Command::new("flatpak")
        .args(["info", "--user", app_id])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(FlatpakError::InfoFailed(stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut name = String::new();
    let mut version = String::new();

    for line in stdout.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("Name:") {
            name = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("Version:") {
            version = val.trim().to_string();
        }
    }

    Ok(FlatpakInfo {
        app_id: app_id.to_string(),
        name,
        version,
    })
}

/// List all user-installed Flatpak applications.
///
/// Returns `Vec<(app_id, name, version, "flatpak")>`.
pub fn list_installed_flatpaks() -> Vec<(String, String, String, String)> {
    let output = match Command::new("flatpak")
        .args([
            "list",
            "--user",
            "--app",
            "--columns=application,name,version",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut apps = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split('\t').collect();
        let app_id = parts.first().unwrap_or(&"").to_string();
        let name = parts.get(1).unwrap_or(&"").to_string();
        let version = parts.get(2).unwrap_or(&"").to_string();

        if !app_id.is_empty() {
            apps.push((app_id, name, version, "flatpak".into()));
        }
    }

    apps
}

/// Generate a default Lunaris permission profile TOML for a Flatpak app.
///
/// Flatpak apps get a conservative default profile. The actual sandbox
/// enforcement comes from Flatpak itself; this profile controls
/// Knowledge Graph and Event Bus access.
pub fn default_permission_profile(app_id: &str) -> String {
    format!(
        r#"[info]
app_id = "{app_id}"
tier = "third-party"

[graph]
read = ["{app_id}.*"]
write = ["{app_id}.*"]

[event_bus]
subscribe = ["system.theme.*"]
publish = ["{app_id}.*"]

[filesystem]
documents = false
downloads = false

[network]
domains = []

[capabilities]
notifications = true
clipboard = false
autostart = false
background = false
"#
    )
}

/// Check that flatpak CLI is available.
fn check_flatpak_available() -> Result<(), FlatpakError> {
    match Command::new("flatpak").arg("--version").output() {
        Ok(o) if o.status.success() => Ok(()),
        _ => Err(FlatpakError::NotFound),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_permission_profile() {
        let profile = default_permission_profile("org.gnome.Calculator");
        assert!(profile.contains("app_id = \"org.gnome.Calculator\""));
        assert!(profile.contains("tier = \"third-party\""));
        assert!(profile.contains("org.gnome.Calculator.*"));

        // Validate it parses as TOML.
        let parsed: toml::Value = toml::from_str(&profile).unwrap();
        assert!(parsed.get("info").is_some());
        assert!(parsed.get("graph").is_some());
    }

    #[test]
    fn test_list_installed_flatpaks_no_flatpak() {
        // If flatpak is not installed or has no user apps, returns empty.
        // This test validates the graceful fallback.
        let apps = list_installed_flatpaks();
        // We can't assert the exact count (depends on system), but it
        // should not panic.
        assert!(apps.iter().all(|(_, _, _, src)| src == "flatpak"));
    }

    #[test]
    fn test_parse_flatpak_list_output() {
        // Simulate parsing the tab-separated output format.
        let line = "org.gnome.Calculator\tCalculator\t46.1";
        let parts: Vec<&str> = line.split('\t').collect();
        assert_eq!(parts[0], "org.gnome.Calculator");
        assert_eq!(parts[1], "Calculator");
        assert_eq!(parts[2], "46.1");
    }
}
