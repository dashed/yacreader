use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;

use crate::types::{StumpMedia, SyncError};

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
struct LibrariesData {
    libraries: LibrariesPayload,
}

#[derive(Deserialize)]
struct LibrariesPayload {
    nodes: Vec<LibraryNode>,
}

#[derive(Deserialize)]
struct LibraryNode {
    series: SeriesPayload,
}

#[derive(Deserialize)]
struct SeriesPayload {
    nodes: Vec<SeriesNode>,
}

#[derive(Deserialize)]
struct SeriesNode {
    media: Vec<StumpMedia>,
}

#[derive(Debug, Deserialize)]
struct UpdateProgressData {
    #[serde(rename = "updateMediaProgress")]
    _update_media_progress: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct MarkCompleteData {
    #[serde(rename = "putMediaCompletion")]
    _put_media_completion: serde_json::Value,
}

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1000;

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

    pub async fn get_library_media(
        &self,
        library_id: &str,
    ) -> Result<Vec<StumpMedia>, SyncError> {
        let query = r#"
            query GetLibraryMedia($libraryId: ID!) {
                libraries(filters: { id: $libraryId }) {
                    nodes {
                        series {
                            nodes {
                                media {
                                    id
                                    name
                                    pages
                                    path
                                    readProgresses {
                                        page
                                        percentageCompleted
                                        isCompleted
                                        epubCfi
                                        completedAt
                                        updatedAt
                                    }
                                }
                            }
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "libraryId": library_id
        });

        let data: LibrariesData = self.graphql_request(query, variables).await?;

        let mut all_media = Vec::new();
        for library in data.libraries.nodes {
            for series in library.series.nodes {
                all_media.extend(series.media);
            }
        }
        Ok(all_media)
    }

    pub async fn update_progress(
        &self,
        media_id: &str,
        page: i32,
    ) -> Result<(), SyncError> {
        let query = r#"
            mutation UpdateProgress($input: UpdateMediaProgress!) {
                updateMediaProgress(input: $input) {
                    page
                }
            }
        "#;

        let variables = serde_json::json!({
            "input": {
                "mediaId": media_id,
                "page": page,
                "isCompleted": false
            }
        });

        let _: UpdateProgressData = self.graphql_request(query, variables).await?;
        Ok(())
    }

    pub async fn mark_complete(
        &self,
        media_id: &str,
        is_complete: bool,
    ) -> Result<(), SyncError> {
        let query = r#"
            mutation MarkComplete($mediaId: ID!, $isCompleted: Boolean!) {
                putMediaCompletion(id: $mediaId, isCompleted: $isCompleted) {
                    isCompleted
                }
            }
        "#;

        let variables = serde_json::json!({
            "mediaId": media_id,
            "isCompleted": is_complete
        });

        let _: MarkCompleteData = self.graphql_request(query, variables).await?;
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
            .and(body_string_contains("\"mediaId\":\"media-123\""))
            .and(body_string_contains("\"page\":5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "updateMediaProgress": {
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
            .and(body_string_contains("putMediaCompletion"))
            .and(body_string_contains("\"mediaId\":\"media-456\""))
            .and(body_string_contains("\"isCompleted\":true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "putMediaCompletion": {
                        "isCompleted": true
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = setup_client(&server).await;
        client.mark_complete("media-456", true).await.unwrap();
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
}
