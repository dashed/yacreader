use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;

use crate::types::{StumpLibrary, StumpMedia, SyncError};

pub struct StumpClient {
    client: reqwest::Client,
    base_url: String,
    #[allow(dead_code)]
    user_id: String,
}

#[derive(Serialize)]
struct GraphQLRequest<'a> {
    query: &'a str,
    variables: serde_json::Value,
}

#[derive(Deserialize)]
struct GraphQLResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Deserialize, Debug)]
struct GraphQLError {
    message: String,
}

#[derive(Deserialize)]
struct LibrariesListData {
    libraries: PaginatedLibraries,
}

#[derive(Deserialize)]
struct PaginatedLibraries {
    nodes: Vec<StumpLibrary>,
}

#[derive(Deserialize)]
struct LibraryByIdData {
    #[serde(rename = "libraryById")]
    library_by_id: Option<LibraryMediaPayload>,
}

#[derive(Deserialize)]
struct LibraryMediaPayload {
    media: Vec<StumpMedia>,
}

/// `updateMediaProgress` returns the `MediaProgress` union
/// (`ActiveReadingSession | FinishedReadingSession`). We treat the push as
/// fire-and-forget, so we only need the field to exist — its shape is captured
/// as an opaque value and never inspected.
#[derive(Debug, Deserialize)]
struct UpdateProgressData {
    #[serde(rename = "updateMediaProgress")]
    _update_media_progress: serde_json::Value,
}

/// `markMediaAsComplete` returns a *nullable* `FinishedReadingSession`.
#[derive(Debug, Deserialize)]
struct MarkCompleteData {
    #[serde(rename = "markMediaAsComplete")]
    _mark_media_as_complete: Option<serde_json::Value>,
}

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1000;

/// Fetch every media in a library with the user-scoped active session
/// (`readProgress`, nullable singular) and finished sessions (`readHistory`).
/// `Library.media` with no `take` returns ALL media — no pagination needed.
/// Completion is derived from `readHistory` presence, so we deliberately do NOT
/// select `percentageCompleted` (async-graphql serializes Decimal as a string).
const GET_LIBRARY_MEDIA_QUERY: &str = r#"
    query GetLibraryMedia($libraryId: ID!) {
        libraryById(id: $libraryId) {
            id
            media {
                id
                name
                pages
                path
                readProgress {
                    page
                    updatedAt
                }
                readHistory {
                    completedAt
                }
            }
        }
    }
"#;

/// Push a paged progress update. `MediaProgressInput` is `@oneOf {paged|epub}`;
/// `PagedProgressInput` is `{ page: Int!, elapsedSeconds: Int }` — there is no
/// `isComplete` here, so a page push never (un)completes a comic. The return is
/// the `MediaProgress` union, so the selection needs inline fragments.
const UPDATE_PROGRESS_MUTATION: &str = r#"
    mutation UpdateProgress($id: ID!, $page: Int!) {
        updateMediaProgress(id: $id, input: { paged: { page: $page } }) {
            __typename
            ... on ActiveReadingSession { id page }
            ... on FinishedReadingSession { id completedAt }
        }
    }
"#;

/// Mark a comic complete. `isComplete: true` is hard-coded — we only ever mark
/// complete, never un-complete. NOTE: Stump does NOT dedupe this server-side
/// (repeated calls append duplicate history), so callers MUST gate it on
/// `final_complete && !stump_complete` to stay idempotent.
const MARK_COMPLETE_MUTATION: &str = r#"
    mutation MarkComplete($id: ID!) {
        markMediaAsComplete(id: $id, isComplete: true) {
            id
            completedAt
        }
    }
"#;

