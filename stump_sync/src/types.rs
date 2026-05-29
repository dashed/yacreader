use serde::Deserialize;
use std::fmt;

/// Progress to push toward Stump. Page and completion are independent
/// dimensions, so either, both, or neither may be present.
#[derive(Debug, Clone, PartialEq)]
pub struct PushAction {
    /// New page to send to Stump (None = leave Stump's page unchanged).
    pub page: Option<i32>,
    /// Mark the comic complete on Stump.
    pub mark_complete: bool,
}

/// Progress to pull toward YACReader. Page and completion are independent
/// dimensions, so either, both, or neither may be present.
#[derive(Debug, Clone, PartialEq)]
pub struct PullAction {
    /// New page to write into the .ydb (None = leave YAC's page unchanged).
    pub page: Option<i32>,
    /// In a normal (monotonic) pull this REQUESTS raising the YAC `read` flag to
    /// true and is ignored when false (the writer never clears an existing
    /// read=1). In a validated regression pull (`allow_regression = true`) this
    /// is the EXACT desired `read` value, so `false` clears it.
    pub set_read: bool,
    /// Permit the .ydb writer to LOWER the page and/or CLEAR `read` — the single
    /// path allowed to bypass the C2 monotonic write-guard. Set true ONLY for a
    /// validated, same-side, timestamp-checked Stump→YAC re-read / unread (H6);
    /// every other pull keeps this false so C2 safety holds.
    pub allow_regression: bool,
    /// lastTimeOpened to record alongside the pull, if a source time is known.
    pub last_opened: Option<i64>,
}

/// The reconciliation result for a single comic. A comic may need BOTH a push
/// and a pull at once (e.g. YAC ahead on page while Stump holds completion),
/// so push and pull are fully independent.
#[derive(Debug, Clone)]
pub struct BidirectionalDelta {
    pub comic_info_id: i64,
    pub stump_media_id: String,
    pub ydb_path: String,
    pub num_pages: i32,
    pub push: Option<PushAction>,
    pub pull: Option<PullAction>,
    /// The converged page both sides should hold after this sync. Recorded into
    /// `sync_state` as the next baseline (H6/M2). Equals `max(yac, stump)` unless
    /// a validated re-read lowered it.
    pub final_page: i32,
    /// The converged completion state both sides should hold after this sync.
    /// Recorded into `sync_state` (H6/M2). Equals `yac.read || stump.is_complete()`
    /// unless a validated unread regression cleared it.
    pub final_complete: bool,
}

/// A Stump library as returned by the root `libraries` query. Used for
/// auto-discovery of the YAC↔Stump library mapping.
#[derive(Debug, Clone, Deserialize)]
pub struct StumpLibrary {
    pub id: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct ComicProgress {
    pub comic_info_id: i64,
    pub comic_id: i64,
    pub current_page: i32,
    pub num_pages: i32,
    pub read: bool,
    pub has_been_opened: bool,
    pub last_time_opened: Option<i64>,
    pub hash: Option<String>,
    pub relative_path: String,
}

/// A single Stump media (comic) as returned by `libraryById(...).media`.
///
/// Completion is NOT a flag on the active session — it is the *presence* of a
/// `FinishedReadingSession` in `read_history`. The active session (`readProgress`,
/// singular & nullable) only carries the in-progress page.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StumpMedia {
    pub id: String,
    pub name: String,
    pub pages: i32,
    pub path: String,
    /// The user's active reading session, or `None` if there is no in-progress
    /// session (e.g. never opened, or finished and the active session cleared).
    #[serde(default)]
    pub read_progress: Option<ActiveReadingSession>,
    /// Finished reading sessions. A non-empty list means the comic is complete.
    #[serde(default)]
    pub read_history: Vec<FinishedReadingSession>,
}

impl StumpMedia {
    /// The current page on Stump.
    ///   * an active session with a real (>= 0) page wins (epub sessions use -1);
    ///   * otherwise a completed comic sits at its LAST page (`pages`), never 0 —
    ///     critical so a finished comic isn't treated as "page 0" by conflict
    ///     resolution;
    ///   * otherwise 0 (untouched).
    pub fn current_page(&self) -> i32 {
        match self.read_progress.as_ref().and_then(|rp| rp.page) {
            Some(page) if page >= 0 => page,
            _ if self.is_complete() => self.pages,
            _ => 0,
        }
    }

    /// A comic is complete iff Stump recorded at least one finished session.
    pub fn is_complete(&self) -> bool {
        !self.read_history.is_empty()
    }

    /// The Stump-side "source last-modified" timestamp used for H6 conflict
    /// detection: the active session's `updatedAt` if present, else the first
    /// finished session's `completedAt`. Both are RFC3339 UTC strings emitted by
    /// the same Stump server, so two such values compare correctly
    /// lexically (lexical order == chronological order for a fixed RFC3339
    /// format). Cross-server / mixed-offset comparison is NOT attempted — H6
    /// only ever compares a Stump timestamp against an earlier Stump timestamp.
    pub fn stump_timestamp(&self) -> Option<&str> {
        self.read_progress
            .as_ref()
            .and_then(|rp| rp.updated_at.as_deref())
            .or_else(|| {
                self.read_history
                    .first()
                    .and_then(|fs| fs.completed_at.as_deref())
            })
    }
}

/// The user's active (in-progress) reading session for a comic. Singular and
/// nullable on Stump. `page` is nullable and is -1 for epub sessions.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveReadingSession {
    pub page: Option<i32>,
    pub updated_at: Option<String>,
}

