pub mod config;
pub mod mapping_db;
pub mod runtime;
pub mod stump_client;
pub mod sync_engine;
pub mod types;

use std::sync::Mutex;

use config::{Config, LibraryConfig};
use runtime::SyncRuntime;

static RUNTIME: Mutex<Option<SyncRuntime>> = Mutex::new(None);

// NOTE: the cxx bridge is intentionally NOT behind `#[cfg(feature = "ffi")]`.
// The `cxxbridge` CLI used by `corrosion_add_cxxbridge` parses this file without
// any Cargo features, so a feature-gated bridge would be skipped and generate an
// empty header. `cxx` is a normal dependency, so the bridge always compiles; the
// `ffi` feature only gates the standalone C++ glue compile in build.rs.
#[cxx::bridge(namespace = "stump_sync")]
mod ffi {
    /// One YAC↔Stump library mapping. Empty `stump_library_id` /
    /// `stump_library_path` mean "auto-discover this library at init".
    #[derive(Debug)]
    struct LibraryEntry {
        yac_library_id: i64,
        ydb_path: String,
        library_root: String,
        stump_library_id: String,
        stump_library_path: String,
    }

    #[derive(Debug)]
    struct SyncConfig {
        stump_url: String,
        api_key: String,
        user_id: String,
        mapping_db_path: String,
        sync_interval_secs: u64,
        libraries: Vec<LibraryEntry>,
    }

    #[derive(Debug)]
    struct SyncResult {
        success: bool,
        error_message: String,
    }

    #[derive(Debug)]
    struct SyncStatus {
        is_running: bool,
        last_error: String,
        synced_count: u32,
    }

    extern "Rust" {
        fn rust_sync_init(config: &SyncConfig) -> SyncResult;
        fn rust_sync_push_progress(library_id: i64, comic_id: i64) -> SyncResult;
        fn rust_sync_push_all() -> SyncResult;
        fn rust_sync_pull_all() -> SyncResult;
        fn rust_sync_sync_all() -> SyncResult;
        fn rust_sync_shutdown() -> SyncResult;
        fn rust_sync_status() -> SyncStatus;
    }
}

fn make_ok() -> ffi::SyncResult {
    ffi::SyncResult {
        success: true,
        error_message: String::new(),
    }
}

fn make_err(msg: String) -> ffi::SyncResult {
    ffi::SyncResult {
        success: false,
        error_message: msg,
    }
}

fn library_configs_from_ffi(entries: &[ffi::LibraryEntry]) -> Vec<LibraryConfig> {
    entries
        .iter()
        .map(|e| LibraryConfig {
            yac_library_id: e.yac_library_id,
            ydb_path: e.ydb_path.clone(),
            library_root: e.library_root.clone(),
            stump_library_id: e.stump_library_id.clone(),
            stump_library_path: e.stump_library_path.clone(),
        })
        .collect()
}

fn rust_sync_init(config: &ffi::SyncConfig) -> ffi::SyncResult {
    // Per-entry config: each library carries its own ids/paths (no parallel
    // vectors, no equal-length check). Empty stump_* fields are auto-discovered.
    let libraries = library_configs_from_ffi(&config.libraries);

    let internal_config = Config {
        stump_url: config.stump_url.clone(),
        api_key: config.api_key.clone(),
        user_id: config.user_id.clone(),
        mapping_db_path: config.mapping_db_path.clone(),
        sync_interval_secs: config.sync_interval_secs,
        libraries,
    };

    match SyncRuntime::init(internal_config) {
        Ok(rt) => {
            let mut guard = match RUNTIME.lock() {
                Ok(g) => g,
                Err(e) => return make_err(format!("lock poisoned: {e}")),
            };
            *guard = Some(rt);
            tracing::info!("stump_sync initialized successfully");
            make_ok()
        }
        Err(e) => make_err(e.to_string()),
    }
}

fn rust_sync_push_progress(library_id: i64, comic_id: i64) -> ffi::SyncResult {
    let guard = match RUNTIME.lock() {
        Ok(g) => g,
        Err(e) => return make_err(format!("lock poisoned: {e}")),
    };

    match guard.as_ref() {
        Some(rt) => match rt.send(runtime::SyncCommand::PushProgress {
            library_id,
            comic_id,
        }) {
            Ok(()) => make_ok(),
            Err(e) => make_err(e.to_string()),
        },
        None => make_err("sync engine not initialized".into()),
    }
}

