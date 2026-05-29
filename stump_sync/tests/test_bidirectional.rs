mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use stump_sync::config::LibraryConfig;
use stump_sync::mapping_db::MappingDb;
use stump_sync::stump_client::StumpClient;
use stump_sync::sync_engine::SyncEngine;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path as req_path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A wiremock responder that returns a different body on each successive call,
/// clamping to the final entry once the list is exhausted. Used to simulate
/// Stump's state changing between two `sync_all` fetches.
struct SequencedResponder {
    responses: Vec<serde_json::Value>,
    calls: AtomicUsize,
}

impl Respond for SequencedResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        let i = idx.min(self.responses.len() - 1);
        ResponseTemplate::new(200).set_body_json(self.responses[i].clone())
    }
}

fn make_engine(
    server: &MockServer,
    mapping_db_path: &str,
    lib_configs: Vec<LibraryConfig>,
) -> SyncEngine {
    let client =
        StumpClient::new(server.uri(), "test-key".into(), "test-user".into()).unwrap();
    let mapping_db = MappingDb::open(mapping_db_path).unwrap();
    SyncEngine::new(client, mapping_db, lib_configs)
}

fn requests_containing(requests: &[wiremock::Request], needle: &str) -> Vec<String> {
    requests
        .iter()
        .filter_map(|r| {
            let body = String::from_utf8_lossy(&r.body).to_string();
            if body.contains(needle) {
                Some(body)
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn test_pull_when_stump_ahead() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(&conn, 1, "hash-1", "001.cbz", "Marvel", 5, 20, false, 1700000000);
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[mock_media_stump_ahead(
            "media-1",
            "/srv/comics/Marvel/001.cbz",
            20,
            20,
        )],
    )
    .await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        yac_library_id: 1,
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.sync_all().await.unwrap();

    let comic = read_comic_from_ydb(&ydb_path, 1);
    assert_eq!(comic.current_page, 20);

    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert!(updates.is_empty(), "expected no UpdateProgress mutations");

    assert_eq!(report.pages_pulled, 1);
    assert_eq!(report.pages_pushed, 0);
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn test_bidirectional_mixed() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    // A: YAC page 15/20, Stump page 5 → push to Stump
    insert_comic(
        &conn, 1, "hash-a", "001.cbz", "Marvel", 15, 20, false, 1700000000,
    );
    // B: YAC page 3/30, Stump page 20 → pull from Stump
    insert_comic(
        &conn, 2, "hash-b", "001.cbz", "DC", 3, 30, false, 1700000000,
    );
    // C: YAC page 10/20, Stump page 10 → no change
    insert_comic(
        &conn, 3, "hash-c", "001.cbz", "Manga", 10, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[
            MockMedia {
                id: "media-a".into(),
                name: "001.cbz".into(),
                pages: 20,
                path: "/srv/comics/Marvel/001.cbz".into(),
                read_page: Some(5),
                is_complete: false,
            },
            mock_media_stump_ahead("media-b", "/srv/comics/DC/001.cbz", 20, 30),
            MockMedia {
                id: "media-c".into(),
                name: "001.cbz".into(),
                pages: 20,
                path: "/srv/comics/Manga/001.cbz".into(),
                read_page: Some(10),
                is_complete: false,
            },
        ],
    )
    .await;
    mock_update_progress_response(&server).await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        yac_library_id: 1,
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.sync_all().await.unwrap();

    // A: pushed to Stump (mutation received for page 15)
    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert_eq!(updates.len(), 1);
    assert!(updates[0].contains("media-a"));
    assert!(updates[0].contains("\"page\":15"));

    // B: pulled to .ydb (now at page 20)
    let comic_b = read_comic_from_ydb(&ydb_path, 2);
    assert_eq!(comic_b.current_page, 20);

    // C: unchanged (no mutation, .ydb unchanged)
    let comic_c = read_comic_from_ydb(&ydb_path, 3);
    assert_eq!(comic_c.current_page, 10);
    assert!(updates.iter().all(|b| !b.contains("media-c")));

    assert_eq!(report.pages_pushed, 1);
    assert_eq!(report.pages_pulled, 1);
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn test_conflict_resolution_max_page() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-1", "001.cbz", "Comics", 10, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[mock_media_stump_ahead(
            "media-1",
            "/srv/comics/Comics/001.cbz",
            15,
            20,
        )],
    )
    .await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        yac_library_id: 1,
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.sync_all().await.unwrap();

    // Stump wins (higher page), .ydb updated to page 15
    let comic = read_comic_from_ydb(&ydb_path, 1);
    assert_eq!(comic.current_page, 15);

    // No mutation sent to Stump
    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert!(updates.is_empty());
    let completions = requests_containing(&requests, "MarkComplete");
    assert!(completions.is_empty());

    assert_eq!(report.pages_pulled, 1);
    assert_eq!(report.pages_pushed, 0);
}

