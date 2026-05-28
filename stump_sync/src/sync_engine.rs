use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::OpenFlags;

use crate::config::LibraryConfig;
use crate::mapping_db::MappingDb;
use crate::stump_client::StumpClient;
use crate::types::{
    BidirectionalDelta, ComicProgress, StumpMedia, SyncDelta, SyncDirection, SyncError, SyncReport,
};

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

    pub async fn push_single(
        &self,
        library_id: i64,
        comic_id: i64,
    ) -> Result<(), SyncError> {
        let lib_config = self
            .libraries
            .iter()
            .find(|l| {
                self.mapping_db
                    .ensure_library_mapping(
                        &l.ydb_path,
                        &l.stump_library_id,
                        &l.stump_library_path,
                    )
                    .ok()
                    == Some(library_id)
            })
            .ok_or_else(|| SyncError::Config(format!("library mapping {library_id} not found")))?
            .clone();

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
                self.build_mappings(&lib_config, library_id).await?;
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

            match delta.direction {
                SyncDirection::PushToStump if !pull_only => {
                    self.apply_push_delta(&delta, report).await;
                }
                SyncDirection::PullToYac => {
                    self.apply_pull_delta(&delta, comic.current_page, report);
                }
                _ => {}
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

    async fn apply_push_delta(&self, delta: &BidirectionalDelta, report: &mut SyncReport) {
        if let Some(page) = delta.push_page {
            tracing::info!(media_id = %delta.stump_media_id, page, "pushing page progress");
            if let Err(e) = self.client.update_progress(&delta.stump_media_id, page).await {
                tracing::warn!(media_id = %delta.stump_media_id, error = %e, "failed to push page");
                report.errors.push(format!("{}: {e}", delta.stump_media_id));
                return;
            }
            report.pages_pushed += 1;
        }
        if delta.push_mark_complete {
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
        current_yac_page: i32,
        report: &mut SyncReport,
    ) {
        let page = delta.pull_page.unwrap_or(current_yac_page);
        let has_page_change = delta.pull_page.is_some();
        tracing::info!(
            comic_info_id = delta.comic_info_id,
            page,
            read = delta.pull_read,
            "pulling progress to YAC"
        );
        match write_yac_progress(
            &delta.ydb_path,
            delta.comic_info_id,
            page,
            delta.num_pages,
            delta.pull_read,
            delta.pull_last_opened,
        ) {
            Ok(()) => {
                if has_page_change {
                    report.pages_pulled += 1;
                }
                if delta.pull_read {
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

    let has_been_opened: i32 = if page > 0 { 1 } else { 0 };
    let is_read: i32 = if read || (num_pages > 0 && page >= num_pages) {
        1
    } else {
        0
    };
    let timestamp = last_time_opened.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });

    let updated = conn.execute(
        "UPDATE comic_info SET currentPage = ?1, read = ?2, hasBeenOpened = ?3, lastTimeOpened = ?4 WHERE id = ?5",
        rusqlite::params![page, is_read, has_been_opened, timestamp, comic_info_id],
    )?;

    if updated == 0 {
        return Err(SyncError::Database(format!(
            "comic_info_id {comic_info_id} not found in {ydb_path}"
        )));
    }

    tracing::debug!(
        comic_info_id,
        page,
        read = is_read,
        "wrote progress to .ydb"
    );
    Ok(())
}

pub fn compute_bidirectional_delta(
    yac: &ComicProgress,
    stump: &StumpMedia,
    ydb_path: &str,
) -> BidirectionalDelta {
    let stump_page = stump.current_page();
    let stump_complete = stump.is_complete();

    let yac_page_ahead = yac.current_page > stump_page;
    let stump_page_ahead = stump_page > yac.current_page;

    let yac_has_completion = yac.read && !stump_complete;
    let stump_has_completion = stump_complete && !yac.read;

    let mut delta = BidirectionalDelta {
        direction: SyncDirection::NoChange,
        comic_info_id: yac.comic_info_id,
        stump_media_id: stump.id.clone(),
        ydb_path: ydb_path.to_string(),
        num_pages: yac.num_pages,
        push_page: None,
        push_mark_complete: false,
        pull_page: None,
        pull_read: false,
        pull_last_opened: None,
    };

    if yac_page_ahead {
        delta.direction = SyncDirection::PushToStump;
        delta.push_page = Some(yac.current_page);
        if yac_has_completion {
            delta.push_mark_complete = true;
        }
    } else if stump_page_ahead {
        delta.direction = SyncDirection::PullToYac;
        delta.pull_page = Some(stump_page);
        if stump_has_completion {
            delta.pull_read = true;
        }
    } else if yac_has_completion {
        delta.direction = SyncDirection::PushToStump;
        delta.push_mark_complete = true;
    } else if stump_has_completion {
        delta.direction = SyncDirection::PullToYac;
        delta.pull_read = true;
        delta.pull_last_opened = stump
            .read_progresses
            .first()
            .and_then(|rp| rp.completed_at.as_ref())
            .and_then(|_| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs() as i64)
            });
    }

    delta
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
        assert_eq!(delta.direction, SyncDirection::PushToStump);
        assert_eq!(delta.push_page, Some(15));
        assert!(!delta.push_mark_complete);
        assert!(delta.pull_page.is_none());
    }

    #[test]
    fn test_compute_bidirectional_stump_ahead() {
        let comic = make_comic(1, "test.cbz", 5, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 15, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert_eq!(delta.direction, SyncDirection::PullToYac);
        assert_eq!(delta.pull_page, Some(15));
        assert!(!delta.pull_read);
        assert!(delta.push_page.is_none());
    }

    #[test]
    fn test_compute_bidirectional_equal() {
        let comic = make_comic(1, "test.cbz", 10, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert_eq!(delta.direction, SyncDirection::NoChange);
    }

    #[test]
    fn test_compute_bidirectional_yac_complete_stump_not() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 20, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert_eq!(delta.direction, SyncDirection::PushToStump);
        assert!(delta.push_mark_complete);
        assert!(delta.push_page.is_none());
    }

    #[test]
    fn test_compute_bidirectional_stump_complete_yac_not() {
        let comic = make_comic(1, "test.cbz", 20, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 20, true);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert_eq!(delta.direction, SyncDirection::PullToYac);
        assert!(delta.pull_read);
        assert!(delta.pull_page.is_none());
    }

    #[test]
    fn test_compute_bidirectional_both_complete() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 20, true);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert_eq!(delta.direction, SyncDirection::NoChange);
    }

    #[test]
    fn test_compute_bidirectional_yac_ahead_and_complete() {
        let comic = make_comic(1, "test.cbz", 20, true);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, false);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert_eq!(delta.direction, SyncDirection::PushToStump);
        assert_eq!(delta.push_page, Some(20));
        assert!(delta.push_mark_complete);
    }

    #[test]
    fn test_compute_bidirectional_stump_ahead_and_complete() {
        let comic = make_comic(1, "test.cbz", 5, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 18, true);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert_eq!(delta.direction, SyncDirection::PullToYac);
        assert_eq!(delta.pull_page, Some(18));
        assert!(delta.pull_read);
    }

    #[test]
    fn test_compute_bidirectional_page_priority_over_completion() {
        // YAC has higher page, Stump has completion — page wins (PushToStump)
        let comic = make_comic(1, "test.cbz", 18, false);
        let media = make_stump_media("sm-1", "/comics/test.cbz", 10, true);

        let delta = compute_bidirectional_delta(&comic, &media, "/tmp/test.ydb");
        assert_eq!(delta.direction, SyncDirection::PushToStump);
        assert_eq!(delta.push_page, Some(18));
        assert!(!delta.push_mark_complete);
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
}
