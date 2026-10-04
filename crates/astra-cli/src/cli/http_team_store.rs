//! HTTP-backed team persistence for CLI clients.
//!
//! Team CRUD/history/snapshots belong to the server's cloud authority. The CLI
//! keeps an in-memory registry for the current process, but persisted team state
//! flows through the runtime HTTP API rather than direct MatrixOne access.

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;

use astra_services::team_persistence::{
    TeamDefinition, TeamExecutionRecord, TeamPersistenceService, TeamSnapshotRecord,
};

const TEAM_HTTP_TIMEOUT_SECS: u64 = 15;

#[derive(Debug, Deserialize)]
struct TeamListResponse {
    teams: Vec<TeamDefinition>,
}

#[derive(Debug, Deserialize)]
struct ExecutionListResponse {
    executions: Vec<ExecutionWire>,
}

#[derive(Debug, Deserialize)]
struct ExecutionWire {
    execution_id: String,
    team_id: String,
    task: String,
    status: String,
    result_json: Option<String>,
    started_at: String,
    completed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SnapshotListResponse {
    snapshots: Vec<SnapshotWire>,
}

#[derive(Debug, Deserialize)]
struct SnapshotWire {
    snapshot_id: String,
    team_name: String,
    label: String,
    git_commit: Option<String>,
    session_id: Option<String>,
    team_definition_json: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct DeleteResponse {
    deleted: bool,
}

#[derive(Debug, Serialize)]
struct UpsertTeamRequest<'a> {
    name: &'a str,
    description: &'a str,
    coordination: &'a astra_services::team_persistence::TeamCoordination,
    members: &'a Vec<astra_services::team_persistence::TeamMemberDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'a std::collections::HashMap<String, String>>,
    worktree_mode: &'a astra_services::team_persistence::WorktreeMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<&'a astra_services::team_persistence::TeamBudget>,
    max_parallel: u32,
}

#[derive(Debug, Serialize)]
struct CreateSnapshotRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

pub(crate) struct HttpTeamStore {
    api: astra_thin_client::ThinClient,
    profile: Option<String>,
}

#[derive(Debug)]
enum TeamHttpError {
    AuthenticationRequired,
    Network {
        method: &'static str,
        path: String,
        error: String,
    },
    Http {
        method: &'static str,
        path: String,
        status: reqwest::StatusCode,
        body: String,
    },
    Decode {
        method: &'static str,
        path: String,
        error: String,
    },
}

impl TeamHttpError {
    fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Http {
                status: reqwest::StatusCode::NOT_FOUND,
                ..
            }
        )
    }
}

impl fmt::Display for TeamHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthenticationRequired => write!(f, "team API requires authentication"),
            Self::Network {
                method,
                path,
                error,
            } => write!(f, "network {method} {path}: {error}"),
            Self::Http {
                method,
                path,
                status,
                body,
            } => write!(f, "team API {method} {path} -> {status}: {body}"),
            Self::Decode {
                method,
                path,
                error,
            } => write!(f, "decode {method} {path}: {error}"),
        }
    }
}