/// A finished reading session. Its mere presence in `read_history` marks the
/// comic as complete.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishedReadingSession {
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncDelta {
    pub stump_media_id: String,
    pub new_page: Option<i32>,
    pub should_mark_complete: bool,
}

#[derive(Debug, Clone)]
pub struct MediaMapping {
    pub id: i64,
    pub library_mapping_id: i64,
    pub yac_comic_info_id: i64,
    pub yac_comic_id: i64,
    pub stump_media_id: String,
    pub relative_path: String,
    pub filename: String,
    pub matched_via: String,
}

#[derive(Debug, Clone)]
pub struct SyncState {
    pub id: i64,
    pub media_mapping_id: i64,
    pub yac_current_page: i32,
    pub stump_current_page: i32,
    pub yac_read: bool,
    pub stump_complete: bool,
    pub yac_last_modified: Option<String>,
    pub stump_last_modified: Option<String>,
    pub last_synced_at: String,
}

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub libraries_processed: u32,
    pub comics_matched: u32,
    pub pages_pushed: u32,
    pub completions_pushed: u32,
    pub pages_pulled: u32,
    pub completions_pulled: u32,
    pub errors: Vec<String>,
}

impl SyncReport {
    pub fn new() -> Self {
        Self {
            libraries_processed: 0,
            comics_matched: 0,
            pages_pushed: 0,
            completions_pushed: 0,
            pages_pulled: 0,
            completions_pulled: 0,
            errors: Vec::new(),
        }
    }
}

impl Default for SyncReport {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum SyncError {
    Database(String),
    Http(String),
    GraphQL(String),
    Config(String),
    NotInitialized,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Database(msg) => write!(f, "database error: {msg}"),
            SyncError::Http(msg) => write!(f, "HTTP error: {msg}"),
            SyncError::GraphQL(msg) => write!(f, "GraphQL error: {msg}"),
            SyncError::Config(msg) => write!(f, "config error: {msg}"),
            SyncError::NotInitialized => write!(f, "sync engine not initialized"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<rusqlite::Error> for SyncError {
    fn from(e: rusqlite::Error) -> Self {
        SyncError::Database(e.to_string())
    }
}

impl From<reqwest::Error> for SyncError {
    fn from(e: reqwest::Error) -> Self {
        SyncError::Http(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_error_display() {
        let err = SyncError::Database("table not found".into());
        assert_eq!(err.to_string(), "database error: table not found");

        let err = SyncError::Http("timeout".into());
        assert_eq!(err.to_string(), "HTTP error: timeout");

        let err = SyncError::GraphQL("invalid query".into());
        assert_eq!(err.to_string(), "GraphQL error: invalid query");

        let err = SyncError::Config("missing url".into());
        assert_eq!(err.to_string(), "config error: missing url");

        let err = SyncError::NotInitialized;
        assert_eq!(err.to_string(), "sync engine not initialized");
    }

    #[test]
    fn test_sync_report_default() {
        let report = SyncReport::new();
        assert_eq!(report.libraries_processed, 0);
        assert_eq!(report.comics_matched, 0);
        assert_eq!(report.pages_pushed, 0);
        assert_eq!(report.completions_pushed, 0);
        assert_eq!(report.pages_pulled, 0);
        assert_eq!(report.completions_pulled, 0);
        assert!(report.errors.is_empty());
    }

    /// Untouched: no active session, no history → page 0, not complete.
    #[test]
    fn test_stump_media_current_page_empty() {
        let media = StumpMedia {
            id: "id1".into(),
            name: "test".into(),
            pages: 20,
            path: "/comics/test.cbz".into(),
            read_progress: None,
            read_history: vec![],
        };
        assert_eq!(media.current_page(), 0);
        assert!(!media.is_complete());
    }

    /// In-progress: active session with a page, no history → that page, not complete.
    #[test]
    fn test_stump_media_current_page_with_progress() {
        let media = StumpMedia {
            id: "id1".into(),
            name: "test".into(),
            pages: 20,
            path: "/comics/test.cbz".into(),
            read_progress: Some(ActiveReadingSession {
                page: Some(10),
                updated_at: Some("2026-05-20T14:32:10Z".into()),
            }),
            read_history: vec![],
        };
        assert_eq!(media.current_page(), 10);
        assert!(!media.is_complete());
    }

    /// Re-reading: an active session AND a finished session. The active page wins
    /// for `current_page`, and the comic is still considered complete.
    #[test]
    fn test_stump_media_is_complete() {
        let media = StumpMedia {
            id: "id1".into(),
            name: "test".into(),
            pages: 20,
            path: "/comics/test.cbz".into(),
            read_progress: Some(ActiveReadingSession {
                page: Some(8),
                updated_at: None,
            }),
            read_history: vec![FinishedReadingSession {
                completed_at: Some("2024-01-01".into()),
            }],
        };
        assert_eq!(media.current_page(), 8);
        assert!(media.is_complete());
    }

    /// Completed with NO active session: `current_page` falls back to the last
    /// page (`pages`), NOT 0, so conflict resolution never sees a finished comic
    /// as "page 0".
    #[test]
    fn test_current_page_completed_no_active() {
        let media = StumpMedia {
            id: "id1".into(),
            name: "test".into(),
            pages: 20,
            path: "/comics/test.cbz".into(),
            read_progress: None,
            read_history: vec![FinishedReadingSession {
                completed_at: Some("2026-05-19T09:01:00Z".into()),
            }],
        };
        assert_eq!(media.current_page(), 20);
        assert!(media.is_complete());
    }
}
