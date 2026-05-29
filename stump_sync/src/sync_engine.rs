use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{OpenFlags, OptionalExtension};

use crate::config::LibraryConfig;
use crate::mapping_db::MappingDb;
use crate::stump_client::StumpClient;
use crate::types::{
    BidirectionalDelta, ComicProgress, PullAction, PushAction, StumpLibrary, StumpMedia, SyncDelta,
    SyncError, SyncReport,
};

/// Current wall-clock time in epoch seconds (0 on the impossible pre-epoch case).
fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct SyncEngine {
    client: StumpClient,
    mapping_db: MappingDb,
    libraries: Vec<LibraryConfig>,
}

impl SyncEngine {
    pub fn new(
        client: StumpClient,
        mapping_db: MappingDb,
        libraries: Vec<LibraryConfig>,
    ) -> Self {
        Self {
            client,
            mapping_db,
            libraries,
        }
    }

    /// Find the configured library for a YACReader numeric library id (the id
    /// carried by `comicUpdated` / used in `/v2/library/<id>/`).
    fn get_library_config_by_id(&self, yac_library_id: i64) -> Option<&LibraryConfig> {
        self.libraries
            .iter()
            .find(|l| l.yac_library_id == yac_library_id)
    }

    pub async fn push_single(
        &self,
        library_id: i64,
        comic_id: i64,
    ) -> Result<(), SyncError> {
        // H2: resolve the library by its stable YACReader id. The previous code
        // matched on `ensure_library_mapping(...)`'s autoincrement rowid inside a
        // `find()` predicate, which is a different id space (it mutated + swallowed
        // errors and only coincided by luck for a single library).
        let lib_config = self
            .get_library_config_by_id(library_id)
            .ok_or_else(|| {
                SyncError::Config(format!("no library config for yac_library_id {library_id}"))
            })?
            .clone();

        let lib_mapping_id = self.mapping_db.ensure_library_mapping(
            &lib_config.ydb_path,
            &lib_config.stump_library_id,
            &lib_config.stump_library_path,
        )?;

        let comics = read_yac_comics(&lib_config.ydb_path)?;
        let comic = comics
            .iter()
            .find(|c| c.comic_id == comic_id)
            .ok_or_else(|| {
                SyncError::Database(format!("comic {comic_id} not found in ydb"))
            })?;

        let stump_media_id = match self.mapping_db.get_stump_id(comic.comic_info_id)? {
            Some(id) => id,
            None => {
                tracing::info!(comic_id, "no mapping found, building mappings");
                self.build_mappings(&lib_config, lib_mapping_id).await?;
                self.mapping_db
                    .get_stump_id(comic.comic_info_id)?
                    .ok_or_else(|| {
                        SyncError::Config(format!(
                            "no Stump match for comic_info_id {}",
                            comic.comic_info_id
                        ))
                    })?
            }
        };

        let stump_media_list = self
            .client
            .get_library_media(&lib_config.stump_library_id)
            .await?;
        let stump_media = stump_media_list
            .iter()
            .find(|m| m.id == stump_media_id)
            .ok_or_else(|| {
                SyncError::Config(format!("Stump media {stump_media_id} not found"))
            })?;

        if let Some(delta) = compute_delta(comic, stump_media) {
            self.apply_delta(&delta).await?;
        }

        Ok(())
    }

    pub async fn push_all(&self) -> Result<SyncReport, SyncError> {
        let mut report = SyncReport::new();

        for lib_config in &self.libraries {
            let lib_mapping_id = self.mapping_db.ensure_library_mapping(
                &lib_config.ydb_path,
                &lib_config.stump_library_id,
                &lib_config.stump_library_path,
            )?;

            report.libraries_processed += 1;

            if let Err(e) = self
                .push_library(lib_config, lib_mapping_id, &mut report)
                .await
            {
                tracing::error!(library = %lib_config.ydb_path, error = %e, "failed to sync library");
                report
                    .errors
                    .push(format!("{}: {e}", lib_config.ydb_path));
            }
        }

        tracing::info!(
            libraries = report.libraries_processed,
            matched = report.comics_matched,
            pages_pushed = report.pages_pushed,
            completions = report.completions_pushed,
            errors = report.errors.len(),
            "push_all complete"
        );

        Ok(report)
    }

