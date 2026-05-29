use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{OpenFlags, OptionalExtension};

use crate::config::LibraryConfig;
use crate::mapping_db::MappingDb;
use crate::stump_client::StumpClient;
use crate::types::{
    BidirectionalDelta, ComicProgress, PullAction, PushAction, StumpLibrary, StumpMedia, SyncDelta,
    SyncError, SyncReport, SyncState,
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

        // M3: fetch Stump media ONCE and reuse it for both mapping-building (on a
        // cache miss) and the lookup below (was fetched inside build_mappings AND
        // again here).
        let stump_media_list = self
            .client
            .get_library_media(&lib_config.stump_library_id)
            .await?;

        let stump_media_id = match self.mapping_db.get_stump_id(comic.comic_info_id)? {
            Some(id) => id,
            None => {
                tracing::info!(comic_id, "no mapping found, building mappings");
                self.build_mappings(&lib_config, lib_mapping_id, &comics, &stump_media_list)?;
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
        // M3: fetch the .ydb comics and Stump media ONCE, then reuse the same
        // data for both mapping-building and the push loop (was fetched twice).
        let comics = read_yac_comics(&lib_config.ydb_path)?;
        let stump_media = self
            .client
            .get_library_media(&lib_config.stump_library_id)
            .await?;

        let matched = self.build_mappings(lib_config, lib_mapping_id, &comics, &stump_media)?;
        report.comics_matched += matched;

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

    /// Build (and refresh) YAC↔Stump media mappings from ALREADY-FETCHED data
    /// (M3): the caller fetches `yac_comics` + `stump_media` once and passes them
    /// in, so this no longer re-reads the .ydb or re-hits the Stump GraphQL API
    /// (the previous version double-fetched both right before the caller used
    /// them again).
    pub fn build_mappings(
        &self,
        lib_config: &LibraryConfig,
        lib_mapping_id: i64,
        yac_comics: &[ComicProgress],
        stump_media: &[StumpMedia],
    ) -> Result<u32, SyncError> {
        let matches = match_comics(yac_comics, stump_media, &lib_config.stump_library_path);

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
            let mapping_id = mapping_by_comic.get(&comic.comic_info_id).copied();

            // H6/M2: read the LAST CONVERGED state for this comic (page, read,
            // and the per-side source timestamps observed at that sync) and feed
            // it into reconciliation so a validated, same-side, timestamp-checked
            // re-read / unread can be honored. get_sync_state is read at the
            // start of this comic's reconciliation, before its own write below,
            // so there is no self-contamination. First sync (no row) => None =>
            // pure monotonic, no regression possible.
            let prev = match mapping_id {
                Some(id) => self.mapping_db.get_sync_state(id)?,
                None => None,
            };

            let delta =
                compute_bidirectional_delta(comic, media, &lib_config.ydb_path, prev.as_ref());

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

            // H6/M2: record the CONVERGED final state (same value on both sides)
            // plus the SOURCE timestamps just observed, so the next sync has an
            // honest baseline. Fixes M2 (was pre-sync per-side values + NULL
            // timestamps, and get_sync_state was never read in production).
            if let Some(id) = mapping_id {
                let yac_modified = comic.last_time_opened.map(|t| t.to_string());
                let stump_modified = media.stump_timestamp().map(|s| s.to_string());
                let _ = self.mapping_db.update_sync_state(
                    id,
                    delta.final_page,
                    delta.final_page,
                    delta.final_complete,
                    delta.final_complete,
                    yac_modified.as_deref(),
                    stump_modified.as_deref(),
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
            if let Err(e) = self.client.mark_complete(&delta.stump_media_id).await {
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
            pull.allow_regression,
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
                .mark_complete(&delta.stump_media_id)
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

/// Return `media_path` expressed relative to `library_path`, or `None` when the
/// media does not live under the library. Unlike a raw string prefix, this
/// requires a real PATH-COMPONENT boundary (M5): `/srv/comics` matches
/// `/srv/comics/x.cbz` but NOT the sibling `/srv/comics-extra/x.cbz`. Both paths
/// are normalized first (collapse `//`, trim a trailing `/`).
pub fn relative_under(library_path: &str, media_path: &str) -> Option<String> {
    let lib = normalize_fs_path(library_path);
    let media = normalize_fs_path(media_path);

    // Root library: every absolute media path is relative to "/".
    if lib == "/" {
        let rel = media.trim_start_matches('/');
        return if rel.is_empty() {
            None
        } else {
            Some(rel.to_string())
        };
    }

    // Requiring the `lib + "/"` prefix enforces the path boundary: a sibling
    // directory whose name merely starts with `lib` (…-extra) is rejected
    // because it lacks the separator right after `lib`.
    let prefix = format!("{lib}/");
    let relative = media.strip_prefix(&prefix)?;
    if relative.is_empty() {
        None
    } else {
        Some(relative.to_string())
    }
}

pub fn match_comics<'a>(
    yac_comics: &'a [ComicProgress],
    stump_media: &'a [StumpMedia],
    stump_library_path: &str,
) -> Vec<(&'a ComicProgress, &'a StumpMedia)> {
    let stump_by_relative: std::collections::HashMap<String, &StumpMedia> = stump_media
        .iter()
        .filter_map(|m| relative_under(stump_library_path, &m.path).map(|rel| (rel, m)))
        .collect();

    let mut matches = Vec::new();
    for comic in yac_comics {
        if let Some(media) = stump_by_relative.get(&comic.relative_path) {
            matches.push((comic, *media));
        }
    }
    matches
}

/// Normalize a filesystem path for comparison (M5): collapse any run of '/'
/// into a single '/', then trim a trailing '/' (without turning a bare "/" into
/// the empty string). NOTE: case- and Unicode-NFC-folding are deliberately NOT
/// applied — they are environment-dependent and would require a new dependency;
/// that remains a documented limitation.
pub fn normalize_fs_path(path: &str) -> String {
    let mut collapsed = String::with_capacity(path.len());
    let mut prev_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !prev_slash {
                collapsed.push('/');
            }
            prev_slash = true;
        } else {
            collapsed.push(ch);
            prev_slash = false;
        }
    }

    let trimmed = collapsed.trim_end_matches('/');
    if trimmed.is_empty() && !collapsed.is_empty() {
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

/// Apply pulled progress to the YACReader `.ydb` using a read-modify-write.
///
/// By DEFAULT (`allow_regression = false`) the write is monotonic and lossless
/// (C2/H4) — this guards EVERY normal pull:
///   * `currentPage` never regresses (`max(existing, incoming)`),
///   * an existing `read = 1` is never cleared,
///   * page-based auto-complete may RAISE `read`,
///   * `hasBeenOpened` is sticky once set,
///   * `lastTimeOpened` only advances.
///
/// When `allow_regression = true` the engine has computed an EXACT converged
/// state from a validated, same-side, timestamp-checked re-read / unread (H6),
/// so the monotonic page/read guards are bypassed: `currentPage` is set to
/// `page` verbatim (may be LOWER) and `read` is set to `read` verbatim (may be
/// CLEARED). `hasBeenOpened` stays sticky and `lastTimeOpened` still only
/// advances even in this mode (re-reading is still "having opened" the comic,
/// and never moving the clock backward keeps the H6 baseline monotone).
///
/// A `busy_timeout` is set (H3) so a lock held by the live server is waited out
/// rather than failing immediately. The UPDATE is skipped when nothing changed.
pub fn write_yac_progress(
    ydb_path: &str,
    comic_info_id: i64,
    page: i32,
    num_pages: i32,
    read: bool,
    allow_regression: bool,
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

    let (new_page, new_read) = if allow_regression {
        // Validated regression: trust the engine's exact converged values. Page
        // may drop below the existing page; read may be cleared. No page-based
        // auto-complete here (it would fight a deliberate re-read).
        (page, read)
    } else {
        // Monotonic, C2-safe: page never regresses; an existing read=1 is never
        // cleared; auto-complete may RAISE read.
        let new_page = existing_page.max(page);
        let new_read =
            existing_read || read || (effective_num_pages > 0 && new_page >= effective_num_pages);
        (new_page, new_read)
    };
    // hasBeenOpened is sticky once set (true even on a regression — re-reading a
    // comic does not un-open it).
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

/// True iff `current` is strictly newer than `baseline`, where both are YAC
/// `lastTimeOpened` epoch seconds (same clock, skew-safe). ANY missing value =>
/// NOT newer, so unverifiable data can never trigger a regression (H6 safety).
/// `update_sync_state` stores the baseline as a stringified i64, so it
/// round-trips through `parse`.
fn yac_ts_newer(current: Option<i64>, baseline: Option<&str>) -> bool {
    match (current, baseline.and_then(|s| s.parse::<i64>().ok())) {
        (Some(c), Some(b)) => c > b,
        _ => false,
    }
}

/// True iff `current` is strictly newer than `baseline`, where both are Stump
/// RFC3339 UTC timestamp strings emitted by the SAME server (so a lexical
/// compare equals a chronological one for the fixed format). ANY missing value
/// => NOT newer (H6 safety).
fn stump_ts_newer(current: Option<&str>, baseline: Option<&str>) -> bool {
    match (current, baseline) {
        (Some(c), Some(b)) => c > b,
        _ => false,
    }
}

/// Reconcile one comic across YACReader and Stump.
///
/// Page and completion are INDEPENDENT dimensions (C2/H4). The DEFAULT for each
/// is monotonic — `final_page = max(yac, stump)`, `final_complete = yac.read ||
/// stump.is_complete()` — which can never lose progress.
///
/// H6 layers INTENTIONAL-REGRESSION detection over that default, using the
/// previous converged `sync_state` (`prev`) as a baseline. The rule is
/// SAFETY-CRITICAL: a side may pull the other DOWN (lower page / cleared read)
/// ONLY when it ALONE moved backward since the last sync AND its OWN source
/// clock advanced past the recorded baseline (a same-side, skew-safe compare).
/// If both sides or neither regressed — or any needed timestamp is missing — we
/// keep the monotonic result, so stale/older data can NEVER regress or clear
/// anything (the exact C2 data-loss this guards against). With no `prev` (first
/// sync), no regression is possible.
///
/// Page convergence is clean: the lower page is propagated to BOTH sides, so the
/// next baseline matches and it sticks. Completion can only ever be ADDED on
/// Stump (`read_history` is append-only), so a YAC→Stump unread is honored for
/// the current cycle (Stump's history is NOT cleared) and Stump, as source of
/// truth, re-propagates completion on a later cycle — a documented limitation.
pub fn compute_bidirectional_delta(
    yac: &ComicProgress,
    stump: &StumpMedia,
    ydb_path: &str,
    prev: Option<&SyncState>,
) -> BidirectionalDelta {
    let stump_page = stump.current_page();
    let stump_complete = stump.is_complete();
    let yac_page = yac.current_page;
    let yac_read = yac.read;
    let stump_ts = stump.stump_timestamp();

    // ---- PAGE dimension ----
    let mut final_page = yac_page.max(stump_page); // monotonic default
    let mut page_regress_stump = false; // a Stump re-read pulls YAC DOWN
    if let Some(prev) = prev {
        let last_page = prev.yac_current_page; // converged page at last sync
        let yac_back = yac_page < last_page
            && yac_ts_newer(yac.last_time_opened, prev.yac_last_modified.as_deref());
        let stump_back = stump_page < last_page
            && stump_ts_newer(stump_ts, prev.stump_last_modified.as_deref());
        if yac_back && !stump_back {
            final_page = yac_page; // YAC's re-read wins; the lower page propagates to Stump
        } else if stump_back && !yac_back {
            final_page = stump_page; // Stump's re-read wins; the lower page propagates to YAC
            page_regress_stump = true;
        }
        // both or neither => keep the monotonic max
    }

    // ---- COMPLETION dimension ----
    // Only the read -> unread transition is dangerous (unread -> read is always
    // monotonic-safe), so regression detection is gated on `last_read == true`.
    let mut final_complete = yac_read || stump_complete; // monotonic default
    let mut unread_regress_stump = false; // a Stump un-complete clears YAC read
    if let Some(prev) = prev {
        if prev.yac_read {
            let yac_unread = !yac_read
                && yac_ts_newer(yac.last_time_opened, prev.yac_last_modified.as_deref());
            let stump_unread = !stump_complete
                && stump_ts_newer(stump_ts, prev.stump_last_modified.as_deref());
            if yac_unread && !stump_unread {
                // YAC intentionally unread; don't pull Stump's (sticky) completion
                // back. Stump can't be un-completed, so it is left as-is.
                final_complete = false;
            } else if stump_unread && !yac_unread {
                // Stump intentionally un-completed; clear YAC's read via the
                // allow_regression pull below.
                final_complete = false;
                unread_regress_stump = true;
            }
        }
    }

    // ---- PUSH to Stump ----
    // Send the converged page whenever Stump's page differs from it: covers Stump
    // being behind (push UP) and a validated YAC re-read (push DOWN — final_page
    // only drops below stump_page via the yac_back branch). Completion is only
    // ever added, never removed.
    let push_page = if stump_page != final_page {
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

    // ---- PULL to YAC ----
    // A Stump-side regression is the only thing that may LOWER YAC's page or
    // CLEAR its read, and the only case allowed to bypass the C2 write-guard.
    let stump_regressed = page_regress_stump || unread_regress_stump;
    let pull = if stump_regressed {
        // Write the EXACT converged state to YAC, but only when it differs.
        let page = if yac_page != final_page {
            Some(final_page)
        } else {
            None
        };
        if page.is_some() || final_complete != yac_read {
            Some(PullAction {
                page,
                set_read: final_complete, // EXACT value in regression mode
                allow_regression: true,
                last_opened: None, // let the writer keep lastTimeOpened monotonic
            })
        } else {
            None
        }
    } else {
        // Normal monotonic pull (C2-safe): only ever RAISE page/read.
        let pull_page = if yac_page < final_page {
            Some(final_page)
        } else {
            None
        };
        let set_read = final_complete && !yac_read;
        // When pulling a Stump-side completion, record a completion time if Stump
        // reports one (completion lives in read_history; stump_complete => it is
        // non-empty).
        let last_opened = if set_read && stump_complete {
            stump
                .read_history
                .first()
                .and_then(|fs| fs.completed_at.as_ref())
                .map(|_| now_epoch_secs())
        } else {
            None
        };
        if pull_page.is_some() || set_read {
            Some(PullAction {
                page: pull_page,
                set_read,
                allow_regression: false,
                last_opened,
            })
        } else {
            None
        }
    };

    BidirectionalDelta {
        comic_info_id: yac.comic_info_id,
        stump_media_id: stump.id.clone(),
        ydb_path: ydb_path.to_string(),
        num_pages: yac.num_pages,
        push,
        pull,
        final_page,
        final_complete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActiveReadingSession, FinishedReadingSession};

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

    /// Build a `StumpMedia` with an active session at `page` and, when
    /// `complete`, a single finished session (completion = presence in
    /// `read_history`). The active session is always present so `current_page()`
    /// reflects `page` (these tests assert exact pages on both sides).
    fn make_stump_media(id: &str, path: &str, page: i32, complete: bool) -> StumpMedia {
        StumpMedia {
            id: id.to_string(),
            name: "test".into(),
            pages: 20,
            path: path.to_string(),
            read_progress: Some(ActiveReadingSession {
                page: Some(page),
                updated_at: None,
            }),
            read_history: if complete {
                vec![FinishedReadingSession {
                    completed_at: Some("2024-01-01".into()),
                }]
            } else {
                vec![]
            },
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
        // M5: runs of '/' anywhere collapse to a single separator.
        assert_eq!(normalize_fs_path("/srv//comics"), "/srv/comics");
        assert_eq!(normalize_fs_path("/srv///comics//"), "/srv/comics");
        assert_eq!(normalize_fs_path("//"), "/");
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
            read_progress: None,
            read_history: vec![],
        };

        let delta = compute_delta(&comic, &media).unwrap();
        assert_eq!(delta.new_page, Some(5));
        assert!(!delta.should_mark_complete);
    }

    #[test]
    fn test_compute_bidirectional_yac_ahead() {
        let comic = make_comic(1, "test.cbz", 15, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 5, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
        let push = delta.push.expect("expected a push");
        assert_eq!(push.page, Some(15));
        assert!(!push.mark_complete);
        assert!(delta.pull.is_none());
    }

    #[test]
    fn test_compute_bidirectional_stump_ahead() {
        let comic = make_comic(1, "test.cbz", 5, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 15, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
        let pull = delta.pull.expect("expected a pull");
        assert_eq!(pull.page, Some(15));
        assert!(!pull.set_read);
        assert!(delta.push.is_none());
    }

    #[test]
    fn test_compute_bidirectional_equal() {
        let comic = make_comic(1, "test.cbz", 10, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
        assert!(delta.push.is_none());
        assert!(delta.pull.is_none());
    }

    #[test]
    fn test_compute_bidirectional_yac_complete_stump_not() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 20, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
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

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
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

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
        assert!(delta.push.is_none());
        assert!(delta.pull.is_none());
    }

    #[test]
    fn test_compute_bidirectional_yac_ahead_and_complete() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
        let push = delta.push.expect("expected a push");
        assert_eq!(push.page, Some(20));
        assert!(push.mark_complete);
        assert!(delta.pull.is_none());
    }

    #[test]
    fn test_compute_bidirectional_stump_ahead_and_complete() {
        let comic = make_comic(1, "test.cbz", 5, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 18, true);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
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

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);

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

        write_yac_progress(path, 1, 15, 20, false, false, Some(1700000000)).unwrap();

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

        write_yac_progress(path, 1, 20, 20, false, false, None).unwrap();

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

        let result = write_yac_progress(path, 999, 5, 20, false, false, None);
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
        write_yac_progress(path, 1, 10, 20, false, false, None).unwrap();

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
        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
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
        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb", None);
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
        write_yac_progress(path, comic.comic_info_id, page, comic.num_pages, pull.set_read, pull.allow_regression, pull.last_opened)
            .unwrap();
        let (page, read, _opened) = read_row(path);
        assert_eq!(page, 10);
        assert_eq!(read, 1, "case B: YAC read flag preserved");
    }

    // ---- H6: hybrid re-read / unread conflict resolution ----

    /// A prior CONVERGED `sync_state` baseline (page/read are the same on both
    /// sides) plus the per-side source timestamps observed at that sync.
    fn make_sync_state(
        last_page: i32,
        last_read: bool,
        yac_ts: Option<&str>,
        stump_ts: Option<&str>,
    ) -> SyncState {
        SyncState {
            id: 1,
            media_mapping_id: 1,
            yac_current_page: last_page,
            stump_current_page: last_page,
            yac_read: last_read,
            stump_complete: last_read,
            yac_last_modified: yac_ts.map(str::to_string),
            stump_last_modified: stump_ts.map(str::to_string),
            last_synced_at: "2026-05-20T00:00:00Z".to_string(),
        }
    }

    /// A YAC comic with an explicit `last_time_opened` (the H6 source clock).
    fn comic_with_ts(page: i32, read: bool, last_time_opened: Option<i64>) -> ComicProgress {
        ComicProgress {
            comic_info_id: 1,
            comic_id: 1001,
            current_page: page,
            num_pages: 20,
            read,
            has_been_opened: true,
            last_time_opened,
            hash: Some("h".into()),
            relative_path: "test.cbz".into(),
        }
    }

    /// A Stump media with an explicit `readProgress.updatedAt` (the H6 source
    /// clock). `complete` adds a finished session to `read_history`.
    fn stump_with_ts(page: i32, complete: bool, updated_at: Option<&str>) -> StumpMedia {
        StumpMedia {
            id: "sm-1".into(),
            name: "test".into(),
            pages: 20,
            path: "/comics/test.cbz".into(),
            read_progress: Some(ActiveReadingSession {
                page: Some(page),
                updated_at: updated_at.map(str::to_string),
            }),
            read_history: if complete {
                vec![FinishedReadingSession {
                    completed_at: Some("2024-01-01".into()),
                }]
            } else {
                vec![]
            },
        }
    }

    /// YAC dropped below the last-synced page with a NEWER YAC timestamp while
    /// Stump stayed put: YAC's re-read is honored — its lower page propagates to
    /// Stump (push DOWN) and YAC keeps its position (no pull).
    #[test]
    fn test_reread_propagates_with_newer_same_side_ts() {
        let prev = make_sync_state(15, false, Some("1700000000"), Some("2026-05-20T10:00:00Z"));
        // YAC re-read back to page 3, opened more recently than the baseline.
        let yac = comic_with_ts(3, false, Some(1700000999));
        // Stump unchanged at page 15, same timestamp as the baseline (not newer).
        let stump = stump_with_ts(15, false, Some("2026-05-20T10:00:00Z"));

        let delta = compute_bidirectional_delta(&yac, &stump, "/tmp/test.ydb", Some(&prev));

        assert_eq!(delta.final_page, 3, "the validated YAC re-read wins the page");
        let push = delta.push.expect("Stump must be pulled DOWN to the re-read page");
        assert_eq!(push.page, Some(3));
        assert!(!push.mark_complete);
        assert!(
            delta.pull.is_none(),
            "YAC already holds the re-read page; nothing to pull"
        );
    }

    /// SAME lower page but an OLDER YAC timestamp: this is stale data, NOT a
    /// re-read. Monotonic max must win and YAC is raised back up — no regression
    /// (guards the C2 data-loss).
    #[test]
    fn test_stale_lower_page_does_not_regress() {
        let prev = make_sync_state(15, false, Some("1700001000"), Some("2026-05-20T10:00:00Z"));
        // YAC shows page 3 but was opened BEFORE the recorded baseline.
        let yac = comic_with_ts(3, false, Some(1700000000));
        let stump = stump_with_ts(15, false, Some("2026-05-20T10:00:00Z"));

        let delta = compute_bidirectional_delta(&yac, &stump, "/tmp/test.ydb", Some(&prev));

        assert_eq!(
            delta.final_page, 15,
            "stale lower page must NOT regress; monotonic max wins"
        );
        assert!(
            delta.push.is_none(),
            "Stump is at the converged page; nothing pushed"
        );
        let pull = delta.pull.expect("YAC is raised back up to the converged page");
        assert_eq!(pull.page, Some(15));
        assert!(
            !pull.allow_regression,
            "stale data never takes the regression write path (C2 guard intact)"
        );
    }

    /// First sync (no baseline): regression is impossible, so even a YAC page
    /// below Stump resolves by pure monotonic max.
    #[test]
    fn test_first_sync_no_regression() {
        let yac = comic_with_ts(3, false, Some(1700000000));
        let stump = stump_with_ts(15, false, Some("2026-05-20T10:00:00Z"));

        let delta = compute_bidirectional_delta(&yac, &stump, "/tmp/test.ydb", None);

        assert_eq!(delta.final_page, 15, "no baseline => monotonic max");
        let pull = delta.pull.expect("YAC is raised to the higher page");
        assert_eq!(pull.page, Some(15));
        assert!(!pull.allow_regression, "no baseline => no regression possible");
        assert!(delta.push.is_none());
    }

    /// Stump un-completed (newer Stump timestamp) while YAC stayed read: the
    /// validated regression clears YAC's read via the allow_regression pull, and
    /// the writer actually drops read=1 to 0.
    #[test]
    fn test_stump_unread_regression_clears_yac_read() {
        let prev = make_sync_state(20, true, Some("1700000000"), Some("2026-05-20T10:00:00Z"));
        // YAC still read, unchanged since the baseline.
        let yac = comic_with_ts(20, true, Some(1700000000));
        // Stump is no longer complete and was touched more recently.
        let stump = stump_with_ts(20, false, Some("2026-05-21T10:00:00Z"));

        let delta = compute_bidirectional_delta(&yac, &stump, "/tmp/test.ydb", Some(&prev));

        assert!(
            !delta.final_complete,
            "a newer Stump un-complete clears the converged read"
        );
        let pull = delta
            .pull
            .clone()
            .expect("YAC read must be cleared via a regression pull");
        assert!(
            pull.allow_regression,
            "clearing read requires the regression write path"
        );
        assert!(!pull.set_read, "exact converged read value is false (cleared)");
        assert!(delta.push.is_none(), "Stump cannot be (un)completed by a push");

        // The writer actually clears read=1 in this mode.
        let tmp = ydb_with_comic(20, 20, 1, 1);
        let path = tmp.path().to_str().unwrap();
        let page = pull.page.unwrap_or(yac.current_page);
        write_yac_progress(
            path,
            yac.comic_info_id,
            page,
            yac.num_pages,
            pull.set_read,
            pull.allow_regression,
            pull.last_opened,
        )
        .unwrap();
        let (_page, read, _opened) = read_row(path);
        assert_eq!(read, 0, "validated Stump un-complete clears YAC read");
    }

    /// Writer-level contrast: `allow_regression` lets the page drop; the default
    /// monotonic write keeps the higher existing page (C2).
    #[test]
    fn test_write_yac_progress_allow_regression_lowers_page() {
        let tmp = ydb_with_comic(15, 20, 0, 1);
        let path = tmp.path().to_str().unwrap();
        write_yac_progress(path, 1, 3, 20, false, true, Some(1700000500)).unwrap();
        let (page, read, _opened) = read_row(path);
        assert_eq!(page, 3, "allow_regression lets currentPage drop");
        assert_eq!(read, 0);

        let tmp2 = ydb_with_comic(15, 20, 0, 1);
        let path2 = tmp2.path().to_str().unwrap();
        write_yac_progress(path2, 1, 3, 20, false, false, Some(1700000500)).unwrap();
        let (page2, _read2, _opened2) = read_row(path2);
        assert_eq!(page2, 15, "default monotonic write keeps the higher page (C2)");
    }

    // ---- M5: path-boundary matching ----

    /// `/srv/comics` must NOT swallow the sibling directory `/srv/comics-extra`:
    /// a raw string prefix would false-match it.
    #[test]
    fn test_library_prefix_boundary() {
        let yac = vec![make_comic(1, "x.cbz", 5, false)];

        let sibling = vec![make_stump_media("sm-1", "/srv/comics-extra/x.cbz", 3, false)];
        let matches = match_comics(&yac, &sibling, "/srv/comics");
        assert!(
            matches.is_empty(),
            "a sibling dir sharing a name prefix must not match across the boundary"
        );

        // The genuine child still matches.
        let child = vec![make_stump_media("sm-2", "/srv/comics/x.cbz", 3, false)];
        let matches = match_comics(&yac, &child, "/srv/comics");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1.id, "sm-2");
    }

    /// Redundant '/' runs in either the library path or the media path normalize
    /// away so matching still succeeds.
    #[test]
    fn test_match_comics_collapses_double_slash() {
        let yac = vec![make_comic(1, "Marvel/001.cbz", 5, false)];
        let stump = vec![make_stump_media("sm-1", "/srv/comics//Marvel/001.cbz", 3, false)];
        let matches = match_comics(&yac, &stump, "/srv//comics/");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1.id, "sm-1");
    }
}
