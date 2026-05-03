/// Module permission profile generation.
///
/// When `installd` installs a sandboxed module (Tier 1 WASM or
/// Tier 2 iframe), it derives the runtime `PermissionProfile`
/// (`sdk/permissions`) from the manifest's `[capabilities]` block
/// and writes it to `~/.config/lunaris/permissions/{module_id}.toml`.
/// `lunaris-modulesd` reads that profile when loading the module so
/// the runtime gating matches what the user agreed to at install
/// time.
///
/// The translation is deliberate. The manifest is the *request*
/// (what the module asks for); the permission profile is the
/// *grant* (what the user actually allowed). For the first ship the
/// installer grants whatever the manifest declares, because the
/// `.lunpkg` consent UI is not yet wired up. When that lands the
/// installer can edit the profile before writing it (e.g. strip a
/// `network.allow` entry the user disabled in the consent screen).

use std::fs;
use std::path::PathBuf;

use lunaris_modules::{ModuleCapabilities, ModuleManifest, ModuleType};
use lunaris_permissions::{
    AppTier, ClipboardPermissions, EventBusPermissions, FilesystemPermissions,
    GraphPermissions, NetworkPermissions, NotificationPermissions,
    PermissionProfile, ProfileInfo,
};

use crate::install::InstallError;

/// Foundation §7.3 canonical: `~/.config/permissions/{app_id}.toml`.
/// No `lunaris/` sub-dir (this module previously used the wrong path).
/// `LUNARIS_PERMISSIONS_DIR` test override resolves directly to a flat
/// `<dir>/{app_id}.toml` for test simplicity.
pub fn permissions_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LUNARIS_PERMISSIONS_DIR") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("permissions")
}

pub fn permission_profile_path(module_id: &str) -> PathBuf {
    permissions_dir().join(format!("{module_id}.toml"))
}

/// Convert manifest capabilities into a runtime permission profile.
pub fn profile_from_manifest(manifest: &ModuleManifest) -> PermissionProfile {
    let tier = match manifest.module.module_type {
        ModuleType::System => AppTier::System,
        ModuleType::FirstParty => AppTier::FirstParty,
        ModuleType::ThirdParty => AppTier::ThirdParty,
    };

    PermissionProfile {
        info: ProfileInfo {
            app_id: manifest.module.id.clone(),
            tier,
        },
        graph: graph_from(&manifest.capabilities),
        event_bus: event_bus_from(&manifest.capabilities),
        filesystem: FilesystemPermissions::default(),
        network: network_from(&manifest.capabilities),
        notifications: notifications_from(&manifest.capabilities),
        clipboard: clipboard_from(&manifest.capabilities),
        system: Default::default(),
        input: Default::default(),
    }
}

fn graph_from(caps: &ModuleCapabilities) -> GraphPermissions {
    match &caps.graph {
        Some(g) => GraphPermissions {
            read: g.read.clone(),
            write: g.write.clone(),
            app_isolated: false,
            annotations_read_cross_namespace: Vec::new(),
        },
        None => GraphPermissions::default(),
    }
}

fn event_bus_from(caps: &ModuleCapabilities) -> EventBusPermissions {
    match &caps.event_bus {
        Some(eb) => EventBusPermissions {
            subscribe: eb.subscribe.clone(),
            publish: eb.publish.clone(),
        },
        None => EventBusPermissions::default(),
    }
}

fn network_from(caps: &ModuleCapabilities) -> NetworkPermissions {
    match &caps.network {
        Some(n) => NetworkPermissions {
            allow_all: false,
            allowed_domains: n.allowed_domains.clone(),
        },
        None => NetworkPermissions::default(),
    }
}

fn notifications_from(caps: &ModuleCapabilities) -> NotificationPermissions {
    NotificationPermissions {
        enabled: caps.notifications,
    }
}

