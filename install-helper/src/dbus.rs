/// D-Bus interface for the install helper.
///
/// Interface: org.lunaris.InstallHelper1
/// Object path: /org/lunaris/InstallHelper1
///
/// Only lunaris-installd may invoke methods. Caller identity is verified
/// via /proc/{pid}/exe.

use zbus::{interface, Connection};

use crate::install;

/// Allowed caller binaries (resolved from /proc/{pid}/exe).
const ALLOWED_CALLERS: &[&str] = &[
    "lunaris-installd",
    "lunaris-install-helper", // self-test
];

/// D-Bus interface implementation.
pub struct InstallHelper;

#[interface(name = "org.lunaris.InstallHelper1")]
impl InstallHelper {
    /// Install an app to the system-wide location.
    ///
    /// Copies the prepared directory at `source_path` to
    /// `/usr/lib/lunaris/apps/{app_id}/`. The source directory must
    /// contain the app structure (bin/, lib/, share/).
    ///
    /// Returns (success, error_message).
    async fn install_system(
        &self,
        app_id: &str,
        source_path: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> (bool, String) {
        if let Err(e) = validate_caller(&header, connection).await {
            return (false, e);
        }

        match install::install_system(app_id, source_path) {
            Ok(path) => {
                tracing::info!("installed {app_id} at {}", path.display());
                (true, String::new())
            }
            Err(e) => {
                tracing::warn!("install_system failed for {app_id}: {e}");
                (false, e.to_string())
            }
        }
    }

    /// Uninstall a system-wide app.
    ///
    /// Removes `/usr/lib/lunaris/apps/{app_id}/` and any system desktop
    /// entry for the app.
    ///
    /// Returns (success, error_message).
    async fn uninstall_system(
        &self,
        app_id: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> (bool, String) {
        if let Err(e) = validate_caller(&header, connection).await {
            return (false, e);
        }

        match install::uninstall_system(app_id) {
            Ok(()) => {
                tracing::info!("uninstalled {app_id}");
                (true, String::new())
            }
            Err(e) => {
                tracing::warn!("uninstall_system failed for {app_id}: {e}");
                (false, e.to_string())
            }
        }
    }

    /// Write a desktop entry to /usr/share/applications/.
    ///
    /// `entry_content` must be valid desktop entry format. The file is
    /// named `{app_id}.desktop`.
    ///
    /// Returns (success, error_message).
    async fn create_desktop_entry(
        &self,
        app_id: &str,
        entry_content: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> (bool, String) {
        if let Err(e) = validate_caller(&header, connection).await {
            return (false, e);
        }

        match install::create_desktop_entry(app_id, entry_content) {
            Ok(path) => {
                tracing::info!("desktop entry for {app_id} at {}", path.display());
                (true, String::new())
            }
            Err(e) => {
                tracing::warn!("create_desktop_entry failed for {app_id}: {e}");
                (false, e.to_string())
            }
        }
    }

    /// Check if a system-wide app is installed.
    async fn is_installed(&self, app_id: &str) -> bool {
        install::validate_app_id(app_id).is_ok() && {
            let base = std::env::var("LUNARIS_SYSTEM_APPS_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/usr/lib/lunaris/apps"));
            base.join(app_id).exists()
        }
    }
}

/// Validate that the D-Bus caller is an authorized process.
async fn validate_caller(
    header: &zbus::message::Header<'_>,
    connection: &Connection,
) -> Result<(), String> {
    let sender = header
        .sender()
        .ok_or_else(|| "no sender in message".to_string())?;

    let proxy = zbus::fdo::DBusProxy::new(connection)
        .await
        .map_err(|e| format!("DBusProxy: {e}"))?;

    let pid = proxy
        .get_connection_unix_process_id(sender.clone().into())
        .await
        .map_err(|e| format!("get PID: {e}"))?;

    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .map_err(|e| format!("read exe: {e}"))?;

    let exe_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if !ALLOWED_CALLERS.iter().any(|c| exe_name.contains(c)) {
        return Err(format!(
            "unauthorized caller: {exe_name} (pid {pid})"
        ));
    }

    Ok(())
}
