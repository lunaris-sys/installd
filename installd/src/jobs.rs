/// Job queue for serialized install/uninstall operations.
///
/// Each method on the D-Bus interface creates a Job and enqueues it.
/// A single worker task processes jobs sequentially to avoid
/// concurrent filesystem mutations.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::mpsc;
use uuid::Uuid;
use zbus::Connection;

use crate::install;

/// Job types supported by the daemon.
#[derive(Debug, Clone)]
pub enum JobKind {
    /// Install from a local .lunpkg file.
    InstallPackage { path: String },
    /// Install a Flatpak app.
    InstallFlatpak { app_id: String, remote: String },
    /// Uninstall an app by app_id (auto-detects source).
    Uninstall { app_id: String },
    /// Uninstall a Flatpak app.
    UninstallFlatpak { app_id: String },
}

/// Current state of a job.
#[derive(Debug, Clone, Serialize)]
pub enum JobState {
    /// Queued, waiting for worker.
    Pending,
    /// Currently executing.
    Running,
    /// Finished successfully.
    Completed,
    /// Failed with error.
    Failed { error: String },
    /// Cancelled by user.
    Cancelled,
}

/// A tracked install/uninstall job.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub kind: JobKind,
    pub state: JobState,
    pub progress: u8,
    pub status: String,
}

/// Serialized job queue.
///
/// Jobs are submitted via `enqueue()` and processed by `run_worker()`.
pub struct JobQueue {
    sender: mpsc::UnboundedSender<Job>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<Job>>>,
    /// Tracks job state for GetJobStatus.
    pub jobs: Mutex<HashMap<String, Job>>,
}

impl JobQueue {
    /// Create a new job queue.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            sender: tx,
            receiver: Mutex::new(Some(rx)),
            jobs: Mutex::new(HashMap::new()),
        }
    }

    /// Take the receiver (called once by the worker).
    pub fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<Job>> {
        self.receiver.lock().unwrap().take()
    }

    /// Enqueue a new job. Returns the job ID.
    pub fn enqueue(&self, kind: JobKind) -> String {
        let id = Uuid::new_v4().to_string();
        let job = Job {
            id: id.clone(),
            kind,
            state: JobState::Pending,
            progress: 0,
            status: "queued".into(),
        };
        self.jobs.lock().unwrap().insert(id.clone(), job.clone());
        let _ = self.sender.send(job);
        id
    }

    /// Update a job's progress and status.
    pub fn update_progress(&self, job_id: &str, progress: u8, status: &str) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(job_id) {
            job.progress = progress;
            job.status = status.into();
            job.state = JobState::Running;
        }
    }

    /// Mark a job as completed.
    pub fn mark_completed(&self, job_id: &str) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(job_id) {
            job.state = JobState::Completed;
            job.progress = 100;
            job.status = "complete".into();
        }
    }

    /// Mark a job as failed.
    pub fn mark_failed(&self, job_id: &str, error: &str) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(job_id) {
            job.state = JobState::Failed {
                error: error.into(),
            };
            job.status = "failed".into();
        }
    }

    /// Get the current state of a job.
    pub fn get_status(&self, job_id: &str) -> Option<(u8, String, String)> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(job_id)?;
        let state_str = match &job.state {
            JobState::Pending => "pending",
            JobState::Running => "running",
            JobState::Completed => "completed",
            JobState::Failed { .. } => "failed",
            JobState::Cancelled => "cancelled",
        };
        Some((job.progress, state_str.to_string(), job.status.clone()))
    }
}

/// Emit a JobProgress D-Bus signal.
async fn emit_progress(conn: &Connection, job_id: &str, percent: u8, status: &str) {
    let iface_ref = conn
        .object_server()
        .interface::<_, crate::dbus::InstallDaemon>("/org/lunaris/InstallDaemon1")
        .await;
    if let Ok(iface) = iface_ref {
        let ctx = iface.signal_emitter();
        let _ = crate::dbus::InstallDaemon::job_progress(
            ctx,
            job_id.to_string(),
            percent as u32,
            status.to_string(),
        )
        .await;
    }
}

/// Emit a JobCompleted D-Bus signal.
async fn emit_completed(conn: &Connection, job_id: &str, success: bool, error: &str) {
    let iface_ref = conn
        .object_server()
        .interface::<_, crate::dbus::InstallDaemon>("/org/lunaris/InstallDaemon1")
        .await;
    if let Ok(iface) = iface_ref {
        let ctx = iface.signal_emitter();
        let _ = crate::dbus::InstallDaemon::job_completed(
            ctx,
            job_id.to_string(),
            success,
            error.to_string(),
        )
        .await;
    }
}

