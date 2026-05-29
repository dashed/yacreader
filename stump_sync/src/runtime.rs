use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::mapping_db::MappingDb;
use crate::stump_client::StumpClient;
use crate::sync_engine::SyncEngine;
use crate::types::SyncError;

pub enum SyncCommand {
    PushProgress { library_id: i64, comic_id: i64 },
    PushAll,
    PullAll,
    SyncAll,
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct SyncStatusInfo {
    pub is_running: bool,
    pub last_error: String,
    pub synced_count: u32,
}

impl Default for SyncStatusInfo {
    fn default() -> Self {
        Self {
            is_running: false,
            last_error: String::new(),
            synced_count: 0,
        }
    }
}

pub struct SyncRuntime {
    tx: mpsc::UnboundedSender<SyncCommand>,
    runtime: Option<tokio::runtime::Runtime>,
    status: Arc<Mutex<SyncStatusInfo>>,
}

impl SyncRuntime {
    pub fn init(config: Config) -> Result<Self, SyncError> {
        let client = StumpClient::new(
            config.stump_url.clone(),
            config.api_key.clone(),
            config.user_id.clone(),
        )?;

        let mapping_db = MappingDb::open(&config.mapping_db_path)?;
        let poll_interval_secs = config.sync_interval_secs;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| SyncError::Config(format!("failed to create tokio runtime: {e}")))?;

        // Resolve auto-discovered libraries (those configured without an explicit
        // Stump id/path) up front, on the runtime. Unmatched libraries are
        // dropped with a warning rather than failing init.
        let libraries = runtime.block_on(crate::sync_engine::resolve_library_configs(
            &client,
            config.libraries,
        ));
        let engine = SyncEngine::new(client, mapping_db, libraries);

        let (tx, rx) = mpsc::unbounded_channel();
        let status = Arc::new(Mutex::new(SyncStatusInfo {
            is_running: true,
            ..Default::default()
        }));

        let status_clone = Arc::clone(&status);
        runtime.spawn(event_loop(rx, engine, status_clone, poll_interval_secs));

        tracing::info!(poll_interval_secs, "stump_sync runtime initialized");

        Ok(Self {
            tx,
            runtime: Some(runtime),
            status,
        })
    }

    pub fn send(&self, cmd: SyncCommand) -> Result<(), SyncError> {
        self.tx
            .send(cmd)
            .map_err(|_| SyncError::NotInitialized)
    }

    pub fn shutdown(mut self) -> Result<(), SyncError> {
        tracing::info!("shutting down stump_sync runtime");
        let _ = self.tx.send(SyncCommand::Shutdown);
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(10));
        }
        if let Ok(mut s) = self.status.lock() {
            s.is_running = false;
        }
        Ok(())
    }

    pub fn status(&self) -> SyncStatusInfo {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
}

async fn event_loop(
    mut rx: mpsc::UnboundedReceiver<SyncCommand>,
    engine: SyncEngine,
    status: Arc<Mutex<SyncStatusInfo>>,
    poll_interval_secs: u64,
) {
    tracing::info!("stump_sync event loop started");

    let mut poll_timer = if poll_interval_secs > 0 {
        Some(tokio::time::interval(std::time::Duration::from_secs(
            poll_interval_secs,
        )))
    } else {
        None
    };

    if let Some(ref mut timer) = poll_timer {
        timer.tick().await;
    }

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                match cmd {
                    Some(SyncCommand::PushProgress { library_id, comic_id }) => {
                        if let Err(e) = engine.push_single(library_id, comic_id).await {
                            tracing::error!(error = %e, "push_single failed");
                            if let Ok(mut s) = status.lock() {
                                s.last_error = e.to_string();
                            }
                        } else if let Ok(mut s) = status.lock() {
                            s.synced_count += 1;
                        }
                    }
                    Some(SyncCommand::PushAll) => {
                        match engine.push_all().await {
                            Ok(report) => {
                                if let Ok(mut s) = status.lock() {
                                    s.synced_count += report.pages_pushed + report.completions_pushed;
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "push_all failed");
                                if let Ok(mut s) = status.lock() {
                                    s.last_error = e.to_string();
                                }
                            }
                        }
                    }
                    Some(SyncCommand::PullAll) => {
                        match engine.pull_all().await {
                            Ok(report) => {
                                if let Ok(mut s) = status.lock() {
                                    s.synced_count += report.pages_pulled + report.completions_pulled;
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "pull_all failed");
                                if let Ok(mut s) = status.lock() {
                                    s.last_error = e.to_string();
                                }
                            }
                        }
                    }
                    Some(SyncCommand::SyncAll) => {
                        match engine.sync_all().await {
                            Ok(report) => {
                                if let Ok(mut s) = status.lock() {
                                    s.synced_count += report.pages_pushed + report.pages_pulled
                                        + report.completions_pushed + report.completions_pulled;
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "sync_all failed");
                                if let Ok(mut s) = status.lock() {
                                    s.last_error = e.to_string();
                                }
                            }
                        }
                    }
                    Some(SyncCommand::Shutdown) | None => {
                        tracing::info!("stump_sync event loop shutting down");
                        break;
                    }
                }
            }
            _ = async {
                if let Some(ref mut timer) = poll_timer {
                    timer.tick().await
                } else {
                    std::future::pending::<tokio::time::Instant>().await
                }
            } => {
                tracing::info!("periodic sync triggered");
                match engine.sync_all().await {
                    Ok(report) => {
                        tracing::info!(
                            pages_pushed = report.pages_pushed,
                            pages_pulled = report.pages_pulled,
                            "periodic sync complete"
                        );
                        if let Ok(mut s) = status.lock() {
                            s.synced_count += report.pages_pushed + report.pages_pulled
                                + report.completions_pushed + report.completions_pulled;
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "periodic sync failed");
                        if let Ok(mut s) = status.lock() {
                            s.last_error = e.to_string();
                        }
                    }
                }
            }
        }
    }

    if let Ok(mut s) = status.lock() {
        s.is_running = false;
    }
}