    async fn push_library(
        &self,
        lib_config: &LibraryConfig,
        lib_mapping_id: i64,
        report: &mut SyncReport,
    ) -> Result<(), SyncError> {
        let matched = self.build_mappings(lib_config, lib_mapping_id).await?;
        report.comics_matched += matched;

        let comics = read_yac_comics(&lib_config.ydb_path)?;
        let stump_media = self
            .client
            .get_library_media(&lib_config.stump_library_id)
            .await?;

        let stump_map: std::collections::HashMap<&str, &StumpMedia> =
            stump_media.iter().map(|m| (m.id.as_str(), m)).collect();

        for comic in &comics {
            let stump_id = match self.mapping_db.get_stump_id(comic.comic_info_id)? {
                Some(id) => id,
                None => continue,
            };

            let media = match stump_map.get(stump_id.as_str()) {
                Some(m) => m,
                None => continue,
            };

            if let Some(delta) = compute_delta(comic, media) {
                match self.apply_delta(&delta).await {
                    Ok(()) => {
                        if delta.new_page.is_some() {
                            report.pages_pushed += 1;
                        }
                        if delta.should_mark_complete {
                            report.completions_pushed += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(media_id = %delta.stump_media_id, error = %e, "failed to push delta");
                        report.errors.push(format!("{}: {e}", delta.stump_media_id));
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn build_mappings(
        &self,
        lib_config: &LibraryConfig,
        lib_mapping_id: i64,
    ) -> Result<u32, SyncError> {
        let yac_comics = read_yac_comics(&lib_config.ydb_path)?;
        let stump_media = self
            .client
            .get_library_media(&lib_config.stump_library_id)
            .await?;

        let matches = match_comics(&yac_comics, &stump_media, &lib_config.stump_library_path);

        let mut count = 0u32;
        for (comic, media) in &matches {
            self.mapping_db.insert_mapping(
                lib_mapping_id,
                comic.comic_info_id,
                comic.comic_id,
                &media.id,
                &comic.relative_path,
                &comic
                    .relative_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&comic.relative_path),
                "path",
            )?;
            count += 1;
        }

        tracing::info!(
            library = %lib_config.ydb_path,
            yac_count = yac_comics.len(),
            stump_count = stump_media.len(),
            matched = count,
            "built mappings"
        );

        Ok(count)
    }

    pub async fn sync_all(&self) -> Result<SyncReport, SyncError> {
        let mut report = SyncReport::new();

        for lib_config in &self.libraries {
            let lib_mapping_id = self.mapping_db.ensure_library_mapping(
                &lib_config.ydb_path,
                &lib_config.stump_library_id,
                &lib_config.stump_library_path,
            )?;

            report.libraries_processed += 1;

            if let Err(e) = self
                .sync_library(lib_config, lib_mapping_id, &mut report, false)
                .await
            {
                tracing::error!(library = %lib_config.ydb_path, error = %e, "failed to sync library");
                report
                    .errors
                    .push(format!("{}: {e}", lib_config.ydb_path));
            }
        }

        tracing::info!(
            libraries = report.libraries_processed,
            matched = report.comics_matched,
            pages_pushed = report.pages_pushed,
            pages_pulled = report.pages_pulled,
            completions_pushed = report.completions_pushed,
            completions_pulled = report.completions_pulled,
            errors = report.errors.len(),
            "sync_all complete"
        );

        Ok(report)
    }

    pub async fn pull_all(&self) -> Result<SyncReport, SyncError> {
        let mut report = SyncReport::new();

        for lib_config in &self.libraries {
            let lib_mapping_id = self.mapping_db.ensure_library_mapping(
                &lib_config.ydb_path,
                &lib_config.stump_library_id,
                &lib_config.stump_library_path,
            )?;

            report.libraries_processed += 1;

            if let Err(e) = self
                .sync_library(lib_config, lib_mapping_id, &mut report, true)
                .await
            {
                tracing::error!(library = %lib_config.ydb_path, error = %e, "failed to pull library");
                report
                    .errors
                    .push(format!("{}: {e}", lib_config.ydb_path));
            }
        }

        tracing::info!(
            libraries = report.libraries_processed,
            matched = report.comics_matched,
            pages_pulled = report.pages_pulled,
            completions_pulled = report.completions_pulled,
            errors = report.errors.len(),
            "pull_all complete"
        );

        Ok(report)
    }

    async fn sync_library(
        &self,
        lib_config: &LibraryConfig,
        lib_mapping_id: i64,
        report: &mut SyncReport,
        pull_only: bool,
    ) -> Result<(), SyncError> {
        let yac_comics = read_all_yac_comics(&lib_config.ydb_path)?;
        let stump_media = self
            .client
            .get_library_media(&lib_config.stump_library_id)
            .await?;

        let matches = match_comics(&yac_comics, &stump_media, &lib_config.stump_library_path);
        for (comic, media) in &matches {
            self.mapping_db.insert_mapping(
                lib_mapping_id,
                comic.comic_info_id,
                comic.comic_id,
                &media.id,
                &comic.relative_path,
                &comic
                    .relative_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&comic.relative_path),
                "path",
            )?;
        }
        report.comics_matched += matches.len() as u32;

        let all_mappings = self.mapping_db.get_all_mappings(lib_mapping_id)?;
        let mapping_by_comic: HashMap<i64, i64> = all_mappings
            .iter()
            .map(|m| (m.yac_comic_info_id, m.id))
            .collect();

        for (comic, media) in &matches {
            let delta =
                compute_bidirectional_delta(comic, media, &lib_config.ydb_path);

            // Push and pull are independent: a single comic may need both (e.g.
            // YAC ahead on page while Stump holds completion).
            if !pull_only {
                if let Some(push) = &delta.push {
                    self.apply_push_delta(&delta, push, report).await;
                }
            }
            if let Some(pull) = &delta.pull {
                self.apply_pull_delta(&delta, pull, comic.current_page, report);
            }

            if let Some(&mapping_id) = mapping_by_comic.get(&comic.comic_info_id) {
                let _ = self.mapping_db.update_sync_state(
                    mapping_id,
                    comic.current_page,
                    media.current_page(),
                    comic.read,
                    media.is_complete(),
                    None,
                    None,
                );
            }
        }

        Ok(())
    }

    async fn apply_push_delta(
        &self,
        delta: &BidirectionalDelta,
        push: &PushAction,
        report: &mut SyncReport,
    ) {
        if let Some(page) = push.page {
            tracing::info!(media_id = %delta.stump_media_id, page, "pushing page progress");
            if let Err(e) = self.client.update_progress(&delta.stump_media_id, page).await {
                tracing::warn!(media_id = %delta.stump_media_id, error = %e, "failed to push page");
                report.errors.push(format!("{}: {e}", delta.stump_media_id));
                return;
            }
            report.pages_pushed += 1;
        }
        if push.mark_complete {
            tracing::info!(media_id = %delta.stump_media_id, "marking complete on Stump");
            if let Err(e) = self.client.mark_complete(&delta.stump_media_id, true).await {
                tracing::warn!(media_id = %delta.stump_media_id, error = %e, "failed to push completion");
                report.errors.push(format!("{}: {e}", delta.stump_media_id));
                return;
            }
            report.completions_pushed += 1;
        }
    }

    fn apply_pull_delta(
        &self,
        delta: &BidirectionalDelta,
        pull: &PullAction,
        current_yac_page: i32,
        report: &mut SyncReport,
    ) {
        let page = pull.page.unwrap_or(current_yac_page);
        let has_page_change = pull.page.is_some();
        tracing::info!(
            comic_info_id = delta.comic_info_id,
            page,
            read = pull.set_read,
            "pulling progress to YAC"
        );
        match write_yac_progress(
            &delta.ydb_path,
            delta.comic_info_id,
            page,
            delta.num_pages,
            pull.set_read,
            pull.last_opened,
        ) {
            Ok(()) => {
                if has_page_change {
                    report.pages_pulled += 1;
                }
                if pull.set_read {
                    report.completions_pulled += 1;
                }
            }
            Err(e) => {
                tracing::warn!(comic_info_id = delta.comic_info_id, error = %e, "failed to pull progress");
                report
                    .errors
                    .push(format!("yac:{}: {e}", delta.comic_info_id));
            }
        }
    }

    async fn apply_delta(&self, delta: &SyncDelta) -> Result<(), SyncError> {
        if let Some(page) = delta.new_page {
            tracing::info!(media_id = %delta.stump_media_id, page, "pushing page progress");
            self.client
                .update_progress(&delta.stump_media_id, page)
                .await?;
        }

        if delta.should_mark_complete {
            tracing::info!(media_id = %delta.stump_media_id, "marking complete");
            self.client
                .mark_complete(&delta.stump_media_id, true)
                .await?;
        }

        Ok(())
    }
}

pub fn read_yac_comics(ydb_path: &str) -> Result<Vec<ComicProgress>, SyncError> {
    let conn = rusqlite::Connection::open_with_flags(ydb_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // H3: the live server may hold the rollback-journal lock; wait rather than
    // failing immediately with SQLITE_BUSY (which would abort the library sync).
    conn.busy_timeout(Duration::from_millis(5000))?;

    let mut stmt = conn.prepare(
        "SELECT ci.id, c.id, ci.currentPage, ci.numPages, ci.read, ci.hasBeenOpened, ci.lastTimeOpened, ci.hash, c.path, c.fileName
         FROM comic_info ci
         JOIN comic c ON c.comicInfoId = ci.id
         WHERE ci.hasBeenOpened = 1",
    )?;

    let rows = stmt.query_map([], |row| {
        let path: String = row.get::<_, String>(8).unwrap_or_default();
        let filename: String = row.get::<_, String>(9).unwrap_or_default();
        let relative_path = normalize_path(&path, &filename);
        Ok(ComicProgress {
            comic_info_id: row.get(0)?,
            comic_id: row.get(1)?,
            current_page: row.get::<_, Option<i32>>(2)?.unwrap_or(0),
            num_pages: row.get::<_, Option<i32>>(3)?.unwrap_or(0),
            read: row.get::<_, Option<i32>>(4)?.unwrap_or(0) != 0,
            has_been_opened: row.get::<_, Option<i32>>(5)?.unwrap_or(0) != 0,
            last_time_opened: row.get(6)?,
            hash: row.get(7)?,
            relative_path,
        })
    })?;

    let mut comics = Vec::new();
    for row in rows {
        comics.push(row?);
    }
    Ok(comics)
}

pub fn normalize_path(path: &str, filename: &str) -> String {
    if path.is_empty() {
        filename.to_string()
    } else {
        format!("{path}/{filename}")
    }
}

pub fn match_comics<'a>(
    yac_comics: &'a [ComicProgress],
    stump_media: &'a [StumpMedia],
    stump_library_path: &str,
) -> Vec<(&'a ComicProgress, &'a StumpMedia)> {
    let stump_library_path = stump_library_path.trim_end_matches('/');

    let stump_by_relative: std::collections::HashMap<String, &StumpMedia> = stump_media
        .iter()
        .filter_map(|m| {
            let relative = m
                .path
                .strip_prefix(stump_library_path)?
                .trim_start_matches('/');
            if relative.is_empty() {
                None
            } else {
                Some((relative.to_string(), m))
            }
        })
        .collect();

    let mut matches = Vec::new();
    for comic in yac_comics {
        if let Some(media) = stump_by_relative.get(&comic.relative_path) {
            matches.push((comic, *media));
        }
    }
    matches
}

/// Normalize a filesystem path for comparison: trim a single trailing '/'
/// (without turning a bare "/" into the empty string).
pub fn normalize_fs_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && !path.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Match a configured YACReader library to a Stump library by normalized
/// filesystem path, falling back to an exact name match (the YAC library's
/// folder name against the Stump library's name or path basename).
pub fn match_stump_library<'a>(
    yac: &LibraryConfig,
    stump_libs: &'a [StumpLibrary],
) -> Option<&'a StumpLibrary> {
    let yac_root = normalize_fs_path(&yac.library_root);

    // 1. Exact normalized full-path match (most precise).
    if let Some(found) = stump_libs
        .iter()
        .find(|s| normalize_fs_path(&s.path) == yac_root)
    {
        return Some(found);
    }

    // 2. Fallback: the YAC library folder name vs a Stump library's name or its
    //    path basename. Paths differ across machines, so a name match is often
    //    the only signal available.
    let yac_name = yac_root.rsplit('/').next().unwrap_or("");
    if !yac_name.is_empty() {
        if let Some(found) = stump_libs.iter().find(|s| {
            s.name == yac_name
                || normalize_fs_path(&s.path).rsplit('/').next() == Some(yac_name)
        }) {
            return Some(found);
        }
    }

    None
}

/// Resolve the Stump id/path for any library configured for auto-discovery
/// (empty `stump_library_id`). Libraries with explicit overrides are kept as-is.
/// Auto-discoverable libraries that can't be matched are logged and dropped so a
/// single unmatched library never fails the whole init. If listing Stump
/// libraries fails entirely, only the explicitly-configured libraries are kept.
pub async fn resolve_library_configs(
    client: &StumpClient,
    libraries: Vec<LibraryConfig>,
) -> Vec<LibraryConfig> {
    let needs_discovery = libraries.iter().any(|l| l.stump_library_id.is_empty());
    if !needs_discovery {
        return libraries;
    }

    let stump_libs = match client.list_libraries().await {
        Ok(libs) => libs,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to list Stump libraries for auto-discovery; keeping only \
                 explicitly-configured libraries"
            );
            return libraries
                .into_iter()
                .filter(|l| !l.stump_library_id.is_empty())
                .collect();
        }
    };

