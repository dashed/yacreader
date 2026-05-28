pub mod config;
pub mod mapping_db;
pub mod runtime;
pub mod stump_client;
pub mod sync_engine;
pub mod types;

#[cfg(feature = "ffi")]
use std::sync::Mutex;

#[cfg(feature = "ffi")]
use config::{Config, LibraryConfig};
#[cfg(feature = "ffi")]
use runtime::SyncRuntime;

#[cfg(feature = "ffi")]
static RUNTIME: Mutex<Option<SyncRuntime>> = Mutex::new(None);

#[cfg(feature = "ffi")]
#[cxx::bridge(namespace = "stump_sync")]
mod ffi {
    #[derive(Debug)]
    struct SyncConfig {
        stump_url: String,
        api_key: String,
        user_id: String,
        mapping_db_path: String,
        ydb_paths: Vec<String>,
        library_roots: Vec<String>,
        stump_library_ids: Vec<String>,
        stump_library_paths: Vec<String>,
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
        fn rust_sync_shutdown() -> SyncResult;
        fn rust_sync_status() -> SyncStatus;
    }
}

#[cfg(feature = "ffi")]
fn make_ok() -> ffi::SyncResult {
    ffi::SyncResult {
        success: true,
        error_message: String::new(),
    }
}

#[cfg(feature = "ffi")]
fn make_err(msg: String) -> ffi::SyncResult {
    ffi::SyncResult {
        success: false,
        error_message: msg,
    }
}

#[cfg(feature = "ffi")]
fn rust_sync_init(config: &ffi::SyncConfig) -> ffi::SyncResult {
    if config.ydb_paths.len() != config.library_roots.len()
        || config.ydb_paths.len() != config.stump_library_ids.len()
        || config.ydb_paths.len() != config.stump_library_paths.len()
    {
        return make_err("library config vectors must have equal length".into());
    }

    let libraries: Vec<LibraryConfig> = config
        .ydb_paths
        .iter()
        .zip(config.library_roots.iter())
        .zip(config.stump_library_ids.iter())
        .zip(config.stump_library_paths.iter())
        .map(|(((ydb, root), sid), spath)| LibraryConfig {
            ydb_path: ydb.clone(),
            library_root: root.clone(),
            stump_library_id: sid.clone(),
            stump_library_path: spath.clone(),
        })
        .collect();

    let internal_config = Config {
        stump_url: config.stump_url.clone(),
        api_key: config.api_key.clone(),
        user_id: config.user_id.clone(),
        mapping_db_path: config.mapping_db_path.clone(),
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

#[cfg(feature = "ffi")]
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

#[cfg(feature = "ffi")]
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

#[cfg(feature = "ffi")]
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

#[cfg(feature = "ffi")]
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