#[tokio::test]
async fn test_completion_stump_to_yac() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-1", "001.cbz", "Comics", 20, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[mock_media_stump_complete(
            "media-1",
            "/srv/comics/Comics/001.cbz",
            20,
            20,
        )],
    )
    .await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        yac_library_id: 1,
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.sync_all().await.unwrap();

    let comic = read_comic_from_ydb(&ydb_path, 1);
    assert!(comic.read, "expected .ydb read=true after pulling completion");
    assert_eq!(comic.current_page, 20);

    assert_eq!(report.completions_pulled, 1);
    assert_eq!(report.pages_pulled, 0);
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn test_sync_state_tracking() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let ydb_str = ydb_path.to_str().unwrap().to_string();
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-1", "001.cbz", "Comics", 5, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[mock_media_stump_ahead(
            "media-1",
            "/srv/comics/Comics/001.cbz",
            10,
            20,
        )],
    )
    .await;

    let mapping_db_path = dir.path().join("mappings.db");
    let mapping_db_str = mapping_db_path.to_str().unwrap().to_string();
    let lib_config = LibraryConfig {
        yac_library_id: 1,
        ydb_path: ydb_str.clone(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(&server, &mapping_db_str, vec![lib_config]);

    let report = engine.sync_all().await.unwrap();
    assert!(report.errors.is_empty());

    // Query sync_state from the mapping DB
    let mapping_db = MappingDb::open(&mapping_db_str).unwrap();
    let lib_id = mapping_db
        .ensure_library_mapping(&ydb_str, "lib-1", "/srv/comics")
        .unwrap();
    let mappings = mapping_db.get_all_mappings(lib_id).unwrap();
    assert_eq!(mappings.len(), 1);

    let state = mapping_db
        .get_sync_state(mappings[0].id)
        .unwrap()
        .expect("sync_state should exist after sync_all");
    // M2/H6: sync_state now records the CONVERGED page/read (the same value on
    // both sides — here max(5, 10) = 10) plus the per-side SOURCE timestamps
    // observed at this sync (previously these were pre-sync per-side values and
    // the timestamps were always NULL).
    assert_eq!(state.yac_current_page, 10);
    assert_eq!(state.stump_current_page, 10);
    assert!(!state.yac_read);
    assert!(!state.stump_complete);
    assert_eq!(
        state.yac_last_modified.as_deref(),
        Some("1700000000"),
        "YAC lastTimeOpened recorded as the source timestamp"
    );
    assert_eq!(
        state.stump_last_modified.as_deref(),
        Some("2026-05-20T14:32:10Z"),
        "Stump readProgress.updatedAt recorded as the source timestamp"
    );
}

#[tokio::test]
async fn test_push_still_works() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    insert_comic(
        &conn, 1, "hash-1", "001.cbz", "Comics", 15, 20, false, 1700000000,
    );
    drop(conn);

    let server = MockServer::start().await;
    mock_library_media_response(
        &server,
        &[MockMedia {
            id: "media-1".into(),
            name: "001.cbz".into(),
            pages: 20,
            path: "/srv/comics/Comics/001.cbz".into(),
            read_page: Some(5),
            is_complete: false,
        }],
    )
    .await;
    mock_update_progress_response(&server).await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        yac_library_id: 1,
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(
        &server,
        mapping_db_path.to_str().unwrap(),
        vec![lib_config],
    );

    let report = engine.push_all().await.unwrap();

    // Mutation sent for page 15
    let requests = server.received_requests().await.unwrap();
    let updates = requests_containing(&requests, "UpdateProgress");
    assert_eq!(updates.len(), 1);
    assert!(updates[0].contains("\"page\":15"));

    // .ydb NOT modified (push_all doesn't pull)
    let comic = read_comic_from_ydb(&ydb_path, 1);
    assert_eq!(comic.current_page, 15);

    assert_eq!(report.pages_pushed, 1);
    assert_eq!(report.pages_pulled, 0);
    assert!(report.errors.is_empty());
}

