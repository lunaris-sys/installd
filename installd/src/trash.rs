/// 30-day staged deletion for uninstalled apps.
///
/// Instead of permanently deleting app data, `stage_for_deletion` moves
/// the app directory to `~/.local/share/lunaris/.trash/{app_id}/` with
/// a metadata file recording when it was deleted. After 30 days,
/// `cleanup_trash` permanently removes expired entries.
///
/// `restore_app` moves a trashed app back and recreates the desktop entry.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::install::{self, InstallError, Manifest};

/// Grace period before permanent deletion.
const GRACE_PERIOD_DAYS: u64 = 30;

/// Errors from trash operations.
#[derive(Debug, Error)]
pub enum TrashError {
    #[error("app not found in trash: {0}")]
    NotInTrash(String),
    #[error("app already installed: {0}")]
    AlreadyInstalled(String),
    #[error("invalid trash entry: {0}")]
    InvalidEntry(String),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("install: {0}")]
    Install(#[from] InstallError),
}

/// Metadata stored alongside each trashed app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashInfo {
    /// The app's reverse-domain ID.
    pub app_id: String,
    /// Display name for UI.
    pub app_name: String,
    /// Version at time of uninstall.
    pub app_version: String,
    /// ISO 8601 timestamp of deletion.
    pub deleted_at: String,
    /// Original install path.
    pub original_path: String,
}

/// Get the trash directory.
fn trash_dir() -> PathBuf {
    std::env::var("LUNARIS_TRASH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("~/.local/share"))
                .join("lunaris/.trash")
        })
}

/// Stage an app for deletion (move to trash).
///
/// Loads the manifest for metadata, removes schemas and modules,
/// moves the app directory to trash, and writes trash-info.
pub fn stage_for_deletion(app_id: &str) -> Result<TrashInfo, TrashError> {
    install::validate_app_id(app_id)?;

    let app_dir = install::user_apps_dir_pub().join(app_id);
    if !app_dir.exists() {
        return Err(TrashError::Install(InstallError::NotInstalled(
            app_id.into(),
        )));
    }

    // Load manifest for cleanup and metadata.
    let manifest = install::load_manifest(&app_dir).ok();
    let app_name = manifest
        .as_ref()
        .map(|m| m.package.name.clone())
        .unwrap_or_default();
    let app_version = manifest
        .as_ref()
        .map(|m| m.package.version.clone())
        .unwrap_or_default();

    // Remove schemas and modules (they are not moved to trash).
    if let Some(ref m) = manifest {
        let _ = install::remove_schemas(m);
        let _ = install::remove_modules(m);
    }

    // Move app directory to trash.
    let trash = trash_dir();
    let trash_entry = trash.join(app_id);
    if trash_entry.exists() {
        // Previous trash entry exists (re-uninstall). Remove it.
        fs::remove_dir_all(&trash_entry)?;
    }
    fs::create_dir_all(&trash)?;
    fs::rename(&app_dir, &trash_entry)?;

    // Write trash info.
    let now = now_iso8601();
    let info = TrashInfo {
        app_id: app_id.to_string(),
        app_name,
        app_version,
        deleted_at: now,
        original_path: app_dir.to_string_lossy().to_string(),
    };
    let info_json = serde_json::to_string_pretty(&info)
        .unwrap_or_else(|_| "{}".to_string());
    fs::write(trash_entry.join(".trash-info"), &info_json)?;

    tracing::info!("staged {app_id} for deletion (30-day grace period)");
    Ok(info)
}

/// Restore an app from trash.
///
/// Moves the app directory back to the install location and recreates
/// the desktop entry from the manifest.
pub fn restore_app(app_id: &str) -> Result<(), TrashError> {
    install::validate_app_id(app_id)?;

    let trash_entry = trash_dir().join(app_id);
    if !trash_entry.exists() {
        return Err(TrashError::NotInTrash(app_id.into()));
    }

    let dest = install::user_apps_dir_pub().join(app_id);
    if dest.exists() {
        return Err(TrashError::AlreadyInstalled(app_id.into()));
    }

    // Move back.
    fs::rename(&trash_entry, &dest)?;

    // Remove the trash info file (it's now inside the app dir).
    let info_file = dest.join(".trash-info");
    if info_file.exists() {
        let _ = fs::remove_file(&info_file);
    }

    // Recreate desktop entry from manifest.
    if let Ok(manifest) = install::load_manifest(&dest) {
        let _ = install::create_desktop_entry(&manifest);
    }

    tracing::info!("restored {app_id} from trash");
    Ok(())
}