/// Worker loop: processes jobs sequentially.
pub async fn run_worker(queue: std::sync::Arc<JobQueue>, conn: Connection) {
    let Some(mut rx) = queue.take_receiver() else {
        tracing::error!("job worker: receiver already taken");
        return;
    };

    tracing::info!("job worker started");

    while let Some(job) = rx.recv().await {
        let job_id = job.id.clone();
        tracing::info!("job {job_id}: starting {:?}", job.kind);

        queue.update_progress(&job_id, 5, "starting");
        emit_progress(&conn, &job_id, 5, "starting").await;

        let result = match job.kind {
            JobKind::InstallPackage { ref path } => {
                run_install_package(&queue, &conn, &job_id, path).await
            }
            JobKind::InstallFlatpak {
                ref app_id,
                ref remote,
            } => run_install_flatpak(&queue, &conn, &job_id, app_id, remote).await,
            JobKind::Uninstall { ref app_id } => {
                run_uninstall(&queue, &conn, &job_id, app_id).await
            }
            JobKind::UninstallFlatpak { ref app_id } => {
                run_uninstall_flatpak(&queue, &conn, &job_id, app_id).await
            }
        };

        match result {
            Ok(()) => {
                queue.mark_completed(&job_id);
                emit_completed(&conn, &job_id, true, "").await;
                tracing::info!("job {job_id}: completed");
            }
            Err(e) => {
                let msg = e.to_string();
                queue.mark_failed(&job_id, &msg);
                emit_completed(&conn, &job_id, false, &msg).await;
                tracing::warn!("job {job_id}: failed: {msg}");
            }
        }
    }
}

/// Execute a .lunpkg install job with transactional rollback.
///
/// If any step after extraction fails, the `InstallTransaction` Drop
/// impl rolls back all completed steps automatically.
async fn run_install_package(
    queue: &JobQueue,
    conn: &Connection,
    job_id: &str,
    path: &str,
) -> Result<(), install::InstallError> {
    use crate::transaction::InstallTransaction;

    // 1. Extract.
    queue.update_progress(job_id, 10, "extracting package");
    emit_progress(conn, job_id, 10, "extracting package").await;
    let temp_dir = install::extract_package(path)?;

    // 2. Validate package structure (signature.sig present).
    queue.update_progress(job_id, 15, "validating structure");
    emit_progress(conn, job_id, 15, "validating structure").await;
    install::validate_package_structure(&temp_dir)?;

    // 3. Verify Ed25519 signature.
    queue.update_progress(job_id, 18, "verifying signature");
    emit_progress(conn, job_id, 18, "verifying signature").await;
    crate::signature::verify_signature(&temp_dir).map_err(|e| {
        install::InstallError::SignatureVerificationFailed(e.to_string())
    })?;

    // 4. Load and validate manifest.
    queue.update_progress(job_id, 22, "reading manifest");
    emit_progress(conn, job_id, 22, "reading manifest").await;
    let manifest = install::load_manifest(&temp_dir)?;

    queue.update_progress(job_id, 25, "validating manifest");
    emit_progress(conn, job_id, 25, "validating manifest").await;
    install::validate_manifest(&manifest)?;

    // 5. Begin transaction. From here, any error triggers auto-rollback.
    let mut txn = InstallTransaction::new(temp_dir, manifest);

    // 6. Check disk space (20% buffer).
    queue.update_progress(job_id, 30, "checking disk space");
    emit_progress(conn, job_id, 30, "checking disk space").await;
    txn.check_disk_space()?;

    // 7. Install app files (bin, lib, share).
    queue.update_progress(job_id, 40, "installing files");
    emit_progress(conn, job_id, 40, "installing files").await;
    txn.install_files()?;

    // 8. Install GSettings schemas.
    queue.update_progress(job_id, 55, "installing schemas");
    emit_progress(conn, job_id, 55, "installing schemas").await;
    txn.install_schemas()?;

    // 9. Install bundled modules.
    queue.update_progress(job_id, 65, "installing modules");
    emit_progress(conn, job_id, 65, "installing modules").await;
    txn.install_modules()?;

    // 10. Create desktop entry.
    queue.update_progress(job_id, 80, "creating desktop entry");
    emit_progress(conn, job_id, 80, "creating desktop entry").await;
    txn.create_desktop_entry()?;

    // 11. Commit -- marks transaction as successful, cleans up temp.
    queue.update_progress(job_id, 95, "committing");
    emit_progress(conn, job_id, 95, "committing").await;
    txn.commit();

    Ok(())
}