/// A comic that is COMPLETE on Stump (readProgress=null, readHistory=[entry])
/// must pull read=1 and the last page into YAC — and must NOT trigger a
/// markMediaAsComplete back to Stump (Stump is already complete).
#[tokio::test]
async fn test_completed_on_stump_pulls_read() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    // YAC: page 5/20, not read.
    insert_comic(&conn, 1, "hash-1", "001.cbz", "Comics", 5, 20, false, 1700000000);
    drop(conn);

    let server = MockServer::start().await;
    // Stump: completed with no active session, pages=20.
    mock_library_media_response(
        &server,
        &[mock_media_completed_no_active(
            "media-1",
            "/srv/comics/Comics/001.cbz",
            20,
        )],
    )
    .await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        yac_library_id: 1,
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(&server, mapping_db_path.to_str().unwrap(), vec![lib_config]);

    let report = engine.sync_all().await.unwrap();

    // YAC pulled to read=1, page advanced to the last page (20).
    let comic = read_comic_from_ydb(&ydb_path, 1);
    assert!(comic.read, "Stump completion must pull read=1 into YAC");
    assert_eq!(comic.current_page, 20);

    // No completion push (Stump already complete) and no page push.
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests_containing(&requests, "MarkComplete").is_empty(),
        "must NOT re-complete an already-complete Stump comic"
    );
    assert!(
        requests_containing(&requests, "UpdateProgress").is_empty(),
        "no page push expected (Stump is at/after the final page)"
    );

    assert_eq!(report.completions_pulled, 1);
    assert_eq!(report.pages_pulled, 1);
    assert!(report.errors.is_empty());
}

/// Idempotency (C3): once we push a completion to Stump, a later sync must NOT
/// push it again after Stump reports the finished session. markMediaAsComplete
/// is NOT deduped server-side, so the guard `final_complete && !stump_complete`
/// is what prevents duplicate history entries.
#[tokio::test]
async fn test_complete_pushed_once_not_repeated() {
    let dir = TempDir::new().unwrap();
    let ydb_path = create_test_ydb(dir.path());
    let conn = open_ydb(&ydb_path);
    // YAC: read=1, page 20/20 (finished locally).
    insert_comic(&conn, 1, "hash-1", "001.cbz", "Comics", 20, 20, true, 1700000000);
    drop(conn);

    let server = MockServer::start().await;

    // 1st fetch: Stump in-progress at page 10, history empty (NOT complete).
    let in_progress = build_media_response_json(&[MockMedia {
        id: "media-1".into(),
        name: "001.cbz".into(),
        pages: 20,
        path: "/srv/comics/Comics/001.cbz".into(),
        read_page: Some(10),
        is_complete: false,
    }]);
    // 2nd fetch: Stump now complete (finished session recorded), active cleared.
    let completed = build_media_response_json(&[mock_media_completed_no_active(
        "media-1",
        "/srv/comics/Comics/001.cbz",
        20,
    )]);

    Mock::given(method("POST"))
        .and(req_path("/graphql"))
        .and(body_string_contains("GetLibraryMedia"))
        .respond_with(SequencedResponder {
            responses: vec![in_progress, completed],
            calls: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    mock_update_progress_response(&server).await;
    mock_mark_complete_response(&server).await;

    let mapping_db_path = dir.path().join("mappings.db");
    let lib_config = LibraryConfig {
        yac_library_id: 1,
        ydb_path: ydb_path.to_str().unwrap().to_string(),
        library_root: dir.path().to_str().unwrap().to_string(),
        stump_library_id: "lib-1".into(),
        stump_library_path: "/srv/comics".into(),
    };
    let engine = make_engine(&server, mapping_db_path.to_str().unwrap(), vec![lib_config]);

    // First sync: Stump is behind & not complete → push page 20 + markComplete.
    engine.sync_all().await.unwrap();
    // Second sync: Stump now reports the finished session → push NOTHING.
    engine.sync_all().await.unwrap();

    let requests = server.received_requests().await.unwrap();
    let completions = requests_containing(&requests, "MarkComplete");
    assert_eq!(
        completions.len(),
        1,
        "markMediaAsComplete must be sent exactly once across both syncs"
    );
    assert!(completions[0].contains("media-1"));
}
