/// D-Bus interface for the permission helper.
///
/// Interface: org.lunaris.PermissionHelper1
/// Object path: /org/lunaris/PermissionHelper1
///
/// Only authorized callers (installd, settings) may invoke methods.

use zbus::{interface, Connection};

use crate::profile;

/// Allowed caller binaries (resolved from /proc/{pid}/exe).
const ALLOWED_CALLERS: &[&str] = &[
    "lunaris-installd",
    "lunaris-settings",
    "lunaris-permission-helper", // self-test
];

/// D-Bus interface implementation.
pub struct PermissionHelper;

#[interface(name = "org.lunaris.PermissionHelper1")]
impl PermissionHelper {
    /// Write a permission profile for an app.
    async fn write_profile(
        &self,
        app_id: &str,
        uid: u32,
        profile_toml: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> (bool, String) {
        // Validate caller.
        if let Err(e) = validate_caller(&header, connection).await {
            return (false, e);
        }

        match profile::write_profile(uid, app_id, profile_toml) {
            Ok(path) => {
                tracing::info!("wrote profile for {app_id} (uid {uid}) at {}", path.display());
                (true, String::new())
            }
            Err(e) => {
                tracing::warn!("write_profile failed for {app_id}: {e}");
                (false, e.to_string())
            }
        }
    }

    /// Delete a permission profile for an app.
    async fn delete_profile(
        &self,
        app_id: &str,
        uid: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> (bool, String) {
        if let Err(e) = validate_caller(&header, connection).await {
            return (false, e);
        }

        match profile::delete_profile(uid, app_id) {
            Ok(()) => {
                tracing::info!("deleted profile for {app_id} (uid {uid})");
                (true, String::new())
            }
            Err(e) => {
                tracing::warn!("delete_profile failed for {app_id}: {e}");
                (false, e.to_string())
            }
        }
    }

    /// Check if a profile exists for an app.
    async fn profile_exists(&self, app_id: &str, uid: u32) -> bool {
        profile::profile_exists(uid, app_id)
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

    // Get the caller's PID via D-Bus.
    let proxy = zbus::fdo::DBusProxy::new(connection)
        .await
        .map_err(|e| format!("DBusProxy: {e}"))?;

    let pid = proxy
        .get_connection_unix_process_id(sender.clone().into())
        .await
        .map_err(|e| format!("get PID: {e}"))?;

    // Read /proc/{pid}/exe to check the binary.
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