/// Execute an uninstall job using staged deletion (30-day grace period).
async fn run_uninstall(
    queue: &JobQueue,
    conn: &Connection,
    job_id: &str,
    app_id: &str,
) -> Result<(), install::InstallError> {
    // 1. Stage for deletion (moves app to trash, removes schemas/modules).
    queue.update_progress(job_id, 20, "staging for deletion");
    emit_progress(conn, job_id, 20, "staging for deletion").await;
    crate::trash::stage_for_deletion(app_id).map_err(|e| {
        install::InstallError::TrashFailed(e.to_string())
    })?;

    // 2. Remove desktop entry (app no longer launchable).
    queue.update_progress(job_id, 60, "removing desktop entry");
    emit_progress(conn, job_id, 60, "removing desktop entry").await;
    install::remove_desktop_entry(app_id)?;

    Ok(())
}

/// Execute a Flatpak install job.
async fn run_install_flatpak(
    queue: &JobQueue,
    conn: &Connection,
    job_id: &str,
    app_id: &str,
    remote: &str,
) -> Result<(), install::InstallError> {
    use crate::flatpak;

    // 1. Install via flatpak CLI.
    queue.update_progress(job_id, 20, "installing via flatpak");
    emit_progress(conn, job_id, 20, "installing via flatpak").await;
    flatpak::install_flatpak(app_id, remote).map_err(|e| {
        install::InstallError::FlatpakFailed(e.to_string())
    })?;

    // 2. Create default Lunaris permission profile.
    queue.update_progress(job_id, 70, "creating permission profile");
    emit_progress(conn, job_id, 70, "creating permission profile").await;
    let profile = flatpak::default_permission_profile(app_id);
    let profile_dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
        .join("lunaris/flatpak-profiles");
    let _ = std::fs::create_dir_all(&profile_dir);
    let profile_path = profile_dir.join(format!("{app_id}.toml"));
    std::fs::write(&profile_path, &profile).map_err(install::InstallError::Io)?;
    tracing::info!("wrote Lunaris permission profile for flatpak {app_id}");

    Ok(())
}

/// Execute a Flatpak uninstall job.
async fn run_uninstall_flatpak(
    queue: &JobQueue,
    conn: &Connection,
    job_id: &str,
    app_id: &str,
) -> Result<(), install::InstallError> {
    use crate::flatpak;

    // 1. Uninstall via flatpak CLI.
    queue.update_progress(job_id, 30, "uninstalling via flatpak");
    emit_progress(conn, job_id, 30, "uninstalling via flatpak").await;
    flatpak::uninstall_flatpak(app_id).map_err(|e| {
        install::InstallError::FlatpakFailed(e.to_string())
    })?;

    // 2. Remove Lunaris permission profile.
    queue.update_progress(job_id, 70, "removing permission profile");
    emit_progress(conn, job_id, 70, "removing permission profile").await;
    let profile_path = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
        .join(format!("lunaris/flatpak-profiles/{app_id}.toml"));
    if profile_path.exists() {
        let _ = std::fs::remove_file(&profile_path);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enqueue_and_status() {
        let q = JobQueue::new();
        let id = q.enqueue(JobKind::Uninstall {
            app_id: "com.test".into(),
        });
        let (progress, state, _status) = q.get_status(&id).unwrap();
        assert_eq!(progress, 0);
        assert_eq!(state, "pending");
    }

    #[test]
    fn test_update_progress() {
        let q = JobQueue::new();
        let id = q.enqueue(JobKind::Uninstall {
            app_id: "com.test".into(),
        });
        q.update_progress(&id, 50, "halfway");
        let (progress, state, status) = q.get_status(&id).unwrap();
        assert_eq!(progress, 50);
        assert_eq!(state, "running");
        assert_eq!(status, "halfway");
    }

    #[test]
    fn test_mark_completed() {
        let q = JobQueue::new();
        let id = q.enqueue(JobKind::Uninstall {
            app_id: "com.test".into(),
        });
        q.mark_completed(&id);
        let (progress, state, _) = q.get_status(&id).unwrap();
        assert_eq!(progress, 100);
        assert_eq!(state, "completed");
    }

    #[test]
    fn test_mark_failed() {
        let q = JobQueue::new();
        let id = q.enqueue(JobKind::Uninstall {
            app_id: "com.test".into(),
        });
        q.mark_failed(&id, "disk full");
        let (_, state, _) = q.get_status(&id).unwrap();
        assert_eq!(state, "failed");
    }
}
