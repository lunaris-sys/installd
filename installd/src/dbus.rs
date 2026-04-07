/// D-Bus interface for the install daemon.
///
/// Interface: org.lunaris.InstallDaemon1
/// Object path: /org/lunaris/InstallDaemon1
/// Bus: Session Bus

use std::sync::Arc;

use zbus::interface;

use crate::install;
use crate::jobs::{JobKind, JobQueue};

/// D-Bus interface implementation.
pub struct InstallDaemon {
    queue: Arc<JobQueue>,
}

impl InstallDaemon {
    /// Create a new daemon with the given job queue.
    pub fn new(queue: Arc<JobQueue>) -> Self {
        Self { queue }
    }
}

#[interface(name = "org.lunaris.InstallDaemon1")]
impl InstallDaemon {
    /// Install a .lunpkg package from a local file path.
    ///
    /// Returns a job_id that can be used to track progress via signals.
    async fn install_package(&self, path: String) -> String {
        let job_id = self.queue.enqueue(JobKind::InstallPackage { path });
        tracing::info!("enqueued install job {job_id}");
        job_id
    }

    /// Install a Flatpak app.
    ///
    /// `remote` defaults to "flathub" if empty. Returns a job_id.
    async fn install_flatpak(&self, app_id: String, remote: String) -> String {
        let job_id = self.queue.enqueue(JobKind::InstallFlatpak { app_id, remote });
        tracing::info!("enqueued flatpak install job {job_id}");
        job_id
    }

    /// Uninstall an app by app_id (.lunpkg source).
    ///
    /// Returns a job_id.
    async fn uninstall(&self, app_id: String) -> String {
        let job_id = self.queue.enqueue(JobKind::Uninstall { app_id });
        tracing::info!("enqueued uninstall job {job_id}");
        job_id
    }

    /// Uninstall a Flatpak app.
    ///
    /// Returns a job_id.
    async fn uninstall_flatpak(&self, app_id: String) -> String {
        let job_id = self.queue.enqueue(JobKind::UninstallFlatpak { app_id });
        tracing::info!("enqueued flatpak uninstall job {job_id}");
        job_id
    }

    /// List all installed apps (lunpkg + flatpak combined).
    ///
    /// Returns an array of (app_id, name, version, source).
    /// Source is "lunpkg", "flatpak", or "unknown".
    async fn list_installed(&self) -> Vec<(String, String, String, String)> {
        let mut apps = install::list_installed();
        apps.extend(crate::flatpak::list_installed_flatpaks());
        apps
    }

    /// Restore a previously uninstalled app from the 30-day trash.
    ///
    /// Returns (success, error_message).
    async fn restore_app(&self, app_id: String) -> (bool, String) {
        match crate::trash::restore_app(&app_id) {
            Ok(()) => {
                tracing::info!("restored {app_id} from trash");
                (true, String::new())
            }
            Err(e) => {
                tracing::warn!("restore failed for {app_id}: {e}");
                (false, e.to_string())
            }
        }
    }

    /// List all apps in the 30-day trash.
    ///
    /// Returns an array of (app_id, app_name, app_version, deleted_at).
    async fn list_trashed(&self) -> Vec<(String, String, String, String)> {
        crate::trash::list_trashed()
            .into_iter()
            .map(|info| (info.app_id, info.app_name, info.app_version, info.deleted_at))
            .collect()
    }

    /// Run trash cleanup (remove entries older than 30 days).
    ///
    /// Called by the systemd timer and on daemon startup. Returns the
    /// number of entries permanently deleted.
    async fn cleanup_trash(&self) -> u32 {
        crate::trash::cleanup_trash() as u32
    }

    /// Get the current status of a job.
    ///
    /// Returns (progress: u8, state: String, status_message: String).
    /// State is one of: "pending", "running", "completed", "failed", "cancelled".
    /// Returns ("0", "unknown", "") if the job_id is not found.
    async fn get_job_status(&self, job_id: String) -> (u8, String, String) {
        self.queue
            .get_status(&job_id)
            .unwrap_or((0, "unknown".into(), String::new()))
    }

    // ── Signals ──────────────────────────────────────────────────────────

    /// Emitted when a job makes progress.
    #[zbus(signal)]
    pub async fn job_progress(
        signal_ctxt: &zbus::object_server::SignalEmitter<'_>,
        job_id: String,
        percent: u32,
        status: String,
    ) -> zbus::Result<()>;

    /// Emitted when a job completes (successfully or with error).
    #[zbus(signal)]
    pub async fn job_completed(
        signal_ctxt: &zbus::object_server::SignalEmitter<'_>,
        job_id: String,
        success: bool,
        error: String,
    ) -> zbus::Result<()>;

    /// Emitted when user consent is required for permissions.
    #[zbus(signal)]
    pub async fn consent_required(
        signal_ctxt: &zbus::object_server::SignalEmitter<'_>,
        job_id: String,
        app_id: String,
        app_name: String,
        permissions: Vec<String>,
    ) -> zbus::Result<()>;
}