fn rust_sync_push_all() -> ffi::SyncResult {
    let guard = match RUNTIME.lock() {
        Ok(g) => g,
        Err(e) => return make_err(format!("lock poisoned: {e}")),
    };

    match guard.as_ref() {
        Some(rt) => match rt.send(runtime::SyncCommand::PushAll) {
            Ok(()) => make_ok(),
            Err(e) => make_err(e.to_string()),
        },
        None => make_err("sync engine not initialized".into()),
    }
}

fn rust_sync_pull_all() -> ffi::SyncResult {
    let guard = match RUNTIME.lock() {
        Ok(g) => g,
        Err(e) => return make_err(format!("lock poisoned: {e}")),
    };

    match guard.as_ref() {
        Some(rt) => match rt.send(runtime::SyncCommand::PullAll) {
            Ok(()) => make_ok(),
            Err(e) => make_err(e.to_string()),
        },
        None => make_err("sync engine not initialized".into()),
    }
}

fn rust_sync_sync_all() -> ffi::SyncResult {
    let guard = match RUNTIME.lock() {
        Ok(g) => g,
        Err(e) => return make_err(format!("lock poisoned: {e}")),
    };

    match guard.as_ref() {
        Some(rt) => match rt.send(runtime::SyncCommand::SyncAll) {
            Ok(()) => make_ok(),
            Err(e) => make_err(e.to_string()),
        },
        None => make_err("sync engine not initialized".into()),
    }
}

fn rust_sync_shutdown() -> ffi::SyncResult {
    let mut guard = match RUNTIME.lock() {
        Ok(g) => g,
        Err(e) => return make_err(format!("lock poisoned: {e}")),
    };

    match guard.take() {
        Some(rt) => match rt.shutdown() {
            Ok(()) => {
                tracing::info!("stump_sync shut down successfully");
                make_ok()
            }
            Err(e) => make_err(e.to_string()),
        },
        None => make_err("sync engine not initialized".into()),
    }
}

fn rust_sync_status() -> ffi::SyncStatus {
    let guard = match RUNTIME.lock() {
        Ok(g) => g,
        Err(_) => {
            return ffi::SyncStatus {
                is_running: false,
                last_error: "lock poisoned".into(),
                synced_count: 0,
            }
        }
    };

    match guard.as_ref() {
        Some(rt) => {
            let info = rt.status();
            ffi::SyncStatus {
                is_running: info.is_running,
                last_error: info.last_error,
                synced_count: info.synced_count,
            }
        }
        None => ffi::SyncStatus {
            is_running: false,
            last_error: String::new(),
            synced_count: 0,
        },
    }
}

#[cfg(test)]
mod ffi_tests {
    use super::*;

    #[test]
    fn test_library_configs_from_ffi_per_entry() {
        // Per-entry mapping: an explicit library and an auto-discover library
        // (empty stump_* fields) are both carried through without any
        // equal-length constraint.
        let entries = vec![
            ffi::LibraryEntry {
                yac_library_id: 7,
                ydb_path: "/a/.yacreaderlibrary/library.ydb".into(),
                library_root: "/a".into(),
                stump_library_id: "stump-a".into(),
                stump_library_path: "/srv/a".into(),
            },
            ffi::LibraryEntry {
                yac_library_id: 8,
                ydb_path: "/b/.yacreaderlibrary/library.ydb".into(),
                library_root: "/b".into(),
                stump_library_id: String::new(),
                stump_library_path: String::new(),
            },
        ];

        let configs = library_configs_from_ffi(&entries);
        assert_eq!(configs.len(), 2);

        assert_eq!(configs[0].yac_library_id, 7);
        assert_eq!(configs[0].ydb_path, "/a/.yacreaderlibrary/library.ydb");
        assert_eq!(configs[0].stump_library_id, "stump-a");
        assert_eq!(configs[0].stump_library_path, "/srv/a");

        assert_eq!(configs[1].yac_library_id, 8);
        assert!(
            configs[1].stump_library_id.is_empty(),
            "empty stump id marks this entry for auto-discovery"
        );
    }
}