    let mut resolved = Vec::new();
    for mut lib in libraries {
        if !lib.stump_library_id.is_empty() {
            resolved.push(lib);
            continue;
        }
        match match_stump_library(&lib, &stump_libs) {
            Some(stump_lib) => {
                lib.stump_library_id = stump_lib.id.clone();
                lib.stump_library_path = stump_lib.path.clone();
                tracing::info!(
                    yac_library_id = lib.yac_library_id,
                    library_root = %lib.library_root,
                    stump_library_id = %lib.stump_library_id,
                    stump_library_path = %lib.stump_library_path,
                    "auto-discovered Stump library"
                );
                resolved.push(lib);
            }
            None => {
                tracing::warn!(
                    yac_library_id = lib.yac_library_id,
                    library_root = %lib.library_root,
                    "no matching Stump library found for auto-discovery; skipping"
                );
            }
        }
    }
    resolved
}

pub fn compute_delta(yac: &ComicProgress, stump: &StumpMedia) -> Option<SyncDelta> {
    let stump_page = stump.current_page();
    let stump_complete = stump.is_complete();

    let should_push_page = yac.current_page > stump_page;
    let should_mark_complete = yac.read && !stump_complete;

    if !should_push_page && !should_mark_complete {
        return None;
    }

    Some(SyncDelta {
        stump_media_id: stump.id.clone(),
        new_page: if should_push_page {
            Some(yac.current_page)
        } else {
            None
        },
        should_mark_complete,
    })
}