impl HttpTeamStore {
    pub(crate) fn new(api: &astra_thin_client::ThinClient, profile: Option<&str>) -> Self {
        Self {
            api: api.clone(),
            profile: profile.map(str::to_string),
        }
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: &'static str,
        path: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<T, TeamHttpError> {
        let token = crate::cli::session::session_runtime::fresh_access_token(
            &self.api,
            self.profile.as_deref(),
        )
        .await
        .ok_or(TeamHttpError::AuthenticationRequired)?;
        let response = request
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(TEAM_HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|error| TeamHttpError::Network {
                method,
                path: path.to_string(),
                error: error.to_string(),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(TeamHttpError::Http {
                method,
                path: path.to_string(),
                status,
                body: response.text().await.unwrap_or_default(),
            });
        }
        response
            .json::<T>()
            .await
            .map_err(|error| TeamHttpError::Decode {
                method,
                path: path.to_string(),
                error: error.to_string(),
            })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api.api_origin(), path)
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, TeamHttpError> {
        self.request_json(
            "GET",
            path,
            self.api.http_client().get(self.url(path)).query(query),
        )
        .await
    }

    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, TeamHttpError> {
        self.request_json(
            "POST",
            path,
            self.api.http_client().post(self.url(path)).json(body),
        )
        .await
    }

    async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, TeamHttpError> {
        self.request_json(
            "DELETE",
            path,
            self.api.http_client().delete(self.url(path)),
        )
        .await
    }

    fn team_path_segment(value: &str) -> String {
        urlencoding::encode(value).into_owned()
    }
}

#[async_trait]
impl TeamPersistenceService for HttpTeamStore {
    async fn ensure_builtins(&self, user_id: &str) -> Result<(), String> {
        // The authenticated server endpoint owns idempotent materialization.
        // Reading the collection exercises that owner-scoped boundary without
        // duplicating builtin definitions in the CLI.
        self.list_teams(user_id).await.map(|_| ())
    }

    async fn save_team(&self, team: &TeamDefinition) -> Result<TeamDefinition, String> {
        let body = UpsertTeamRequest {
            name: &team.name,
            description: &team.description,
            coordination: &team.coordination,
            members: &team.members,
            context: if team.context.is_empty() {
                None
            } else {
                Some(&team.context)
            },
            worktree_mode: &team.worktree_mode,
            budget: team.budget.as_ref(),
            max_parallel: team.max_parallel,
        };
        self.post_json("/teams", &body)
            .await
            .map_err(|e| e.to_string())
    }

    async fn load_team(
        &self,
        _user_id: &str,
        name: &str,
    ) -> Result<Option<TeamDefinition>, String> {
        let name = Self::team_path_segment(name);
        match self
            .get_json::<TeamDefinition>(&format!("/teams/{name}"), &[])
            .await
        {
            Ok(team) => Ok(Some(team)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn load_team_by_id(
        &self,
        _user_id: &str,
        team_id: &str,
    ) -> Result<Option<TeamDefinition>, String> {
        let team_id = Self::team_path_segment(team_id);
        match self
            .get_json::<TeamDefinition>(&format!("/teams/{team_id}"), &[])
            .await
        {
            Ok(team) => Ok(Some(team)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn list_teams(&self, _user_id: &str) -> Result<Vec<TeamDefinition>, String> {
        let list: TeamListResponse = self
            .get_json("/teams", &[])
            .await
            .map_err(|e| e.to_string())?;
        let mut teams = list.teams;
        teams.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(teams)
    }

    async fn delete_team(&self, _user_id: &str, name: &str) -> Result<bool, String> {
        let name = Self::team_path_segment(name);
        match self
            .delete_json::<DeleteResponse>(&format!("/teams/{name}"))
            .await
        {
            Ok(response) => Ok(response.deleted),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn list_executions(
        &self,
        team_id: &str,
        limit: u32,
    ) -> Result<Vec<TeamExecutionRecord>, String> {
        let team_id = Self::team_path_segment(team_id);
        let response: ExecutionListResponse = self
            .get_json(
                &format!("/teams/{team_id}/executions"),
                &[("limit", limit.to_string())],
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(response
            .executions
            .into_iter()
            .map(|entry| TeamExecutionRecord {
                execution_id: entry.execution_id,
                team_id: entry.team_id,
                user_id: String::new(),
                task: entry.task,
                status: entry.status,
                result_json: entry.result_json,
                started_at: entry.started_at,
                completed_at: entry.completed_at,
            })
            .collect())
    }

    async fn save_snapshot(
        &self,
        snapshot: &TeamSnapshotRecord,
    ) -> Result<TeamSnapshotRecord, String> {
        let body = CreateSnapshotRequest {
            label: (!snapshot.label.is_empty()).then_some(snapshot.label.as_str()),
            git_commit: snapshot.git_commit.as_deref(),
            session_id: snapshot.session_id.as_deref(),
        };
        let accepted: SnapshotWire = self
            .post_json(&format!("/teams/{}/snapshots", snapshot.team_name), &body)
            .await
            .map_err(|e| e.to_string())?;
        Ok(TeamSnapshotRecord {
            snapshot_id: accepted.snapshot_id,
            team_name: accepted.team_name,
            user_id: snapshot.user_id.clone(),
            label: accepted.label,
            git_commit: accepted.git_commit,
            session_id: accepted.session_id,
            team_definition_json: accepted.team_definition_json,
            created_at: accepted.created_at,
        })
    }

    async fn list_snapshots(
        &self,
        team_name: &str,
        user_id: &str,
        _limit: u32,
    ) -> Result<Vec<TeamSnapshotRecord>, String> {
        let response: SnapshotListResponse = self
            .get_json(&format!("/teams/{team_name}/snapshots"), &[])
            .await
            .map_err(|e| e.to_string())?;
        Ok(response
            .snapshots
            .into_iter()
            .map(|snapshot| TeamSnapshotRecord {
                snapshot_id: snapshot.snapshot_id,
                team_name: snapshot.team_name,
                user_id: user_id.to_string(),
                label: snapshot.label,
                git_commit: snapshot.git_commit,
                session_id: snapshot.session_id,
                team_definition_json: snapshot.team_definition_json,
                created_at: snapshot.created_at,
            })
            .collect())
    }

    async fn find_snapshot(
        &self,
        snapshot_id: &str,
        user_id: &str,
    ) -> Result<Option<TeamSnapshotRecord>, String> {
        match self
            .get_json::<SnapshotWire>(&format!("/teams/snapshots/{snapshot_id}"), &[])
            .await
        {
            Ok(snapshot) => Ok(Some(TeamSnapshotRecord {
                snapshot_id: snapshot.snapshot_id,
                team_name: snapshot.team_name,
                user_id: user_id.to_string(),
                label: snapshot.label,
                git_commit: snapshot.git_commit,
                session_id: snapshot.session_id,
                team_definition_json: snapshot.team_definition_json,
                created_at: snapshot.created_at,
            })),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn delete_snapshot(&self, snapshot_id: &str, _user_id: &str) -> Result<bool, String> {
        match self
            .delete_json::<DeleteResponse>(&format!("/teams/snapshots/{snapshot_id}"))
            .await
        {
            Ok(response) => Ok(response.deleted),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HttpTeamStore;
    use astra_credentials::{CredentialsFile, Profile};
    use astra_services::team_persistence::TeamPersistenceService;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_test_profile() {
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".into()),
                ..Default::default()
            },
        );
        crate::cli::cli_config::cli_utils::save_credentials(&creds).unwrap();
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn save_team_returns_server_identity_without_an_extra_read() {
        let _creds_guard = crate::tests::isolate_credentials();
        write_test_profile();
        let server = MockServer::start().await;
        let mut requested =
            astra_services::team_persistence::builtin_teams("user-1", "2026-10-03T00:00:00Z")
                .remove(0);
        requested.max_parallel = 2;
        requested.budget = Some(astra_services::team_persistence::TeamBudget {
            max_cost_usd: 1.0,
            max_tokens: 100,
            max_duration_secs: 30,
        });
        requested.members[0].can_delegate = true;
        requested.members[0].max_delegation_depth = 2;
        requested.members[0].mcp_servers = vec!["fixture-mcp".into()];
        let mut accepted = requested.clone();
        accepted.team_id = "server-assigned-team".into();
        Mock::given(method("POST"))
            .and(path("/teams"))
            .and(header("authorization", "Bearer test-token"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "max_parallel": 2,
                "budget": requested.budget,
                "members": requested.members,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(&accepted))
            .expect(1)
            .mount(&server)
            .await;
        let saved = HttpTeamStore::new(
            &astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
            None,
        )
        .save_team(&requested)
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(saved).unwrap(),
            serde_json::to_value(accepted).unwrap()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn load_team_returns_none_on_404() {
        let _creds_guard = crate::tests::isolate_credentials();
        write_test_profile();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/missing-team"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .expect(1)
            .mount(&server)
            .await;

        let store = HttpTeamStore::new(
            &astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
            None,
        );
        let team = store.load_team("user-1", "missing-team").await.unwrap();
        assert!(team.is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn load_team_refreshes_credentials_before_dispatch_and_fails_closed() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _env_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let expired = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjF9.sig";
        let valid = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjQxMDAwMDAwMDB9.sig";
        let team =
            astra_services::team_persistence::builtin_teams("user-1", "2026-10-03T00:00:00Z")
                .remove(0);
        let team_path = format!("/teams/{}", team.name);

        for (access_token, refresh_status, expected_token) in [
            (Some(valid), None, Some(valid)),
            (Some(expired), Some(200), Some("fresh-access")),
            (None, Some(200), Some("fresh-access")),
            (Some(expired), Some(503), None),
            (None, Some(503), None),
        ] {
            let mut credentials = CredentialsFile::default();
            credentials.profiles.insert(
                "default".into(),
                Profile {
                    access_token: Some("other-profile-token".into()),
                    ..Default::default()
                },
            );
            credentials.profiles.insert(
                "team-profile".into(),
                Profile {
                    account_id: Some("user-1".into()),
                    access_token: access_token.map(str::to_string),
                    refresh_token: Some("refresh-old".into()),
                    ..Default::default()
                },
            );
            crate::cli::cli_config::cli_utils::save_credentials(&credentials).unwrap();

            let server = MockServer::start().await;
            let refresh_count = u64::from(refresh_status.is_some());
            let team_count = u64::from(expected_token.is_some());
            Mock::given(method("POST"))
                .and(path("/auth/refresh"))
                .and(wiremock::matchers::body_json(serde_json::json!({
                    "refresh_token": "refresh-old"
                })))
                .respond_with(
                    ResponseTemplate::new(refresh_status.unwrap_or(503)).set_body_json(
                        serde_json::json!({
                            "user_id": "user-1",
                            "access_token": "fresh-access",
                            "refresh_token": "fresh-refresh"
                        }),
                    ),
                )
                .expect(refresh_count)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(&team_path))
                .and(header(
                    "authorization",
                    format!("Bearer {}", expected_token.unwrap_or("fresh-access")),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(&team))
                .expect(team_count)
                .mount(&server)
                .await;

            let result = HttpTeamStore::new(
                &astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
                Some("team-profile"),
            )
            .load_team("user-1", &team.name)
            .await;
            if expected_token.is_some() {
                assert_eq!(
                    serde_json::to_value(result.unwrap().expect("authorized team")).unwrap(),
                    serde_json::to_value(&team).unwrap()
                );
            } else {
                assert_eq!(result.unwrap_err(), "team API requires authentication");
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len() as u64, refresh_count + team_count);
            if refresh_status.is_some() {
                assert_eq!(requests[0].url.path(), "/auth/refresh");
            }
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.url.path() == team_path)
                    .count() as u64,
                team_count
            );
            let saved = crate::cli::cli_config::cli_utils::load_credentials();
            assert_eq!(
                saved.profiles["team-profile"].access_token.as_deref(),
                expected_token.or(access_token)
            );
            assert_eq!(
                saved.profiles["default"].access_token.as_deref(),
                Some("other-profile-token")
            );
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn delete_snapshot_refreshes_credentials_on_the_shared_transport() {
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let api =
            astra_thin_client::ThinClient::new(&server.uri(), Some("stale-default-token".into()))
                .unwrap();
        let store = HttpTeamStore::new(&api, None);
        for token in ["first-token", "rotated-token"] {
            let mut creds = CredentialsFile::default();
            creds.profiles.insert(
                "default".into(),
                Profile {
                    access_token: Some(token.into()),
                    ..Default::default()
                },
            );
            crate::cli::cli_config::cli_utils::save_credentials(&creds).unwrap();
            Mock::given(method("DELETE"))
                .and(path("/teams/snapshots/missing-snapshot"))
                .and(header("authorization", format!("Bearer {token}")))
                .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                !store
                    .delete_snapshot("missing-snapshot", "user-1")
                    .await
                    .unwrap()
            );
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn list_executions_uses_team_id_path_directly() {
        let _creds_guard = crate::tests::isolate_credentials();
        write_test_profile();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/team-1/executions"))
            .and(query_param("limit", "3"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "team_id": "team-1",
                "team_name": "known-name",
                "executions": [{
                    "execution_id": "exec-1",
                    "team_id": "team-1",
                    "task": "review",
                    "status": "completed",
                    "result_json": null,
                    "started_at": "2026-05-17T00:00:00Z",
                    "completed_at": "2026-05-17T00:01:00Z"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let store = HttpTeamStore::new(
            &astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
            None,
        );
        let executions = store.list_executions("team-1", 3).await.unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].team_id, "team-1");
    }
}
