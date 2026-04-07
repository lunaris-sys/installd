/// Lunaris Install Helper -- root D-Bus service.
///
/// Provides `org.lunaris.InstallHelper1` for privileged install operations
/// that require root access: copying apps to `/usr/lib/lunaris/apps/`,
/// writing desktop entries to `/usr/share/applications/`, and removing
/// system-wide installations.
///
/// Only authorized callers (lunaris-installd) may invoke methods.
/// Caller identity is verified via `/proc/{pid}/exe`.
///
/// See `docs/architecture/install-daemon.md`.

mod dbus;
mod install;

use zbus::connection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("lunaris_install_helper=info".parse()?),
        )
        .init();

    tracing::info!("starting install helper");

    let helper = dbus::InstallHelper;

    let _conn = connection::Builder::system()?
        .name("org.lunaris.InstallHelper1")?
        .serve_at("/org/lunaris/InstallHelper1", helper)?
        .build()
        .await?;

    tracing::info!("D-Bus service ready on org.lunaris.InstallHelper1");

    // Run until SIGTERM.
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");

    Ok(())
}