pub fn read_all_yac_comics(ydb_path: &str) -> Result<Vec<ComicProgress>, SyncError> {
    let conn = rusqlite::Connection::open_with_flags(ydb_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // H3: see read_yac_comics — wait out the live server's lock instead of aborting.
    conn.busy_timeout(Duration::from_millis(5000))?;

    let mut stmt = conn.prepare(
        "SELECT ci.id, c.id, ci.currentPage, ci.numPages, ci.read, ci.hasBeenOpened, ci.lastTimeOpened, ci.hash, c.path, c.fileName
         FROM comic_info ci
         JOIN comic c ON c.comicInfoId = ci.id",
    )?;

    let rows = stmt.query_map([], |row| {
        let path: String = row.get::<_, String>(8).unwrap_or_default();
        let filename: String = row.get::<_, String>(9).unwrap_or_default();
        let relative_path = normalize_path(&path, &filename);
        Ok(ComicProgress {
            comic_info_id: row.get(0)?,
            comic_id: row.get(1)?,
            current_page: row.get::<_, Option<i32>>(2)?.unwrap_or(0),
            num_pages: row.get::<_, Option<i32>>(3)?.unwrap_or(0),
            read: row.get::<_, Option<i32>>(4)?.unwrap_or(0) != 0,
            has_been_opened: row.get::<_, Option<i32>>(5)?.unwrap_or(0) != 0,
            last_time_opened: row.get(6)?,
            hash: row.get(7)?,
            relative_path,
        })
    })?;

    let mut comics = Vec::new();
    for row in rows {
        comics.push(row?);
    }
    Ok(comics)
}

/// Apply pulled progress to the YACReader `.ydb` using a read-modify-write so
/// progress is monotonic and lossless (fixes C2/H4):
///   * `currentPage` never regresses (`max(existing, incoming)`),
///   * an existing `read = 1` is never cleared,
///   * `hasBeenOpened` is sticky once set,
///   * `lastTimeOpened` only advances.
/// A `busy_timeout` is set (H3) so a lock held by the live server is waited out
/// rather than failing immediately. The UPDATE is skipped when nothing changed.
pub fn write_yac_progress(
    ydb_path: &str,
    comic_info_id: i64,
    page: i32,
    num_pages: i32,
    read: bool,
    last_time_opened: Option<i64>,
) -> Result<(), SyncError> {
    let conn = rusqlite::Connection::open_with_flags(
        ydb_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    // H3: wait for the live server's whole-file (rollback-journal) lock.
    conn.busy_timeout(Duration::from_millis(5000))?;

    // Read the current row first; the row must already exist.
    let existing: Option<(i32, bool, bool, Option<i64>, i32)> = conn
        .query_row(
            "SELECT currentPage, read, hasBeenOpened, lastTimeOpened, numPages FROM comic_info WHERE id = ?1",
            rusqlite::params![comic_info_id],
            |row| {
                Ok((
                    row.get::<_, Option<i32>>(0)?.unwrap_or(0),
                    row.get::<_, Option<i32>>(1)?.unwrap_or(0) != 0,
                    row.get::<_, Option<i32>>(2)?.unwrap_or(0) != 0,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i32>>(4)?.unwrap_or(0),
                ))
            },
        )
        .optional()?;

    let (existing_page, existing_read, existing_hbo, existing_lto, existing_num_pages) =
        match existing {
            Some(row) => row,
            None => {
                return Err(SyncError::Database(format!(
                    "comic_info_id {comic_info_id} not found in {ydb_path}"
                )))
            }
        };

    // The live row's numPages is authoritative; fall back to the caller's value.
    let effective_num_pages = if existing_num_pages > 0 {
        existing_num_pages
    } else {
        num_pages
    };

    // Page never regresses.
    let new_page = existing_page.max(page);
    // Completion is monotonic: never clear an existing read flag.
    let new_read =
        existing_read || read || (effective_num_pages > 0 && new_page >= effective_num_pages);
    // hasBeenOpened is sticky once set.
    let new_hbo = existing_hbo || new_page > 0 || new_read;

    let content_changed =
        new_page != existing_page || new_read != existing_read || new_hbo != existing_hbo;

    // lastTimeOpened only advances. Avoid synthesizing "now" (and the resulting
    // write churn) when nothing else changed.
    let candidate_ts = match last_time_opened {
        Some(ts) => Some(ts),
        None if content_changed => Some(now_epoch_secs()),
        None => None,
    };
    let new_lto = match (existing_lto, candidate_ts) {
        (Some(e), Some(c)) => Some(e.max(c)),
        (Some(e), None) => Some(e),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    };

    if !content_changed && new_lto == existing_lto {
        tracing::debug!(comic_info_id, "no .ydb change needed; skipping update");
        return Ok(());
    }

    conn.execute(
        "UPDATE comic_info SET currentPage = ?1, read = ?2, hasBeenOpened = ?3, lastTimeOpened = ?4 WHERE id = ?5",
        rusqlite::params![new_page, new_read as i32, new_hbo as i32, new_lto, comic_info_id],
    )?;

    tracing::debug!(
        comic_info_id,
        page = new_page,
        read = new_read as i32,
        "wrote progress to .ydb"
    );
    Ok(())
}

/// Reconcile one comic across YACReader and Stump, resolving page and completion
/// as INDEPENDENT dimensions (fixes C2/H4):
///   * `final_page     = max(yac.current_page, stump_page)`
///   * `final_complete = yac.read || stump.is_complete()`  (monotonic)
/// Stump is pushed to whenever it is behind on either dimension; YACReader is
/// pulled to whenever it is behind on either dimension. A single comic can
/// therefore require both a push and a pull at once.
pub fn compute_bidirectional_delta(
    yac: &ComicProgress,
    stump: &StumpMedia,
    ydb_path: &str,
) -> BidirectionalDelta {
    let stump_page = stump.current_page();
    let stump_complete = stump.is_complete();

    let final_page = yac.current_page.max(stump_page);
    let final_complete = yac.read || stump_complete;

    // Push to Stump when Stump is behind on page and/or completion.
    let push_page = if stump_page < final_page {
        Some(final_page)
    } else {
        None
    };
    let push_mark_complete = final_complete && !stump_complete;
    let push = if push_page.is_some() || push_mark_complete {
        Some(PushAction {
            page: push_page,
            mark_complete: push_mark_complete,
        })
    } else {
        None
    };

    // Pull to YAC when YAC is behind on page and/or completion.
    let pull_page = if yac.current_page < final_page {
        Some(final_page)
    } else {
        None
    };
    let set_read = final_complete && !yac.read;
    // When pulling a Stump-side completion, record a completion time if Stump
    // reports one.
    let last_opened = if set_read && stump_complete {
        stump
            .read_progresses
            .first()
            .and_then(|rp| rp.completed_at.as_ref())
            .map(|_| now_epoch_secs())
    } else {
        None
    };
    let pull = if pull_page.is_some() || set_read {
        Some(PullAction {
            page: pull_page,
            set_read,
            last_opened,
        })
    } else {
        None
    };

    BidirectionalDelta {
        comic_info_id: yac.comic_info_id,
        stump_media_id: stump.id.clone(),
        ydb_path: ydb_path.to_string(),
        num_pages: yac.num_pages,
        push,
        pull,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ReadProgress;

    #[test]
    fn test_normalize_path_with_dir() {
        assert_eq!(
            normalize_path("Marvel/Spider-Man", "001.cbz"),
            "Marvel/Spider-Man/001.cbz"
        );
    }

    #[test]
    fn test_normalize_path_empty_dir() {
        assert_eq!(normalize_path("", "001.cbz"), "001.cbz");
    }

    #[test]
    fn test_normalize_path_nested() {
        assert_eq!(
            normalize_path("DC/Batman/Year One", "chapter1.cbz"),
            "DC/Batman/Year One/chapter1.cbz"
        );
    }

    fn make_comic(id: i64, path: &str, page: i32, read: bool) -> ComicProgress {
        ComicProgress {
            comic_info_id: id,
            comic_id: id + 1000,
            current_page: page,
            num_pages: 20,
            read,
            has_been_opened: true,
            last_time_opened: Some(1700000000),
            hash: Some("abc123".into()),
            relative_path: path.to_string(),
        }
    }

    fn make_stump_media(id: &str, path: &str, page: i32, complete: bool) -> StumpMedia {
        StumpMedia {
            id: id.to_string(),
            name: "test".into(),
            pages: 20,
            path: path.to_string(),
            read_progresses: vec![ReadProgress {
                page,
                percentage_completed: Some(if complete { 100.0 } else { (page as f64 / 20.0) * 100.0 }),
                is_completed: complete,
                epub_cfi: None,
                completed_at: if complete {
                    Some("2024-01-01".into())
                } else {
                    None
                },
                updated_at: None,
            }],
        }
    }

    #[test]
    fn test_match_comics_exact_path() {
        let yac = vec![make_comic(1, "Marvel/Spider-Man/001.cbz", 5, false)];
        let stump = vec![make_stump_media(
            "sm-1",
            "/srv/comics/Marvel/Spider-Man/001.cbz",
            3,
            false,
        )];

        let matches = match_comics(&yac, &stump, "/srv/comics");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0.comic_info_id, 1);
        assert_eq!(matches[0].1.id, "sm-1");
    }

    #[test]
    fn test_match_comics_no_match() {
        let yac = vec![make_comic(1, "Marvel/001.cbz", 5, false)];
        let stump = vec![make_stump_media("sm-1", "/srv/comics/DC/001.cbz", 3, false)];

        let matches = match_comics(&yac, &stump, "/srv/comics");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_match_comics_multiple() {
        let yac = vec![
            make_comic(1, "Marvel/001.cbz", 5, false),
            make_comic(2, "Marvel/002.cbz", 10, true),
            make_comic(3, "DC/001.cbz", 1, false),
        ];
        let stump = vec![
            make_stump_media("sm-1", "/srv/comics/Marvel/001.cbz", 3, false),
            make_stump_media("sm-2", "/srv/comics/Marvel/002.cbz", 5, false),
        ];

        let matches = match_comics(&yac, &stump, "/srv/comics");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_match_comics_trailing_slash_in_library_path() {
        let yac = vec![make_comic(1, "Marvel/001.cbz", 5, false)];
        let stump = vec![make_stump_media(
            "sm-1",
            "/srv/comics/Marvel/001.cbz",
            3,
            false,
        )];

        let matches = match_comics(&yac, &stump, "/srv/comics/");
        assert_eq!(matches.len(), 1);
    }

    fn make_lib_config(yac_library_id: i64, library_root: &str) -> LibraryConfig {
        LibraryConfig {
            yac_library_id,
            ydb_path: format!("{library_root}/.yacreaderlibrary/library.ydb"),
            library_root: library_root.to_string(),
            stump_library_id: String::new(),
            stump_library_path: String::new(),
        }
    }

    fn make_stump_library(id: &str, name: &str, path: &str) -> StumpLibrary {
        StumpLibrary {
            id: id.to_string(),
            name: name.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn test_normalize_fs_path() {
        assert_eq!(normalize_fs_path("/srv/comics/"), "/srv/comics");
        assert_eq!(normalize_fs_path("/srv/comics"), "/srv/comics");
        assert_eq!(normalize_fs_path("/srv/comics///"), "/srv/comics");
        assert_eq!(normalize_fs_path("/"), "/");
        assert_eq!(normalize_fs_path(""), "");
    }

    #[test]
    fn test_match_stump_library_by_path() {
        let yac = make_lib_config(1, "/srv/comics/"); // trailing slash differs
        let libs = vec![
            make_stump_library("lib-1", "Comics", "/srv/comics"),
            make_stump_library("lib-2", "Manga", "/srv/manga"),
        ];
        let found = match_stump_library(&yac, &libs).expect("expected a path match");
        assert_eq!(found.id, "lib-1");
    }

    #[test]
    fn test_match_stump_library_by_name_fallback() {
        // Paths differ across machines; fall back to the folder name.
        let yac = make_lib_config(1, "/Users/me/Comics");
        let libs = vec![
            make_stump_library("lib-1", "Manga", "/srv/manga"),
            make_stump_library("lib-2", "Comics", "/srv/data/Comics"),
        ];
        let found = match_stump_library(&yac, &libs).expect("expected a name fallback match");
        assert_eq!(found.id, "lib-2");
    }

    #[test]
    fn test_match_stump_library_none() {
        let yac = make_lib_config(1, "/Users/me/Nope");
        let libs = vec![make_stump_library("lib-1", "Comics", "/srv/comics")];
        assert!(match_stump_library(&yac, &libs).is_none());
    }

    #[tokio::test]
    async fn test_resolve_library_configs_auto_and_override() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("ListLibraries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "libraries": { "nodes": [
                    { "id": "stump-comics", "name": "Comics", "path": "/srv/comics" },
                    { "id": "stump-manga", "name": "Manga", "path": "/srv/manga" }
                ] } }
            })))
            .mount(&server)
            .await;

        let client = StumpClient::new(server.uri(), "k".into(), "u".into()).unwrap();

        let libraries = vec![
            // (1) auto-discover by path
            make_lib_config(1, "/srv/comics"),
            // (2) explicit override is honored verbatim (no discovery)
            LibraryConfig {
                yac_library_id: 2,
                ydb_path: "/b/.ydb".into(),
                library_root: "/whatever".into(),
                stump_library_id: "manual-id".into(),
                stump_library_path: "/manual/path".into(),
            },
            // (3) auto-discover with no possible match → dropped, not fatal
            make_lib_config(3, "/no/match/here"),
        ];

        let resolved = resolve_library_configs(&client, libraries).await;
        assert_eq!(resolved.len(), 2, "unmatched auto-discover library is dropped");

        let lib1 = resolved.iter().find(|l| l.yac_library_id == 1).unwrap();
        assert_eq!(lib1.stump_library_id, "stump-comics");
        assert_eq!(lib1.stump_library_path, "/srv/comics");

        let lib2 = resolved.iter().find(|l| l.yac_library_id == 2).unwrap();
        assert_eq!(lib2.stump_library_id, "manual-id", "override preserved");
        assert_eq!(lib2.stump_library_path, "/manual/path");

        assert!(resolved.iter().all(|l| l.yac_library_id != 3));
    }

    #[tokio::test]
    async fn test_resolve_library_configs_skips_network_when_all_explicit() {
        // No library needs discovery → list_libraries is never called, so a
        // server that would fail the call is irrelevant.
        let client = StumpClient::new("http://127.0.0.1:1".into(), "k".into(), "u".into()).unwrap();
        let libraries = vec![LibraryConfig {
            yac_library_id: 1,
            ydb_path: "/a/.ydb".into(),
            library_root: "/srv/comics".into(),
            stump_library_id: "explicit".into(),
            stump_library_path: "/srv/comics".into(),
        }];
        let resolved = resolve_library_configs(&client, libraries).await;
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].stump_library_id, "explicit");
    }

    #[test]
    fn test_delta_yac_ahead() {
        let comic = make_comic(1, "test.cbz", 10, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 5, false);

        let delta = compute_delta(&comic, &media).unwrap();
        assert_eq!(delta.stump_media_id, "sm-1");
        assert_eq!(delta.new_page, Some(10));
        assert!(!delta.should_mark_complete);
    }

    #[test]
    fn test_delta_stump_ahead() {
        let comic = make_comic(1, "test.cbz", 5, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);

        let delta = compute_delta(&comic, &media);
        assert!(delta.is_none());
    }

    #[test]
    fn test_delta_equal() {
        let comic = make_comic(1, "test.cbz", 10, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);

        let delta = compute_delta(&comic, &media);
        assert!(delta.is_none());
    }

    #[test]
    fn test_delta_yac_complete_stump_not() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 20, false);

        let delta = compute_delta(&comic, &media).unwrap();
        assert!(delta.new_page.is_none());
        assert!(delta.should_mark_complete);
    }

    #[test]
    fn test_delta_both_complete() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 20, true);

        let delta = compute_delta(&comic, &media);
        assert!(delta.is_none());
    }

    #[test]
    fn test_delta_yac_ahead_and_complete() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);

        let delta = compute_delta(&comic, &media).unwrap();
        assert_eq!(delta.new_page, Some(20));
        assert!(delta.should_mark_complete);
    }

    #[test]
    fn test_delta_no_stump_progress() {
        let comic = make_comic(1, "test.cbz", 5, false);
        let media = StumpMedia {
            id: "sm-1".into(),
            name: "test".into(),
            pages: 20,
            path: "/comics/test.cbz".into(),
            read_progresses: vec![],
        };

        let delta = compute_delta(&comic, &media).unwrap();
        assert_eq!(delta.new_page, Some(5));
        assert!(!delta.should_mark_complete);
    }

    #[test]
    fn test_compute_bidirectional_yac_ahead() {
        let comic = make_comic(1, "test.cbz", 15, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 5, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        let push = delta.push.expect("expected a push");
        assert_eq!(push.page, Some(15));
        assert!(!push.mark_complete);
        assert!(delta.pull.is_none());
    }

    #[test]
    fn test_compute_bidirectional_stump_ahead() {
        let comic = make_comic(1, "test.cbz", 5, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 15, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        let pull = delta.pull.expect("expected a pull");
        assert_eq!(pull.page, Some(15));
        assert!(!pull.set_read);
        assert!(delta.push.is_none());
    }

    #[test]
    fn test_compute_bidirectional_equal() {
        let comic = make_comic(1, "test.cbz", 10, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert!(delta.push.is_none());
        assert!(delta.pull.is_none());
    }

    #[test]
    fn test_compute_bidirectional_yac_complete_stump_not() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 20, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        let push = delta.push.expect("expected a push");
        assert!(push.mark_complete);
        assert!(push.page.is_none());
        // YAC is already read, so nothing to pull.
        assert!(delta.pull.is_none());
    }

    #[test]
    fn test_compute_bidirectional_stump_complete_yac_not() {
        let comic = make_comic(1, "test.cbz", 20, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 20, true);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        let pull = delta.pull.expect("expected a pull");
        assert!(pull.set_read);
        assert!(pull.page.is_none());
        // Stump is already complete, so nothing to push.
        assert!(delta.push.is_none());
    }

    #[test]
    fn test_compute_bidirectional_both_complete() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 20, true);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert!(delta.push.is_none());
        assert!(delta.pull.is_none());
    }

    #[test]
    fn test_compute_bidirectional_yac_ahead_and_complete() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        let push = delta.push.expect("expected a push");
        assert_eq!(push.page, Some(20));
        assert!(push.mark_complete);
        assert!(delta.pull.is_none());
    }

    #[test]
    fn test_compute_bidirectional_stump_ahead_and_complete() {
        let comic = make_comic(1, "test.cbz", 5, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 18, true);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        let pull = delta.pull.expect("expected a pull");
        assert_eq!(pull.page, Some(18));
        assert!(pull.set_read);
        assert!(delta.push.is_none());
    }

    #[test]
    fn test_compute_bidirectional_independent_page_and_completion() {
        // YAC is ahead on page; Stump holds completion. The OLD code chose a
        // single direction "by page", so Stump's completion was never pulled to
        // YAC. With independent dimensions, the page is pushed to Stump AND the
        // completion is pulled to YAC.
        let comic = make_comic(1, "test.cbz", 18, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, true);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");

        let push = delta.push.expect("expected a push (page to Stump)");
        assert_eq!(push.page, Some(18));
        assert!(!push.mark_complete, "Stump is already complete");

        let pull = delta.pull.expect("expected a pull (completion to YAC)");
        assert!(pull.set_read, "Stump's completion must propagate to YAC");
        assert!(pull.page.is_none(), "YAC is already page-ahead");
    }

    #[test]
    fn test_write_yac_progress() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();

        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE comic_info (
                id INTEGER PRIMARY KEY,
                currentPage INTEGER DEFAULT 0,
                numPages INTEGER DEFAULT 20,
                read INTEGER DEFAULT 0,
                hasBeenOpened INTEGER DEFAULT 0,
                lastTimeOpened INTEGER,
                hash TEXT
            );
            CREATE TABLE comic (
                id INTEGER PRIMARY KEY,
                comicInfoId INTEGER,
                path TEXT,
                fileName TEXT
            );
            INSERT INTO comic_info (id, currentPage, numPages, read, hasBeenOpened) VALUES (1, 0, 20, 0, 0);
            INSERT INTO comic (id, comicInfoId, path, fileName) VALUES (1, 1, 'Marvel', 'test.cbz');",
        )
        .unwrap();
        drop(conn);

        write_yac_progress(path, 1, 15, 20, false, Some(1700000000)).unwrap();

        let conn = rusqlite::Connection::open(path).unwrap();
        let (page, read, opened, ts): (i32, i32, i32, i64) = conn
            .query_row(
                "SELECT currentPage, read, hasBeenOpened, lastTimeOpened FROM comic_info WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        assert_eq!(page, 15);
        assert_eq!(read, 0);
        assert_eq!(opened, 1);
        assert_eq!(ts, 1700000000);
    }

    #[test]
    fn test_write_yac_progress_auto_complete() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();

        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE comic_info (
                id INTEGER PRIMARY KEY,
                currentPage INTEGER DEFAULT 0,
                numPages INTEGER DEFAULT 20,
                read INTEGER DEFAULT 0,
                hasBeenOpened INTEGER DEFAULT 0,
                lastTimeOpened INTEGER,
                hash TEXT
            );
            INSERT INTO comic_info (id, currentPage, numPages) VALUES (1, 0, 20);",
        )
        .unwrap();
        drop(conn);

        write_yac_progress(path, 1, 20, 20, false, None).unwrap();

        let conn = rusqlite::Connection::open(path).unwrap();
        let read: i32 = conn
            .query_row(
                "SELECT read FROM comic_info WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(read, 1, "should auto-set read when page >= num_pages");
    }

    #[test]
    fn test_write_yac_progress_not_found() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();

        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE comic_info (
                id INTEGER PRIMARY KEY,
                currentPage INTEGER DEFAULT 0,
                numPages INTEGER DEFAULT 20,
                read INTEGER DEFAULT 0,
                hasBeenOpened INTEGER DEFAULT 0,
                lastTimeOpened INTEGER,
                hash TEXT
            );",
        )
        .unwrap();
        drop(conn);

        let result = write_yac_progress(path, 999, 5, 20, false, None);
        assert!(result.is_err());
    }

    /// Create a one-row `.ydb` for write tests and return its path-keeping temp file.
    fn ydb_with_comic(
        current_page: i32,
        num_pages: i32,
        read: i32,
        has_been_opened: i32,
    ) -> tempfile::NamedTempFile {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE comic_info (
                id INTEGER PRIMARY KEY,
                currentPage INTEGER DEFAULT 0,
                numPages INTEGER DEFAULT 20,
                read INTEGER DEFAULT 0,
                hasBeenOpened INTEGER DEFAULT 0,
                lastTimeOpened INTEGER,
                hash TEXT
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO comic_info (id, currentPage, numPages, read, hasBeenOpened) VALUES (1, ?1, ?2, ?3, ?4)",
            rusqlite::params![current_page, num_pages, read, has_been_opened],
        )
        .unwrap();
        drop(conn);
        tmp
    }

    fn read_row(path: &str) -> (i32, i32, i32) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row(
            "SELECT currentPage, read, hasBeenOpened FROM comic_info WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    }

    /// Regression for C2: pulling a page from Stump (which is not complete) must
    /// never clear an existing YAC `read = 1`. The OLD blind-overwrite set read=0.
    #[test]
    fn test_no_read_flag_wipe_on_pull() {
        // YAC: read=1, currentPage=5. Stump: page=10, not complete, numPages=20.
        let tmp = ydb_with_comic(5, 20, 1, 1);
        let path = tmp.path().to_str().unwrap();

        // The pull action for this scenario is page=10, set_read=false.
        write_yac_progress(path, 1, 10, 20, false, None).unwrap();

        let (page, read, _opened) = read_row(path);
        assert_eq!(page, 10, "page should advance to Stump's page");
        assert_eq!(read, 1, "existing read flag must be preserved (C2 regression)");
    }

    /// Completion converges in both directions regardless of which side is
    /// page-ahead (C2/H4): the side lacking completion receives it.
    #[test]
    fn test_completion_converges_both_ways() {
        // Case A: Stump complete but page-behind YAC. YAC must end read=1, and
        // Stump's page catches up (it is already complete, so no re-mark).
        let comic = make_comic(1, "test.cbz", 20, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, true);
        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        let pull = delta.pull.expect("case A: expected a pull");
        assert!(pull.set_read, "case A: YAC must become read");
        let push = delta.push.expect("case A: expected a page push");
        assert_eq!(push.page, Some(20), "case A: Stump page catches up");
        assert!(!push.mark_complete, "case A: Stump already complete");

        // Mirror case B: YAC complete (read=1) but page-behind Stump. Stump must
        // get mark_complete, and applying the pull preserves YAC's read flag
        // while advancing its page.
        let comic = make_comic(1, "test.cbz", 5, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);
        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        let push = delta.push.expect("case B: expected a push");
        assert!(push.mark_complete, "case B: Stump must be marked complete");
        let pull = delta.pull.expect("case B: expected a page pull");
        assert_eq!(pull.page, Some(10), "case B: YAC page catches up");
        assert!(!pull.set_read, "case B: YAC already read");

        // Apply case B's pull to a real .ydb row (read=1, page=5) and confirm
        // the read flag survives and the page advances.
        let tmp = ydb_with_comic(5, 20, 1, 1);
        let path = tmp.path().to_str().unwrap();
        let page = pull.page.unwrap_or(comic.current_page);
        write_yac_progress(path, comic.comic_info_id, page, comic.num_pages, pull.set_read, pull.last_opened)
            .unwrap();
        let (page, read, _opened) = read_row(path);
        assert_eq!(page, 10);
        assert_eq!(read, 1, "case B: YAC read flag preserved");
    }
}