/// List all apps currently in trash.
pub fn list_trashed() -> Vec<TrashInfo> {
    let trash = trash_dir();
    let mut entries = Vec::new();

    let Ok(dir) = fs::read_dir(&trash) else {
        return entries;
    };

    for entry in dir.flatten() {
        let info_path = entry.path().join(".trash-info");
        if let Ok(content) = fs::read_to_string(&info_path) {
            if let Ok(info) = serde_json::from_str::<TrashInfo>(&content) {
                entries.push(info);
            }
        }
    }

    entries
}

/// Clean up expired trash entries (older than 30 days).
///
/// Returns the number of entries permanently deleted.
pub fn cleanup_trash() -> usize {
    let trash = trash_dir();
    let mut deleted = 0;

    let Ok(dir) = fs::read_dir(&trash) else {
        return 0;
    };

    for entry in dir.flatten() {
        let info_path = entry.path().join(".trash-info");
        let should_delete = match fs::read_to_string(&info_path) {
            Ok(content) => match serde_json::from_str::<TrashInfo>(&content) {
                Ok(info) => is_expired(&info.deleted_at),
                Err(_) => {
                    // Invalid trash-info: delete as a safety measure.
                    tracing::warn!(
                        "invalid .trash-info in {}, deleting",
                        entry.path().display()
                    );
                    true
                }
            },
            Err(_) => {
                // No .trash-info: orphaned entry.
                tracing::warn!(
                    "no .trash-info in {}, deleting",
                    entry.path().display()
                );
                true
            }
        };

        if should_delete {
            let app_id = entry.file_name().to_string_lossy().to_string();
            if fs::remove_dir_all(entry.path()).is_ok() {
                tracing::info!("permanently deleted {app_id} from trash");
                deleted += 1;
            }
        }
    }

    if deleted > 0 {
        tracing::info!("trash cleanup: deleted {deleted} expired entries");
    }

    deleted
}

/// Check if a deletion timestamp is older than the grace period.
fn is_expired(deleted_at: &str) -> bool {
    let Ok(deleted) = parse_iso8601(deleted_at) else {
        return true; // Can't parse -> expired (safe default).
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let grace = Duration::from_secs(GRACE_PERIOD_DAYS * 24 * 60 * 60).as_secs();
    now.saturating_sub(deleted) >= grace
}

/// Get current time as ISO 8601 string (UTC).
fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_timestamp(secs)
}