fn clipboard_from(caps: &ModuleCapabilities) -> ClipboardPermissions {
    match &caps.clipboard {
        Some(c) => ClipboardPermissions {
            read: c.read,
            write: c.write,
            // Sprint-C-clipboard-extension fields. Default off
            // unless the manifest explicitly grants them; module
            // manifest schema does not yet expose these as
            // user-toggles, so they stay false at install time
            // and must be added to the user's profile by hand
            // (foundation §7.3 — explicit only).
            read_sensitive: false,
            history: false,
        },
        None => ClipboardPermissions::default(),
    }
}

/// Write the derived permission profile to disk. Idempotent: an
/// existing profile is replaced atomically (write-temp + rename).
pub fn write_profile(profile: &PermissionProfile) -> Result<(), InstallError> {
    let dir = permissions_dir();
    fs::create_dir_all(&dir)?;
    let path = permission_profile_path(&profile.info.app_id);
    let toml = toml::to_string(profile).map_err(|e| {
        InstallError::InvalidManifest(format!("permission profile serialise: {e}"))
    })?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, toml)?;
    fs::rename(&tmp, &path)?;
    tracing::info!("installd: wrote permission profile {}", path.display());
    Ok(())
}

/// Remove a module's permission profile. Called from `remove_modules`.
pub fn remove_profile(module_id: &str) -> Result<(), InstallError> {
    let path = permission_profile_path(module_id);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_modules::{
        EventBusCapability, GraphCapability, ModuleMeta, NetworkCapability,
    };

    fn manifest_with(caps: ModuleCapabilities) -> ModuleManifest {
        ModuleManifest {
            module: ModuleMeta {
                id: "com.example.test".into(),
                name: "Test".into(),
                version: "1.0.0".into(),
                description: String::new(),
                module_type: ModuleType::ThirdParty,
                entry: "module.wasm".into(),
                icon: String::new(),
            },
            waypointer: None,
            topbar: None,
            settings: None,
            capabilities: caps,
            permissions: Default::default(),
            keybindings: Vec::new(),
        }
    }

    #[test]
    fn profile_carries_app_id_and_tier() {
        let p = profile_from_manifest(&manifest_with(ModuleCapabilities::default()));
        assert_eq!(p.info.app_id, "com.example.test");
        assert_eq!(p.info.tier, AppTier::ThirdParty);
    }

    #[test]
    fn graph_capabilities_translate_to_profile() {
        let mut caps = ModuleCapabilities::default();
        caps.graph = Some(GraphCapability {
            read: vec!["core.File".into()],
            write: vec!["module.x.".into()],
        });
        let p = profile_from_manifest(&manifest_with(caps));
        assert_eq!(p.graph.read, vec!["core.File"]);
        assert_eq!(p.graph.write, vec!["module.x."]);
    }

    #[test]
    fn network_capabilities_translate_to_profile() {
        let mut caps = ModuleCapabilities::default();
        caps.network = Some(NetworkCapability {
            allowed_domains: vec!["api.example.com".into()],
        });
        let p = profile_from_manifest(&manifest_with(caps));
        assert_eq!(p.network.allowed_domains, vec!["api.example.com"]);
        assert!(!p.network.allow_all);
    }

    #[test]
    fn event_bus_capabilities_translate_to_profile() {
        let mut caps = ModuleCapabilities::default();
        caps.event_bus = Some(EventBusCapability {
            subscribe: vec!["focus.".into()],
            publish: vec!["module.x.".into()],
        });
        let p = profile_from_manifest(&manifest_with(caps));
        assert_eq!(p.event_bus.subscribe, vec!["focus."]);
        assert_eq!(p.event_bus.publish, vec!["module.x."]);
    }

    #[test]
    fn write_and_remove_profile_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LUNARIS_PERMISSIONS_DIR", tmp.path());

        let p = profile_from_manifest(&manifest_with(ModuleCapabilities::default()));
        write_profile(&p).unwrap();
        let path = permission_profile_path("com.example.test");
        assert!(path.exists());

        remove_profile("com.example.test").unwrap();
        assert!(!path.exists());

        std::env::remove_var("LUNARIS_PERMISSIONS_DIR");
    }
}
