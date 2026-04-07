/// Lunaris Permission Helper -- root D-Bus service.
///
/// Provides `org.lunaris.PermissionHelper1` for writing permission profiles
/// to `/var/lib/lunaris/permissions/`. Only authorized callers (installd,
/// settings) may invoke methods.
///
/// See `docs/architecture/permission-system.md`.

mod dbus;
mod profile;

use zbus::connection;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("lunaris_permission_helper=info".parse()?),
        )
        .init();

    tracing::info!("starting permission helper");

    let helper = dbus::PermissionHelper;

    let _conn = connection::Builder::system()?
        .name("org.lunaris.PermissionHelper1")?
        .serve_at("/org/lunaris/PermissionHelper1", helper)?
        .build()
        .await?;

    tracing::info!("D-Bus service ready on org.lunaris.PermissionHelper1");

    // Run until SIGTERM.
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");

    Ok(())
}
