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
    cloud_base: String,
    profile: Option<String>,
}

#[derive(Debug)]
enum TeamHttpError {
    AuthenticationRequired,
    ClientInit(String),
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
            Self::ClientInit(error) => write!(f, "http client init: {error}"),
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
    pub(crate) fn new(cloud_base: impl Into<String>, profile: Option<&str>) -> Self {
        Self {
            cloud_base: cloud_base.into(),
            profile: profile.map(str::to_string),
        }
    }

    fn authed_client(&self) -> Result<(reqwest::Client, String), TeamHttpError> {
        let token = crate::cli::session_runtime::current_access_token(self.profile.as_deref())
            .ok_or(TeamHttpError::AuthenticationRequired)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(TEAM_HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| TeamHttpError::ClientInit(e.to_string()))?;
        Ok((client, token))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cloud_base.trim_end_matches('/'), path)
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, TeamHttpError> {
        let (client, token) = self.authed_client()?;
        let mut req = client.get(self.url(path)).bearer_auth(token);
        if !query.is_empty() {
            req = req.query(query);
        }
        let resp = req.send().await.map_err(|e| TeamHttpError::Network {
            method: "GET",
            path: path.to_string(),
            error: e.to_string(),
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TeamHttpError::Http {
                method: "GET",
                path: path.to_string(),
                status,
                body,
            });
        }
        resp.json::<T>().await.map_err(|e| TeamHttpError::Decode {
            method: "GET",
            path: path.to_string(),
            error: e.to_string(),
        })
    }

    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, TeamHttpError> {
        let (client, token) = self.authed_client()?;
        let resp = client
            .post(self.url(path))
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|e| TeamHttpError::Network {
                method: "POST",
                path: path.to_string(),
                error: e.to_string(),
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TeamHttpError::Http {
                method: "POST",
                path: path.to_string(),
                status,
                body,
            });
        }
        resp.json::<T>().await.map_err(|e| TeamHttpError::Decode {
            method: "POST",
            path: path.to_string(),
            error: e.to_string(),
        })
    }

    async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, TeamHttpError> {
        let (client, token) = self.authed_client()?;
        let resp = client
            .delete(self.url(path))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| TeamHttpError::Network {
                method: "DELETE",
                path: path.to_string(),
                error: e.to_string(),
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TeamHttpError::Http {
                method: "DELETE",
                path: path.to_string(),
                status,
                body,
            });
        }
        resp.json::<T>().await.map_err(|e| TeamHttpError::Decode {
            method: "DELETE",
            path: path.to_string(),
            error: e.to_string(),
        })
    }

    fn team_path_segment(value: &str) -> String {
        urlencoding::encode(value).into_owned()
    }
}

#[async_trait]
impl TeamPersistenceService for HttpTeamStore {
    async fn save_team(&self, team: &TeamDefinition) -> Result<(), String> {
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
        let _: TeamDefinition = self
            .post_json("/teams", &body)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
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

    async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamDefinition>, String> {
        let list: TeamListResponse = self
            .get_json("/teams", &[])
            .await
            .map_err(|e| e.to_string())?;
        let mut teams = list.teams;
        for team in &mut teams {
            team.user_id = user_id.to_string();
        }
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

    async fn save_snapshot(&self, snapshot: &TeamSnapshotRecord) -> Result<(), String> {
        let body = CreateSnapshotRequest {
            label: (!snapshot.label.is_empty()).then_some(snapshot.label.as_str()),
            git_commit: snapshot.git_commit.as_deref(),
            session_id: snapshot.session_id.as_deref(),
        };
        let _: SnapshotWire = self
            .post_json(&format!("/teams/{}/snapshots", snapshot.team_name), &body)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
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
    use super::*;
    use astra_credentials::{CredentialsFile, Profile};
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
        crate::cli::cli_utils::save_credentials(&creds).unwrap();
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

        let store = HttpTeamStore::new(server.uri(), None);
        let team = store.load_team("user-1", "missing-team").await.unwrap();
        assert!(team.is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn delete_snapshot_returns_false_on_404() {
        let _creds_guard = crate::tests::isolate_credentials();
        write_test_profile();

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/teams/snapshots/missing-snapshot"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .expect(1)
            .mount(&server)
            .await;

        let store = HttpTeamStore::new(server.uri(), None);
        let deleted = store
            .delete_snapshot("missing-snapshot", "user-1")
            .await
            .unwrap();
        assert!(!deleted);
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

        let store = HttpTeamStore::new(server.uri(), None);
        let executions = store.list_executions("team-1", 3).await.unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].team_id, "team-1");
    }
}