/// Format a Unix timestamp as ISO 8601.
fn format_timestamp(secs: u64) -> String {
    // Simple formatting without chrono dependency.
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let s = time_secs % 60;

    // Days since epoch to Y-M-D (simplified leap year handling).
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Parse a subset of ISO 8601 to Unix timestamp.
fn parse_iso8601(s: &str) -> Result<u64, ()> {
    // Expected: "YYYY-MM-DDThh:mm:ssZ"
    if s.len() < 19 {
        return Err(());
    }
    let y: u64 = s[0..4].parse().map_err(|_| ())?;
    let mo: u64 = s[5..7].parse().map_err(|_| ())?;
    let d: u64 = s[8..10].parse().map_err(|_| ())?;
    let h: u64 = s[11..13].parse().map_err(|_| ())?;
    let m: u64 = s[14..16].parse().map_err(|_| ())?;
    let sec: u64 = s[17..19].parse().map_err(|_| ())?;

    let days = ymd_to_days(y, mo, d);
    Ok(days * 86400 + h * 3600 + m * 60 + sec)
}

/// Convert days since epoch to (year, month, day).
fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut y = 1970;
    loop {
        let dy = if is_leap(y) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let months: [u64; 12] = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut mo = 1;
    for &ml in &months {
        if days < ml {
            break;
        }
        days -= ml;
        mo += 1;
    }
    (y, mo, days + 1)
}

/// Convert (year, month, day) to days since epoch.
fn ymd_to_days(y: u64, mo: u64, d: u64) -> u64 {
    let mut days = 0;
    for yr in 1970..y {
        days += if is_leap(yr) { 366 } else { 365 };
    }
    let months: [u64; 12] = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    for i in 0..(mo.saturating_sub(1) as usize).min(11) {
        days += months[i];
    }
    days + d.saturating_sub(1)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stage_and_restore() {
        let apps = tempfile::TempDir::new().unwrap();
        let trash = tempfile::TempDir::new().unwrap();
        let desktop = tempfile::TempDir::new().unwrap();

        std::env::set_var("LUNARIS_USER_APPS_DIR", apps.path());
        std::env::set_var("LUNARIS_TRASH_DIR", trash.path());
        std::env::set_var("LUNARIS_USER_DESKTOP_DIR", desktop.path());

        // Create a minimal installed app.
        let app_dir = apps.path().join("com.test.trash");
        fs::create_dir_all(app_dir.join("bin")).unwrap();
        fs::write(app_dir.join("bin/app"), "#!/bin/sh").unwrap();
        fs::write(
            app_dir.join("manifest.toml"),
            "[package]\nid=\"com.test.trash\"\nname=\"Trash Test\"\nversion=\"1.0\"\n[binary]\npath=\"bin/app\"\n",
        )
        .unwrap();

        // Stage for deletion.
        let info = stage_for_deletion("com.test.trash").unwrap();
        assert_eq!(info.app_id, "com.test.trash");
        assert_eq!(info.app_name, "Trash Test");
        assert!(!app_dir.exists(), "app dir should be moved to trash");
        assert!(
            trash.path().join("com.test.trash/.trash-info").exists(),
            "trash info should exist"
        );

        // List trashed.
        let trashed = list_trashed();
        assert_eq!(trashed.len(), 1);
        assert_eq!(trashed[0].app_id, "com.test.trash");

        // Restore.
        restore_app("com.test.trash").unwrap();
        assert!(app_dir.exists(), "app dir should be restored");
        assert!(
            !trash.path().join("com.test.trash").exists(),
            "trash entry should be gone"
        );

        std::env::remove_var("LUNARIS_USER_APPS_DIR");
        std::env::remove_var("LUNARIS_TRASH_DIR");
        std::env::remove_var("LUNARIS_USER_DESKTOP_DIR");
    }

    #[test]
    fn test_restore_not_in_trash() {
        let trash = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_TRASH_DIR", trash.path());

        assert!(restore_app("com.nonexistent.app").is_err());

        std::env::remove_var("LUNARIS_TRASH_DIR");
    }

    #[test]
    fn test_cleanup_expired() {
        let trash = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_TRASH_DIR", trash.path());

        // Create an expired entry (timestamp 31 days ago).
        let entry_dir = trash.path().join("com.test.old");
        fs::create_dir_all(&entry_dir).unwrap();
        let old_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - (31 * 24 * 60 * 60);
        let info = TrashInfo {
            app_id: "com.test.old".into(),
            app_name: "Old".into(),
            app_version: "1.0".into(),
            deleted_at: format_timestamp(old_ts),
            original_path: "/old/path".into(),
        };
        fs::write(
            entry_dir.join(".trash-info"),
            serde_json::to_string(&info).unwrap(),
        )
        .unwrap();

        // Create a recent entry (should NOT be deleted).
        let recent_dir = trash.path().join("com.test.recent");
        fs::create_dir_all(&recent_dir).unwrap();
        let recent_info = TrashInfo {
            app_id: "com.test.recent".into(),
            app_name: "Recent".into(),
            app_version: "1.0".into(),
            deleted_at: now_iso8601(),
            original_path: "/recent/path".into(),
        };
        fs::write(
            recent_dir.join(".trash-info"),
            serde_json::to_string(&recent_info).unwrap(),
        )
        .unwrap();

        let deleted = cleanup_trash();
        assert_eq!(deleted, 1);
        assert!(!entry_dir.exists(), "expired entry should be deleted");
        assert!(recent_dir.exists(), "recent entry should survive");

        std::env::remove_var("LUNARIS_TRASH_DIR");
    }

    #[test]
    fn test_cleanup_orphaned_entry() {
        let trash = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_TRASH_DIR", trash.path());

        // Entry without .trash-info (orphaned).
        let orphan = trash.path().join("com.test.orphan");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("some-file"), "data").unwrap();

        let deleted = cleanup_trash();
        assert_eq!(deleted, 1);
        assert!(!orphan.exists());

        std::env::remove_var("LUNARIS_TRASH_DIR");
    }

    #[test]
    fn test_iso8601_roundtrip() {
        let ts = 1777777777u64; // 2026-05-02
        let formatted = format_timestamp(ts);
        let parsed = parse_iso8601(&formatted).unwrap();
        assert_eq!(ts, parsed);
    }

    #[test]
    fn test_is_expired() {
        // 31 days ago.
        let old = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - (31 * 86400);
        assert!(is_expired(&format_timestamp(old)));

        // Now.
        assert!(!is_expired(&now_iso8601()));
    }
}