impl StumpClient {
    pub fn new(base_url: String, api_key: String, user_id: String) -> Result<Self, SyncError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let auth_value = format!("Bearer {api_key}");
        let header_val = HeaderValue::from_str(&auth_value)
            .map_err(|e| SyncError::Config(format!("invalid API key header: {e}")))?;
        headers.insert(AUTHORIZATION, header_val);

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| SyncError::Http(format!("failed to build HTTP client: {e}")))?;

        let base_url = base_url.trim_end_matches('/').to_string();

        Ok(Self {
            client,
            base_url,
            user_id,
        })
    }

    /// List all Stump libraries (id, name, path). Used to auto-discover the
    /// YAC↔Stump library mapping when no explicit override is configured.
    pub async fn list_libraries(&self) -> Result<Vec<StumpLibrary>, SyncError> {
        let query = r#"
            query ListLibraries {
                libraries(
                    orderBy: [{ field: NAME, direction: ASC }]
                    pagination: { offset: { page: 1, pageSize: 200, zeroBased: false } }
                ) {
                    nodes {
                        id
                        name
                        path
                    }
                }
            }
        "#;

        let data: LibrariesListData = self
            .graphql_request(query, serde_json::Value::Null)
            .await?;
        Ok(data.libraries.nodes)
    }

    /// Fetch all media for a library, with each media's active session
    /// (`readProgress`) and finished sessions (`readHistory`). Both are
    /// auto-scoped to the API-key user, so no user filter is needed.
    pub async fn get_library_media(
        &self,
        library_id: &str,
    ) -> Result<Vec<StumpMedia>, SyncError> {
        let variables = serde_json::json!({
            "libraryId": library_id
        });

        let data: LibraryByIdData = self
            .graphql_request(GET_LIBRARY_MEDIA_QUERY, variables)
            .await?;
        Ok(data.library_by_id.map(|l| l.media).unwrap_or_default())
    }

    /// Push a page-progress update to Stump (fire-and-forget, but GraphQL errors
    /// are surfaced). Completion is never (un)set here — see `mark_complete`.
    pub async fn update_progress(
        &self,
        media_id: &str,
        page: i32,
    ) -> Result<(), SyncError> {
        let variables = serde_json::json!({
            "id": media_id,
            "page": page
        });

        let _: UpdateProgressData = self
            .graphql_request(UPDATE_PROGRESS_MUTATION, variables)
            .await?;
        Ok(())
    }

    /// Mark a comic complete on Stump (always `isComplete: true`). Callers MUST
    /// only invoke this when Stump is not already complete (idempotency guard) —
    /// Stump appends a duplicate finished session on every call.
    pub async fn mark_complete(&self, media_id: &str) -> Result<(), SyncError> {
        let variables = serde_json::json!({
            "id": media_id
        });

        let _: MarkCompleteData = self
            .graphql_request(MARK_COMPLETE_MUTATION, variables)
            .await?;
        Ok(())
    }

    async fn graphql_request<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T, SyncError> {
        let url = format!("{}/graphql", self.base_url);
        let body = GraphQLRequest { query, variables };

        let mut last_err = SyncError::Http("no attempts made".into());

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let backoff = Duration::from_millis(INITIAL_BACKOFF_MS * 2u64.pow(attempt - 1));
                tracing::warn!(attempt, ?backoff, "retrying GraphQL request");
                tokio::time::sleep(backoff).await;
            }

            let response = match self.client.post(&url).json(&body).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "HTTP request failed");
                    last_err = SyncError::Http(e.to_string());
                    continue;
                }
            };

            let status = response.status();
            if status.is_server_error() {
                let body_text = response.text().await.unwrap_or_default();
                tracing::warn!(attempt, %status, body = %body_text, "server error");
                last_err = SyncError::Http(format!("server error {status}: {body_text}"));
                continue;
            }

            if !status.is_success() {
                let body_text = response.text().await.unwrap_or_default();
                return Err(SyncError::Http(format!(
                    "HTTP {status}: {body_text}"
                )));
            }

            let gql_response: GraphQLResponse<T> = response
                .json()
                .await
                .map_err(|e| SyncError::Http(format!("failed to parse response: {e}")))?;

            if let Some(errors) = gql_response.errors {
                if !errors.is_empty() {
                    let messages: Vec<_> = errors.iter().map(|e| e.message.as_str()).collect();
                    return Err(SyncError::GraphQL(messages.join("; ")));
                }
            }

            return gql_response
                .data
                .ok_or_else(|| SyncError::GraphQL("response contained no data".into()));
        }

        Err(last_err)
    }

    #[cfg(test)]
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn setup_client(server: &MockServer) -> StumpClient {
        StumpClient::new(
            server.uri(),
            "test-api-key".to_string(),
            "test-user-id".to_string(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_update_progress_sends_correct_mutation() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateMediaProgress"))
            .and(body_string_contains("\"id\":\"media-123\""))
            .and(body_string_contains("\"page\":5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "updateMediaProgress": {
                        "__typename": "ActiveReadingSession",
                        "id": "media-123",
                        "page": 5
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = setup_client(&server).await;
        client.update_progress("media-123", 5).await.unwrap();
    }

    #[tokio::test]
    async fn test_mark_complete_sends_correct_mutation() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("markMediaAsComplete"))
            .and(body_string_contains("\"id\":\"media-456\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "markMediaAsComplete": {
                        "id": "media-456",
                        "completedAt": "2024-01-01T00:00:00Z"
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = setup_client(&server).await;
        client.mark_complete("media-456").await.unwrap();
    }

    #[tokio::test]
    async fn test_retry_on_http_500() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .expect(3)
            .mount(&server)
            .await;

        let client = StumpClient::new(
            server.uri(),
            "test-api-key".to_string(),
            "test-user-id".to_string(),
        )
        .unwrap();

        let result: Result<UpdateProgressData, _> = client
            .graphql_request(
                "mutation { updateMediaProgress(input: {}) { page } }",
                serde_json::json!({}),
            )
            .await;

        assert!(result.is_err());
        match result.unwrap_err() {
            SyncError::Http(msg) => assert!(msg.contains("server error"), "got: {msg}"),
            other => panic!("expected Http error, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_auth_header_is_set() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "Bearer test-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "updateMediaProgress": {
                        "page": 1
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = setup_client(&server).await;
        client.update_progress("media-1", 1).await.unwrap();
    }

    #[tokio::test]
    async fn test_graphql_error_handling() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": null,
                "errors": [
                    { "message": "media not found" },
                    { "message": "permission denied" }
                ]
            })))
            .mount(&server)
            .await;

        let client = setup_client(&server).await;
        let result = client.update_progress("bad-id", 1).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            SyncError::GraphQL(msg) => {
                assert!(msg.contains("media not found"), "got: {msg}");
                assert!(msg.contains("permission denied"), "got: {msg}");
            }
            other => panic!("expected GraphQL error, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_list_libraries_parses_nodes() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("ListLibraries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "libraries": {
                        "nodes": [
                            { "id": "lib-1", "name": "Comics", "path": "/srv/comics" },
                            { "id": "lib-2", "name": "Manga", "path": "/srv/manga/" }
                        ]
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = setup_client(&server).await;
        let libs = client.list_libraries().await.unwrap();
        assert_eq!(libs.len(), 2);
        assert_eq!(libs[0].id, "lib-1");
        assert_eq!(libs[0].name, "Comics");
        assert_eq!(libs[0].path, "/srv/comics");
        assert_eq!(libs[1].id, "lib-2");
        assert_eq!(libs[1].path, "/srv/manga/");
    }

    #[tokio::test]
    async fn test_get_library_media_parses_library_by_id() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("GetLibraryMedia"))
            .and(body_string_contains("\"libraryId\":\"lib-1\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "libraryById": {
                        "id": "lib-1",
                        "media": [
                            {
                                "id": "media-1",
                                "name": "001.cbz",
                                "pages": 20,
                                "path": "/srv/comics/001.cbz",
                                "readProgress": {
                                    "page": 5,
                                    "updatedAt": "2026-05-20T14:32:10Z"
                                },
                                "readHistory": []
                            }
                        ]
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = setup_client(&server).await;
        let media = client.get_library_media("lib-1").await.unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].id, "media-1");
        assert_eq!(media[0].current_page(), 5);
        assert!(!media[0].is_complete());
    }

    /// Tripwire: the query/mutation strings must match the REAL Stump schema and
    /// must never regress to the fictional field/type names that only ever
    /// satisfied the old wiremock mocks. The forbidden needles are assembled at
    /// runtime via `concat!` so this guard itself never appears in a schema grep
    /// over the source.
    #[test]
    fn test_queries_match_real_schema() {
        // Required real-schema tokens.
        assert!(GET_LIBRARY_MEDIA_QUERY.contains("libraryById"));
        assert!(GET_LIBRARY_MEDIA_QUERY.contains("readProgress"));
        assert!(GET_LIBRARY_MEDIA_QUERY.contains("readHistory"));
        assert!(UPDATE_PROGRESS_MUTATION.contains("updateMediaProgress"));
        assert!(MARK_COMPLETE_MUTATION.contains("markMediaAsComplete"));

        // Fictional tokens that must NEVER reappear in any query/mutation.
        let all = format!(
            "{GET_LIBRARY_MEDIA_QUERY}{UPDATE_PROGRESS_MUTATION}{MARK_COMPLETE_MUTATION}"
        );
        for forbidden in [
            concat!("readPro", "gresses"),
            concat!("putMedia", "Completion"),
            concat!("Update", "MediaProgress"),
        ] {
            assert!(
                !all.contains(forbidden),
                "fictional token `{forbidden}` reappeared in a query/mutation string"
            );
        }
    }

    #[tokio::test]
    async fn test_get_library_media_null_library_is_empty() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("GetLibraryMedia"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "libraryById": null }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = setup_client(&server).await;
        let media = client.get_library_media("missing").await.unwrap();
        assert!(media.is_empty());
    }
}
