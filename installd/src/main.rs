/// Lunaris Install Daemon -- user-level D-Bus service.
///
/// Provides `org.lunaris.InstallDaemon1` on the session bus. Handles
/// `.lunpkg` installation, uninstallation, and app listing. Delegates
/// privileged operations (system-wide installs) to `install-helper`
/// via the system bus.
///
/// See `docs/architecture/install-daemon.md`.

mod dbus;
mod flatpak;
mod install;
mod jobs;
mod signature;
mod transaction;
mod trash;

use std::sync::Arc;

use zbus::connection;

use jobs::JobQueue;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("lunaris_installd=info".parse()?),
        )
        .init();

    tracing::info!("starting installd");

    // Run trash cleanup on startup.
    let cleaned = trash::cleanup_trash();
    if cleaned > 0 {
        tracing::info!("startup: cleaned {cleaned} expired trash entries");
    }

    let job_queue = Arc::new(JobQueue::new());
    let daemon = dbus::InstallDaemon::new(job_queue.clone());

    let conn = connection::Builder::session()?
        .name("org.lunaris.InstallDaemon1")?
        .serve_at("/org/lunaris/InstallDaemon1", daemon)?
        .build()
        .await?;

    // Start the job worker.
    let worker_queue = job_queue.clone();
    let worker_conn = conn.clone();
    tokio::spawn(async move {
        jobs::run_worker(worker_queue, worker_conn).await;
    });

    tracing::info!("D-Bus service ready on org.lunaris.InstallDaemon1");

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");

    Ok(())
}
