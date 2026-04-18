#![allow(dead_code)]
//! GitHub API tool implementations.
//!
//! Extracted from edge_tools as a standalone `GitHubClient` struct.
//! Both CLI and server executors can instantiate this with their own
//! reqwest::Client and token.

use chrono::{DateTime, Utc};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use std::sync::Mutex;
use url::Url;

fn enforce_github_api_url(url: &str) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|e| format!("Error: invalid GitHub URL: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("Error: GitHub API requests must use HTTPS".to_string());
    }
    match parsed.host_str() {
        Some("api.github.com") => Ok(()),
        Some(h) => Err(format!(
            "Error: GitHub requests are restricted to https://api.github.com (got host {h:?})"
        )),
        None => Err("Error: GitHub URL must include host api.github.com".to_string()),
    }
}

/// GitHub API client for tool operations.
///
/// Wraps a reqwest::Client with optional auth token and a list of
/// preferred repositories (learned from user interactions).
/// Supports token fallback: if the primary token gets a 401/403,
/// automatically retries with the next candidate.
pub struct GitHubClient {
    client: Client,
    tokens: Mutex<Vec<String>>,
    preferred_repos: Mutex<Vec<String>>,
}

impl GitHubClient {
    pub fn new(client: Client, token: Option<String>, preferred_repos: Vec<String>) -> Self {
        let tokens = token.into_iter().collect();
        Self {
            client,
            tokens: Mutex::new(tokens),
            preferred_repos: Mutex::new(preferred_repos),
        }
    }

    /// Create with multiple candidate tokens (first is tried first, fallback on auth failure).
    pub fn from_tokens(client: Client, tokens: Vec<String>, preferred_repos: Vec<String>) -> Self {
        Self {
            client,
            tokens: Mutex::new(tokens),
            preferred_repos: Mutex::new(preferred_repos),
        }
    }

    fn current_token(&self) -> Option<String> {
        self.tokens.lock().ok().and_then(|t| t.first().cloned())
    }

    /// Drop the current (failed) token. Returns `true` if a fallback token is now active.
    fn rotate_token(&self) -> bool {
        if let Ok(mut tokens) = self.tokens.lock() {
            if !tokens.is_empty() {
                tokens.remove(0);
            }
            !tokens.is_empty()
        } else {
            false
        }
    }

    /// Record a repo as preferred for future bare-name resolution.
    pub fn add_preferred_repo(&self, owner_repo: &str) {
        let normalized = owner_repo.to_lowercase();
        match self.preferred_repos.lock() {
            Ok(mut repos) => {
                if !repos.contains(&normalized) {
                    repos.push(normalized);
                }
            }
            Err(p) => {
                let mut repos = p.into_inner();
                if !repos.contains(&normalized) {
                    repos.push(normalized);
                }
            }
        }
    }

    /// Get the current list of preferred repos.
    fn get_preferred_repos(&self) -> Vec<String> {
        match self.preferred_repos.lock() {
            Ok(repos) => repos.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

const GITHUB_MISSING_REPO_ERROR: &str = "Error: missing 'repo' — infer repo from current user text or recent conversation; bare names like 'memoria' are allowed";

/// Collect GitHub token candidates: `GITHUB_TOKEN` env var, then `gh auth token` CLI.
/// Returns all valid tokens found (may be 0, 1, or 2).
pub fn resolve_github_tokens() -> Vec<String> {
    let mut tokens = Vec::new();
    if let Ok(val) = std::env::var("GITHUB_TOKEN") {
        let val = val.trim().to_string();
        if !val.is_empty() {
            tokens.push(val);
        }
    }
    if let Some(t) = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        })
        .filter(|t| !tokens.contains(t))
    {
        tokens.push(t);
    }
    tokens
}

/// Convenience: resolve the best single token (first candidate).
pub fn resolve_github_token() -> Option<String> {
    resolve_github_tokens().into_iter().next()
}

impl GitHubClient {
    async fn github_request(
        &self,
        method: Method,
        url: &str,
        payload: Option<&Value>,
    ) -> Result<Value, String> {
        enforce_github_api_url(url)?;
        let result = self.github_request_once(method.clone(), url, payload).await;
        // On 401/403/404 with a token, try rotating to the next candidate.
        // GitHub returns 404 (not 403) for private repos with bad auth to prevent info leakage.
        // Round-robin rotation is non-destructive, so retrying on real 404 is harmless.
        if let Err(ref e) = result
            && (e.contains("HTTP 401") || e.contains("HTTP 403") || e.contains("HTTP 404"))
            && self.rotate_token()
        {
            return self.github_request_once(method, url, payload).await;
        }
        result
    }

    async fn github_request_once(
        &self,
        method: Method,
        url: &str,
        payload: Option<&Value>,
    ) -> Result<Value, String> {
        let mut request = self
            .client
            .request(method, url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.current_token() {
            request = request.bearer_auth(token);
        }
        if let Some(payload) = payload {
            request = request.json(payload);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("Error: GitHub request failed: {e}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("Error: failed reading GitHub response: {e}"))?;

        if body.trim().is_empty() {
            return Err(format!("Error: empty response from GitHub (HTTP {status})"));
        }

        let value: Value = serde_json::from_str(&body)
            .map_err(|e| format!("Error: invalid GitHub response: {e}"))?;

        if !status.is_success() {
            return Err(github_api_error_message(
                status,
                &value,
                self.current_token().is_some(),
            ));
        }

        Ok(value)
    }

    async fn github_resolve_repo(&self, repo: &str) -> Result<GithubRepoResolution, String> {
        if repo.contains('/') {
            // Learn from explicit owner/repo for future bare-name lookups
            self.add_preferred_repo(repo);
            return Ok(GithubRepoResolution {
                resolved_repo: repo.to_string(),
                resolved_by_search: false,
            });
        }

        // Check preferred repos first — if any preferred repo's name matches,
        // use it directly without hitting the GitHub search API.
        let preferred = self.get_preferred_repos();
        let repo_lower = repo.to_lowercase();
        for pref in &preferred {
            if let Some(name) = pref.rsplit('/').next()
                && name == repo_lower
            {
                return Ok(GithubRepoResolution {
                    resolved_repo: pref.clone(),
                    resolved_by_search: false,
                });
            }
        }

        let response = self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/search/repositories?q={repo}&sort=stars&per_page=5"
                ),
                None,
            )
            .await?;
        let Some(items) = response.get("items").and_then(Value::as_array) else {
            return Err(format!("Could not resolve repo '{repo}'"));
        };

        let resolved_repo = github_pick_resolved_repo(repo, items, &preferred)?;
        // Learn from successful resolution
        self.add_preferred_repo(&resolved_repo);
        Ok(GithubRepoResolution {
            resolved_repo,
            resolved_by_search: true,
        })
    }

    pub async fn github_list_prs(&self, args: &Value) -> String {
        let repo = match args.get("repo").and_then(Value::as_str) {
            Some(r) => r.to_string(),
            None => {
                return github_error_response(
                    "github_list_prs",
                    "pull_requests",
                    Some(GithubDetail::Brief),
                    None,
                    None,
                    GITHUB_MISSING_REPO_ERROR,
                );
            }
        };
        let detail = match GithubDetail::parse(args.get("detail").and_then(Value::as_str)) {
            Ok(detail) => detail,
            Err(error) => {
                return github_error_response(
                    "github_list_prs",
                    "pull_requests",
                    Some(GithubDetail::Brief),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };
        let state = args.get("state").and_then(Value::as_str).unwrap_or("open");
        let limit = github_requested_limit(args.get("limit").and_then(Value::as_u64), detail, 10);

        let resolution = match self.github_resolve_repo(&repo).await {
            Ok(resolution) => resolution,
            Err(error) => {
                return github_error_response(
                    "github_list_prs",
                    "pull_requests",
                    Some(detail),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        let response = match self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/repos/{}/pulls?state={state}&per_page={limit}",
                    resolution.resolved_repo
                ),
                None,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return github_error_response(
                    "github_list_prs",
                    "pull_requests",
                    Some(detail),
                    Some(&repo),
                    Some(&resolution),
                    error,
                );
            }
        };

        let Some(prs) = response.as_array() else {
            return github_error_response(
                "github_list_prs",
                "pull_requests",
                Some(detail),
                Some(&repo),
                Some(&resolution),
                "Error: unexpected GitHub response",
            );
        };

        let items = prs
            .iter()
            .map(|pr| github_pr_list_item(pr, detail))
            .collect::<Vec<_>>();

        github_response_string(json!({
            "ok": true,
            "tool": "github_list_prs",
            "detail": detail.as_str(),
            "requested_repo": repo,
            "resolved_repo": resolution_output_repo(&resolution),
            "resolved_by_search": resolution.resolved_by_search,
            "state": state,
            "count": items.len(),
            "pull_requests": items,
            "error": Value::Null,
        }))
    }

    pub async fn github_get_pr(&self, args: &Value) -> String {
        let repo = match args.get("repo").and_then(Value::as_str) {
            Some(r) => r.to_string(),
            None => {
                return github_error_response(
                    "github_get_pr",
                    "pull_request",
                    Some(GithubDetail::Brief),
                    None,
                    None,
                    GITHUB_MISSING_REPO_ERROR,
                );
            }
        };
        let pr_number = match args.get("pr_number").and_then(Value::as_u64) {
            Some(n) => n,
            None => {
                return github_error_response(
                    "github_get_pr",
                    "pull_request",
                    Some(GithubDetail::Brief),
                    Some(&repo),
                    None,
                    "Error: missing 'pr_number'",
                );
            }
        };
        let detail = match GithubDetail::parse(args.get("detail").and_then(Value::as_str)) {
            Ok(detail) => detail,
            Err(error) => {
                return github_error_response(
                    "github_get_pr",
                    "pull_request",
                    Some(GithubDetail::Brief),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        let resolution = match self.github_resolve_repo(&repo).await {
            Ok(resolution) => resolution,
            Err(error) => {
                return github_error_response(
                    "github_get_pr",
                    "pull_request",
                    Some(detail),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        let response = match self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/repos/{}/pulls/{pr_number}",
                    resolution.resolved_repo
                ),
                None,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return github_error_response(
                    "github_get_pr",
                    "pull_request",
                    Some(detail),
                    Some(&repo),
                    Some(&resolution),
                    error,
                );
            }
        };

        if !response.is_object() {
            return github_error_response(
                "github_get_pr",
                "pull_request",
                Some(detail),
                Some(&repo),
                Some(&resolution),
                "Error: unexpected GitHub response",
            );
        }

        let ci_conclusion =
            match json_value_at_path(&response, &["head", "sha"]).and_then(Value::as_str) {
                Some(sha) => match self
                    .github_commit_ci_conclusion(&resolution.resolved_repo, sha)
                    .await
                {
                    Ok(conclusion) => conclusion,
                    Err(error) => {
                        return github_error_response(
                            "github_get_pr",
                            "pull_request",
                            Some(detail),
                            Some(&repo),
                            Some(&resolution),
                            error,
                        );
                    }
                },
                None => "unknown".to_string(),
            };

        let key_changed_files = if detail.includes_detailed() {
            match self
                .github_pr_files(&resolution.resolved_repo, pr_number, detail)
                .await
            {
                Ok(files) => files,
                Err(error) => {
                    return github_error_response(
                        "github_get_pr",
                        "pull_request",
                        Some(detail),
                        Some(&repo),
                        Some(&resolution),
                        error,
                    );
                }
            }
        } else {
            Vec::new()
        };

        let review_comments = if detail == GithubDetail::Full {
            match self
                .github_pr_review_comments(&resolution.resolved_repo, pr_number, detail)
                .await
            {
                Ok(comments) => comments,
                Err(error) => {
                    return github_error_response(
                        "github_get_pr",
                        "pull_request",
                        Some(detail),
                        Some(&repo),
                        Some(&resolution),
                        error,
                    );
                }
            }
        } else {
            Vec::new()
        };

        let body = json_value_at_path(&response, &["body"])
            .and_then(Value::as_str)
            .unwrap_or("");

        let item = json!({
            "number": json_u64(&response, &["number"]),
            "title": github_title_value(json_value_at_path(&response, &["title"]).and_then(Value::as_str), detail),
            "author": json_value_at_path(&response, &["user", "login"]).and_then(Value::as_str).unwrap_or("?"),
            "state": github_pr_state(&response),
            "created_at": github_timestamp(json_value_at_path(&response, &["created_at"]).and_then(Value::as_str)),
            "ci_conclusion": ci_conclusion,
            "body_summary": if detail.includes_normal() {
                github_excerpt(body, Some(detail.body_limit()))
            } else {
                String::new()
            },
            "labels": if detail.includes_normal() { github_label_names(&response) } else { Vec::<String>::new() },
            "reviewers": if detail.includes_normal() { github_reviewer_names(&response) } else { Vec::<String>::new() },
            "changed_files": if detail.includes_normal() { json_u64(&response, &["changed_files"]) } else { None::<u64> },
            "additions": if detail.includes_detailed() { json_u64(&response, &["additions"]) } else { None::<u64> },
            "deletions": if detail.includes_detailed() { json_u64(&response, &["deletions"]) } else { None::<u64> },
            "key_changed_files": if detail.includes_detailed() { key_changed_files } else { Vec::<Value>::new() },
            "review_comments_count": if detail.includes_detailed() { json_u64(&response, &["review_comments"]) } else { None::<u64> },
            "merge_status": if detail.includes_detailed() {
                json_value_at_path(&response, &["mergeable_state"]).and_then(Value::as_str).map(ToString::to_string)
            } else {
                None::<String>
            },
            "conflicts": if detail.includes_detailed() {
                json_value_at_path(&response, &["mergeable_state"]).and_then(Value::as_str).map(|state| state == "dirty")
            } else {
                None::<bool>
            },
            "body": if detail == GithubDetail::Full {
                github_excerpt(body, Some(detail.body_limit()))
            } else {
                String::new()
            },
            "review_comments": if detail == GithubDetail::Full { review_comments } else { Vec::<Value>::new() },
            "url": json_value_at_path(&response, &["html_url"]).and_then(Value::as_str).unwrap_or(""),
        });

        github_response_string(json!({
            "ok": true,
            "tool": "github_get_pr",
            "detail": detail.as_str(),
            "requested_repo": repo,
            "resolved_repo": resolution_output_repo(&resolution),
            "resolved_by_search": resolution.resolved_by_search,
            "count": 1,
            "pull_request": item,
            "error": Value::Null,
        }))
    }

    pub async fn github_ci_status(&self, args: &Value) -> String {
        let repo = match args.get("repo").and_then(Value::as_str) {
            Some(r) => r.to_string(),
            None => {
                return github_error_response(
                    "github_ci_status",
                    "workflow_runs",
                    Some(GithubDetail::Brief),
                    None,
                    None,
                    GITHUB_MISSING_REPO_ERROR,
                );
            }
        };
        let detail = match GithubDetail::parse(args.get("detail").and_then(Value::as_str)) {
            Ok(detail) => detail,
            Err(error) => {
                return github_error_response(
                    "github_ci_status",
                    "workflow_runs",
                    Some(GithubDetail::Brief),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };
        let limit = github_requested_limit(args.get("limit").and_then(Value::as_u64), detail, 1);

        let resolution = match self.github_resolve_repo(&repo).await {
            Ok(resolution) => resolution,
            Err(error) => {
                return github_error_response(
                    "github_ci_status",
                    "workflow_runs",
                    Some(detail),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        let response = match self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/repos/{}/actions/runs?per_page={limit}",
                    resolution.resolved_repo
                ),
                None,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return github_error_response(
                    "github_ci_status",
                    "workflow_runs",
                    Some(detail),
                    Some(&repo),
                    Some(&resolution),
                    error,
                );
            }
        };

        let Some(runs) = response.get("workflow_runs").and_then(Value::as_array) else {
            return github_error_response(
                "github_ci_status",
                "workflow_runs",
                Some(detail),
                Some(&repo),
                Some(&resolution),
                "Error: unexpected GitHub response",
            );
        };

        let mut normalized_runs = Vec::with_capacity(runs.len());
        for run in runs {
            let run_id = json_u64(run, &["id"]);
            let (failed_jobs, failed_steps, all_jobs) = if detail.includes_detailed() {
                match run_id {
                    Some(id) => match self
                        .github_workflow_run_jobs(&resolution.resolved_repo, id, detail)
                        .await
                    {
                        Ok(job_details) => job_details,
                        Err(error) => {
                            return github_error_response(
                                "github_ci_status",
                                "workflow_runs",
                                Some(detail),
                                Some(&repo),
                                Some(&resolution),
                                error,
                            );
                        }
                    },
                    None => (Vec::new(), Vec::new(), Vec::new()),
                }
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };

            let pr_number = json_value_at_path(run, &["pull_requests"])
                .and_then(Value::as_array)
                .and_then(|prs| prs.first())
                .and_then(|pr| pr.get("number"))
                .and_then(Value::as_u64);
            let display_title = json_value_at_path(run, &["display_title"]).and_then(Value::as_str);

            normalized_runs.push(json!({
                "id": run_id,
                "name": github_title_value(json_value_at_path(run, &["name"]).and_then(Value::as_str), detail),
                "conclusion": github_normalize_conclusion(
                    json_value_at_path(run, &["conclusion"]).and_then(Value::as_str),
                    json_value_at_path(run, &["status"]).and_then(Value::as_str),
                ),
                "branch": json_value_at_path(run, &["head_branch"]).and_then(Value::as_str).unwrap_or("?"),
                "triggered_at": github_timestamp(json_value_at_path(run, &["created_at"]).and_then(Value::as_str)),
                "duration_seconds": if detail.includes_normal() {
                    github_duration_seconds(
                        json_value_at_path(run, &["run_started_at"]).and_then(Value::as_str)
                            .or_else(|| json_value_at_path(run, &["created_at"]).and_then(Value::as_str)),
                        json_value_at_path(run, &["updated_at"]).and_then(Value::as_str),
                    )
                } else {
                    None::<i64>
                },
                "pr_number": if detail.includes_normal() { pr_number } else { None::<u64> },
                "pr_title": if detail.includes_normal() && pr_number.is_some() {
                    display_title.map(|value| github_excerpt(value, Some(detail.title_limit())))
                } else {
                    None::<String>
                },
                "commit_message": if detail.includes_normal() {
                    github_excerpt(display_title.unwrap_or(""), Some(80))
                } else {
                    String::new()
                },
                "failed_jobs": if detail.includes_detailed() { failed_jobs } else { Vec::<String>::new() },
                "failed_steps": if detail.includes_detailed() { failed_steps } else { Vec::<Value>::new() },
                "all_jobs": if detail == GithubDetail::Full { all_jobs } else { Vec::<Value>::new() },
                "url": json_value_at_path(run, &["html_url"]).and_then(Value::as_str).unwrap_or(""),
            }));
        }

        github_response_string(json!({
            "ok": true,
            "tool": "github_ci_status",
            "detail": detail.as_str(),
            "requested_repo": repo,
            "resolved_repo": resolution_output_repo(&resolution),
            "resolved_by_search": resolution.resolved_by_search,
            "count": normalized_runs.len(),
            "workflow_runs": normalized_runs,
            "error": Value::Null,
        }))
    }

    pub async fn github_list_issues(&self, args: &Value) -> String {
        let repo = match args.get("repo").and_then(Value::as_str) {
            Some(r) => r.to_string(),
            None => {
                return github_error_response(
                    "github_list_issues",
                    "issues",
                    Some(GithubDetail::Brief),
                    None,
                    None,
                    GITHUB_MISSING_REPO_ERROR,
                );
            }
        };
        let detail = match GithubDetail::parse(args.get("detail").and_then(Value::as_str)) {
            Ok(detail) => detail,
            Err(error) => {
                return github_error_response(
                    "github_list_issues",
                    "issues",
                    Some(GithubDetail::Brief),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };
        let state = args.get("state").and_then(Value::as_str).unwrap_or("open");
        let limit = github_requested_limit(args.get("limit").and_then(Value::as_u64), detail, 10);
        let labels = args.get("labels").and_then(Value::as_str).unwrap_or("");

        let resolution = match self.github_resolve_repo(&repo).await {
            Ok(resolution) => resolution,
            Err(error) => {
                return github_error_response(
                    "github_list_issues",
                    "issues",
                    Some(detail),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        let labels_param = if labels.is_empty() {
            String::new()
        } else {
            format!("&labels={labels}")
        };
        let response = match self
            .github_request(
            Method::GET,
            &format!(
                "https://api.github.com/repos/{}/issues?state={state}&per_page={limit}{labels_param}",
                resolution.resolved_repo
            ),
            None,
        )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return github_error_response(
                    "github_list_issues",
                    "issues",
                    Some(detail),
                    Some(&repo),
                    Some(&resolution),
                    error,
                );
            }
        };

        let Some(issues) = response.as_array() else {
            return github_error_response(
                "github_list_issues",
                "issues",
                Some(detail),
                Some(&repo),
                Some(&resolution),
                "Error: unexpected GitHub response",
            );
        };

        let items = issues
            .iter()
            .filter(|issue| issue.get("pull_request").is_none())
            .map(|issue| github_issue_list_item(issue, detail))
            .collect::<Vec<_>>();

        github_response_string(json!({
            "ok": true,
            "tool": "github_list_issues",
            "detail": detail.as_str(),
            "requested_repo": repo,
            "resolved_repo": resolution_output_repo(&resolution),
            "resolved_by_search": resolution.resolved_by_search,
            "state": state,
            "labels": labels,
            "count": items.len(),
            "issues": items,
            "error": Value::Null,
        }))
    }

    pub async fn github_get_issue(&self, args: &Value) -> String {
        let repo = match args.get("repo").and_then(Value::as_str) {
            Some(r) => r.to_string(),
            None => {
                return github_error_response(
                    "github_get_issue",
                    "issue",
                    Some(GithubDetail::Brief),
                    None,
                    None,
                    GITHUB_MISSING_REPO_ERROR,
                );
            }
        };
        let issue_number = match args.get("issue_number").and_then(Value::as_u64) {
            Some(n) => n,
            None => {
                return github_error_response(
                    "github_get_issue",
                    "issue",
                    Some(GithubDetail::Brief),
                    Some(&repo),
                    None,
                    "Error: missing 'issue_number'",
                );
            }
        };
        let detail = match GithubDetail::parse(args.get("detail").and_then(Value::as_str)) {
            Ok(detail) => detail,
            Err(error) => {
                return github_error_response(
                    "github_get_issue",
                    "issue",
                    Some(GithubDetail::Brief),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        let resolution = match self.github_resolve_repo(&repo).await {
            Ok(resolution) => resolution,
            Err(error) => {
                return github_error_response(
                    "github_get_issue",
                    "issue",
                    Some(detail),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        let response = match self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/repos/{}/issues/{issue_number}",
                    resolution.resolved_repo
                ),
                None,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return github_error_response(
                    "github_get_issue",
                    "issue",
                    Some(detail),
                    Some(&repo),
                    Some(&resolution),
                    error,
                );
            }
        };

        if !response.is_object() {
            return github_error_response(
                "github_get_issue",
                "issue",
                Some(detail),
                Some(&repo),
                Some(&resolution),
                "Error: unexpected GitHub response",
            );
        }

        if response.get("pull_request").is_some() {
            return github_error_response(
                "github_get_issue",
                "issue",
                Some(detail),
                Some(&repo),
                Some(&resolution),
                format!("Error: #{issue_number} is a pull request, not an issue"),
            );
        }

        let comments = if detail.includes_detailed() {
            match self
                .github_issue_comments(&resolution.resolved_repo, issue_number, detail)
                .await
            {
                Ok(comments) => comments,
                Err(error) => {
                    return github_error_response(
                        "github_get_issue",
                        "issue",
                        Some(detail),
                        Some(&repo),
                        Some(&resolution),
                        error,
                    );
                }
            }
        } else {
            Vec::new()
        };

        let body = json_value_at_path(&response, &["body"])
            .and_then(Value::as_str)
            .unwrap_or("");
        let item = json!({
            "number": json_u64(&response, &["number"]),
            "title": github_title_value(json_value_at_path(&response, &["title"]).and_then(Value::as_str), detail),
            "state": json_value_at_path(&response, &["state"]).and_then(Value::as_str).unwrap_or("?"),
            "labels": github_label_names(&response),
            "created_at": github_timestamp(json_value_at_path(&response, &["created_at"]).and_then(Value::as_str)),
            "author": json_value_at_path(&response, &["user", "login"]).and_then(Value::as_str).unwrap_or("?"),
            "body": if detail.includes_normal() { github_excerpt(body, Some(detail.body_limit())) } else { String::new() },
            "assignee": if detail.includes_normal() { github_primary_assignee(&response) } else { None::<String> },
            "milestone": if detail.includes_normal() { github_milestone_title(&response) } else { None::<String> },
            "comment_count": if detail.includes_normal() { json_u64(&response, &["comments"]) } else { None::<u64> },
            "comments": if detail.includes_detailed() { comments } else { Vec::<Value>::new() },
            "linked_prs": Vec::<Value>::new(),
            "url": json_value_at_path(&response, &["html_url"]).and_then(Value::as_str).unwrap_or(""),
        });

        github_response_string(json!({
            "ok": true,
            "tool": "github_get_issue",
            "detail": detail.as_str(),
            "requested_repo": repo,
            "resolved_repo": resolution_output_repo(&resolution),
            "resolved_by_search": resolution.resolved_by_search,
            "count": 1,
            "issue": item,
            "error": Value::Null,
        }))
    }

    pub async fn github_repo_stats(&self, args: &Value) -> String {
        let repo = match args.get("repo").and_then(Value::as_str) {
            Some(r) => r.to_string(),
            None => {
                return github_error_response(
                    "github_repo_stats",
                    "repository",
                    Some(GithubDetail::Brief),
                    None,
                    None,
                    GITHUB_MISSING_REPO_ERROR,
                );
            }
        };
        let detail = match GithubDetail::parse(args.get("detail").and_then(Value::as_str)) {
            Ok(detail) => detail,
            Err(error) => {
                return github_error_response(
                    "github_repo_stats",
                    "repository",
                    Some(GithubDetail::Brief),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        let resolution = match self.github_resolve_repo(&repo).await {
            Ok(resolution) => resolution,
            Err(error) => {
                return github_error_response(
                    "github_repo_stats",
                    "repository",
                    Some(detail),
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        let response = match self
            .github_request(
                Method::GET,
                &format!("https://api.github.com/repos/{}", resolution.resolved_repo),
                None,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return github_error_response(
                    "github_repo_stats",
                    "repository",
                    Some(detail),
                    Some(&repo),
                    Some(&resolution),
                    error,
                );
            }
        };

        if !response.is_object() {
            return github_error_response(
                "github_repo_stats",
                "repository",
                Some(detail),
                Some(&repo),
                Some(&resolution),
                "Error: unexpected GitHub response",
            );
        }

        let mut repository = json!({
            "name": json_value_at_path(&response, &["name"]).and_then(Value::as_str).unwrap_or(""),
            "full_name": json_value_at_path(&response, &["full_name"]).and_then(Value::as_str).unwrap_or(""),
            "owner": json_value_at_path(&response, &["owner", "login"]).and_then(Value::as_str).unwrap_or(""),
            "description": github_excerpt(
                json_value_at_path(&response, &["description"]).and_then(Value::as_str).unwrap_or(""),
                Some(detail.body_limit().max(120)),
            ),
            "stars": json_u64(&response, &["stargazers_count"]),
            "forks": json_u64(&response, &["forks_count"]),
            "watchers": json_u64(&response, &["subscribers_count"])
                .or_else(|| json_u64(&response, &["watchers_count"])),
            "open_issues": json_u64(&response, &["open_issues_count"]),
            "language": json_value_at_path(&response, &["language"]).and_then(Value::as_str),
            "default_branch": json_value_at_path(&response, &["default_branch"]).and_then(Value::as_str),
            "pushed_at": github_timestamp(json_value_at_path(&response, &["pushed_at"]).and_then(Value::as_str)),
            "updated_at": github_timestamp(json_value_at_path(&response, &["updated_at"]).and_then(Value::as_str)),
            "created_at": github_timestamp(json_value_at_path(&response, &["created_at"]).and_then(Value::as_str)),
            "homepage": json_value_at_path(&response, &["homepage"]).and_then(Value::as_str),
            "html_url": json_value_at_path(&response, &["html_url"]).and_then(Value::as_str).unwrap_or(""),
            "archived": json_value_at_path(&response, &["archived"]).and_then(Value::as_bool).unwrap_or(false),
            "fork": json_value_at_path(&response, &["fork"]).and_then(Value::as_bool).unwrap_or(false),
        });

        if !matches!(detail, GithubDetail::Brief) {
            repository["topics"] = serde_json::Value::Array(
                json_value_at_path(&response, &["topics"])
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            repository["license"] = json_value_at_path(&response, &["license", "spdx_id"])
                .or_else(|| json_value_at_path(&response, &["license", "name"]))
                .cloned()
                .unwrap_or(Value::Null);
        }

        github_response_string(json!({
            "ok": true,
            "tool": "github_repo_stats",
            "detail": detail.as_str(),
            "requested_repo": repo,
            "resolved_repo": resolution_output_repo(&resolution),
            "resolved_by_search": resolution.resolved_by_search,
            "count": 1,
            "repository": repository,
            "error": Value::Null,
        }))
    }

    pub async fn github_create_issue(&self, args: &Value) -> String {
        let repo = match args.get("repo").and_then(Value::as_str) {
            Some(r) => r.to_string(),
            None => {
                return github_error_response(
                    "github_create_issue",
                    "issue",
                    None,
                    None,
                    None,
                    GITHUB_MISSING_REPO_ERROR,
                );
            }
        };
        let title = match args.get("title").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                return github_error_response(
                    "github_create_issue",
                    "issue",
                    None,
                    Some(&repo),
                    None,
                    "Error: missing or empty 'title'",
                );
            }
        };
        if !repo.contains('/') {
            return github_error_response(
                "github_create_issue",
                "issue",
                None,
                Some(&repo),
                None,
                "Error: github_create_issue requires repo in 'owner/repo' form",
            );
        }
        if self.current_token().is_none() {
            return github_error_response(
                "github_create_issue",
                "issue",
                None,
                Some(&repo),
                None,
                "Error: GITHUB_TOKEN is required for github_create_issue",
            );
        }
        let body = args.get("body").and_then(Value::as_str).unwrap_or("");
        let labels = args.get("labels").and_then(Value::as_str).unwrap_or("");

        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": if labels.is_empty() { vec![] } else { labels.split(',').map(|s| s.trim()).collect::<Vec<_>>() }
        });
        let response = match self
            .github_request(
                Method::POST,
                &format!("https://api.github.com/repos/{repo}/issues"),
                Some(&payload),
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return github_error_response(
                    "github_create_issue",
                    "issue",
                    None,
                    Some(&repo),
                    None,
                    error,
                );
            }
        };

        if !response.is_object() {
            return github_error_response(
                "github_create_issue",
                "issue",
                None,
                Some(&repo),
                None,
                "Error: unexpected GitHub response",
            );
        }

        github_response_string(json!({
            "ok": true,
            "tool": "github_create_issue",
            "detail": Value::Null,
            "requested_repo": repo,
            "resolved_repo": Value::Null,
            "resolved_by_search": false,
            "count": 1,
            "issue": {
                "number": json_u64(&response, &["number"]),
                "title": json_value_at_path(&response, &["title"]).and_then(Value::as_str).unwrap_or(""),
                "state": json_value_at_path(&response, &["state"]).and_then(Value::as_str).unwrap_or("?"),
                "created_at": github_timestamp(json_value_at_path(&response, &["created_at"]).and_then(Value::as_str)),
                "labels": github_label_names(&response),
                "url": json_value_at_path(&response, &["html_url"]).and_then(Value::as_str).unwrap_or(""),
            },
            "error": Value::Null,
        }))
    }

    pub async fn github_commit_ci_conclusion(
        &self,
        repo: &str,
        sha: &str,
    ) -> Result<String, String> {
        // Use check-runs API (covers GitHub Actions) with fallback to legacy commit status API.
        let check_response = self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/repos/{repo}/commits/{sha}/check-runs?per_page=100"
                ),
                None,
            )
            .await;

        if let Ok(ref response) = check_response
            && let Some(runs) = response.get("check_runs").and_then(Value::as_array)
            && !runs.is_empty()
        {
            let any_failure = runs.iter().any(|run| {
                matches!(
                    json_value_at_path(run, &["conclusion"]).and_then(Value::as_str),
                    Some("failure") | Some("timed_out") | Some("action_required")
                )
            });
            let any_pending = runs.iter().any(|run| {
                json_value_at_path(run, &["status"])
                    .and_then(Value::as_str)
                    .is_some_and(|s| s != "completed")
            });
            return Ok(if any_failure {
                "failure"
            } else if any_pending {
                "pending"
            } else {
                "success"
            }
            .to_string());
        }

        // Fallback: legacy commit status API
        let response = self
            .github_request(
                Method::GET,
                &format!("https://api.github.com/repos/{repo}/commits/{sha}/status"),
                None,
            )
            .await?;
        Ok(github_normalize_conclusion(
            json_value_at_path(&response, &["state"]).and_then(Value::as_str),
            None,
        ))
    }

    async fn github_pr_files(
        &self,
        repo: &str,
        pr_number: u64,
        detail: GithubDetail,
    ) -> Result<Vec<Value>, String> {
        let response = self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/repos/{repo}/pulls/{pr_number}/files?per_page={}",
                    if detail == GithubDetail::Full { 20 } else { 10 }
                ),
                None,
            )
            .await?;
        let Some(files) = response.as_array() else {
            return Err("Error: unexpected GitHub files response".to_string());
        };

        let mut files = files.to_vec();
        files.sort_by(|left, right| {
            let left_changes = json_u64(left, &["changes"]).unwrap_or(0);
            let right_changes = json_u64(right, &["changes"]).unwrap_or(0);
            right_changes.cmp(&left_changes)
        });

        Ok(files
            .into_iter()
            .take(if detail == GithubDetail::Full { 20 } else { 10 })
            .map(|file| github_pr_file_item(&file, detail))
            .collect())
    }

    async fn github_pr_review_comments(
        &self,
        repo: &str,
        pr_number: u64,
        detail: GithubDetail,
    ) -> Result<Vec<Value>, String> {
        let response = self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/repos/{repo}/pulls/{pr_number}/comments?per_page={}",
                    if detail == GithubDetail::Full { 20 } else { 10 }
                ),
                None,
            )
            .await?;
        let Some(comments) = response.as_array() else {
            return Err("Error: unexpected GitHub review comments response".to_string());
        };

        Ok(comments
            .iter()
            .take(if detail == GithubDetail::Full { 20 } else { 10 })
            .map(|comment| {
                json!({
                    "author": json_value_at_path(comment, &["user", "login"]).and_then(Value::as_str).unwrap_or("?"),
                    "created_at": github_timestamp(json_value_at_path(comment, &["created_at"]).and_then(Value::as_str)),
                    "body": github_excerpt(
                        json_value_at_path(comment, &["body"]).and_then(Value::as_str).unwrap_or(""),
                        Some(200),
                    ),
                    "url": json_value_at_path(comment, &["html_url"]).and_then(Value::as_str).unwrap_or(""),
                })
            })
            .collect())
    }

    async fn github_issue_comments(
        &self,
        repo: &str,
        issue_number: u64,
        detail: GithubDetail,
    ) -> Result<Vec<Value>, String> {
        let response = self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/repos/{repo}/issues/{issue_number}/comments?per_page={}",
                    if detail == GithubDetail::Full { 20 } else { 3 }
                ),
                None,
            )
            .await?;
        let Some(comments) = response.as_array() else {
            return Err("Error: unexpected GitHub issue comments response".to_string());
        };

        Ok(comments
            .iter()
            .take(if detail == GithubDetail::Full { 20 } else { 3 })
            .map(|comment| {
                json!({
                    "author": json_value_at_path(comment, &["user", "login"]).and_then(Value::as_str).unwrap_or("?"),
                    "created_at": github_timestamp(json_value_at_path(comment, &["created_at"]).and_then(Value::as_str)),
                    "body": github_excerpt(
                        json_value_at_path(comment, &["body"]).and_then(Value::as_str).unwrap_or(""),
                        Some(200),
                    ),
                    "url": json_value_at_path(comment, &["html_url"]).and_then(Value::as_str).unwrap_or(""),
                })
            })
            .collect())
    }

    async fn github_workflow_run_jobs(
        &self,
        repo: &str,
        run_id: u64,
        detail: GithubDetail,
    ) -> Result<(Vec<String>, Vec<Value>, Vec<Value>), String> {
        let response = self
            .github_request(
                Method::GET,
                &format!(
                    "https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"
                ),
                None,
            )
            .await?;
        let Some(jobs) = response.get("jobs").and_then(Value::as_array) else {
            return Err("Error: unexpected GitHub jobs response".to_string());
        };

        let mut failed_jobs = Vec::new();
        let mut failed_steps = Vec::new();
        let mut all_jobs = Vec::new();

        for job in jobs {
            let conclusion = github_normalize_conclusion(
                json_value_at_path(job, &["conclusion"]).and_then(Value::as_str),
                json_value_at_path(job, &["status"]).and_then(Value::as_str),
            );
            let job_name = json_value_at_path(job, &["name"])
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            if conclusion == "failure" {
                failed_jobs.push(job_name.clone());
            }

            if detail == GithubDetail::Full {
                all_jobs.push(json!({
                    "name": job_name,
                    "conclusion": conclusion,
                    "started_at": github_timestamp(json_value_at_path(job, &["started_at"]).and_then(Value::as_str)),
                    "completed_at": github_timestamp(json_value_at_path(job, &["completed_at"]).and_then(Value::as_str)),
                }));
            }

            if detail.includes_detailed() {
                let mut job_failed_steps = job
                    .get("steps")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|step| {
                        let step_conclusion = github_normalize_conclusion(
                            json_value_at_path(step, &["conclusion"]).and_then(Value::as_str),
                            json_value_at_path(step, &["status"]).and_then(Value::as_str),
                        );
                        if step_conclusion == "failure" {
                            Some(json!({
                                "job": job_name,
                                "step": json_value_at_path(step, &["name"]).and_then(Value::as_str).unwrap_or("?"),
                                "conclusion": step_conclusion,
                                "number": json_u64(step, &["number"]),
                                "log_snippet": Value::Null,
                            }))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                if detail == GithubDetail::Detailed {
                    if let Some(first) = job_failed_steps.into_iter().next() {
                        failed_steps.push(first);
                    }
                } else {
                    failed_steps.append(&mut job_failed_steps);
                }
            }
        }

        Ok((failed_jobs, failed_steps, all_jobs))
    }
}

#[derive(Clone, Debug)]
struct GithubRepoResolution {
    resolved_repo: String,
    resolved_by_search: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GithubDetail {
    Brief,
    Normal,
    Detailed,
    Full,
}

impl GithubDetail {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("brief") {
            "brief" => Ok(Self::Brief),
            "normal" => Ok(Self::Normal),
            "detailed" => Ok(Self::Detailed),
            "full" => Ok(Self::Full),
            other => Err(format!(
                "Error: invalid detail '{other}'. Use one of: brief, normal, detailed, full"
            )),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Brief => "brief",
            Self::Normal => "normal",
            Self::Detailed => "detailed",
            Self::Full => "full",
        }
    }

    pub(crate) fn includes_normal(self) -> bool {
        matches!(self, Self::Normal | Self::Detailed | Self::Full)
    }

    pub(crate) fn includes_detailed(self) -> bool {
        matches!(self, Self::Detailed | Self::Full)
    }

    pub(crate) fn title_limit(self) -> usize {
        match self {
            Self::Brief | Self::Normal => 80,
            Self::Detailed | Self::Full => usize::MAX,
        }
    }

    pub(crate) fn body_limit(self) -> usize {
        match self {
            Self::Brief => 0,
            Self::Normal => 200,
            Self::Detailed => 500,
            Self::Full => 2000,
        }
    }

    pub(crate) fn diff_limit(self) -> usize {
        match self {
            Self::Brief | Self::Normal => 0,
            Self::Detailed => 500,
            Self::Full => 2000,
        }
    }

    pub(crate) fn list_cap(self) -> usize {
        match self {
            Self::Brief | Self::Normal => 10,
            Self::Detailed => 20,
            Self::Full => 50,
        }
    }
}

fn json_value_at_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn json_u64(value: &Value, path: &[&str]) -> Option<u64> {
    json_value_at_path(value, path).and_then(Value::as_u64)
}

fn github_api_error_message(
    status: StatusCode,
    value: &Value,
    github_token_present: bool,
) -> String {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("GitHub request failed");
    let mut details = vec![message.to_string()];

    if let Some(errors) = value.get("errors").and_then(Value::as_array) {
        let flattened = errors
            .iter()
            .take(3)
            .map(|entry| {
                let field = entry.get("field").and_then(Value::as_str).unwrap_or("");
                let code = entry.get("code").and_then(Value::as_str).unwrap_or("");
                let detail = entry.get("message").and_then(Value::as_str).unwrap_or("");
                [field, code, detail]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(": ")
            })
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>();
        if !flattened.is_empty() {
            details.push(format!("details: {}", flattened.join("; ")));
        }
    }

    if status == StatusCode::NOT_FOUND && !github_token_present {
        details.push("repo may be missing, private, or require GITHUB_TOKEN".to_string());
    }

    if let Some(doc_url) = value.get("documentation_url").and_then(Value::as_str) {
        details.push(format!("docs: {doc_url}"));
    }

    format!(
        "Error: GitHub API HTTP {}: {}",
        status.as_u16(),
        details.join(" | ")
    )
}

fn github_requested_limit(requested: Option<u64>, detail: GithubDetail, default: usize) -> usize {
    requested
        .map(|value| value as usize)
        .unwrap_or(default)
        .min(detail.list_cap())
}

fn github_response_string(value: Value) -> String {
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

fn resolution_output_repo(resolution: &GithubRepoResolution) -> Option<String> {
    resolution
        .resolved_by_search
        .then(|| resolution.resolved_repo.clone())
}

fn github_error_response(
    tool: &str,
    payload_key: &str,
    detail: Option<GithubDetail>,
    requested_repo: Option<&str>,
    resolution: Option<&GithubRepoResolution>,
    error: impl Into<String>,
) -> String {
    let mut response = json!({
        "ok": false,
        "tool": tool,
        "detail": detail.map(GithubDetail::as_str),
        "requested_repo": requested_repo,
        "resolved_repo": resolution.and_then(resolution_output_repo),
        "resolved_by_search": resolution.map(|value| value.resolved_by_search).unwrap_or(false),
        "count": 0,
        "error": error.into(),
    });
    response
        .as_object_mut()
        .expect("github error response should be an object")
        .insert(payload_key.to_string(), Value::Null);
    github_response_string(response)
}

fn github_normalize_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn github_pick_resolved_repo(
    repo: &str,
    items: &[Value],
    preferred_repos: &[String],
) -> Result<String, String> {
    let normalized_repo = github_normalize_name(repo);
    let exact_name_matches = items
        .iter()
        .filter(|item| {
            item.get("name")
                .and_then(Value::as_str)
                .map(|name| {
                    name.eq_ignore_ascii_case(repo)
                        || github_normalize_name(name) == normalized_repo
                })
                .unwrap_or(false)
        })
        .filter_map(|item| item.get("full_name").and_then(Value::as_str))
        .collect::<Vec<_>>();

    if exact_name_matches.len() > 1 {
        // Multiple exact matches — check if one is preferred
        for candidate in &exact_name_matches {
            if preferred_repos
                .iter()
                .any(|pref| pref.eq_ignore_ascii_case(candidate))
            {
                return Ok(candidate.to_string());
            }
        }
        return Err(format!(
            "Error: repo name '{repo}' is ambiguous. Use owner/repo, e.g. {}",
            exact_name_matches.join(", ")
        ));
    }

    if let Some(found) = exact_name_matches.first() {
        return Ok((*found).to_string());
    }

    let candidates = items
        .iter()
        .take(3)
        .filter_map(|item| item.get("full_name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        Err(format!("Could not resolve repo '{repo}'"))
    } else {
        Err(format!(
            "Error: repo name '{repo}' did not resolve safely. Use owner/repo, e.g. {}",
            candidates.join(", ")
        ))
    }
}

fn github_normalize_conclusion(conclusion: Option<&str>, status: Option<&str>) -> String {
    match conclusion.or(status) {
        Some("success") => "success",
        Some("failure")
        | Some("timed_out")
        | Some("action_required")
        | Some("startup_failure")
        | Some("stale")
        | Some("error") => "failure",
        Some("cancelled") => "cancelled",
        Some("skipped") | Some("neutral") => "skipped",
        Some("queued") | Some("in_progress") | Some("waiting") | Some("requested") | None => {
            "pending"
        }
        Some(_) => "unknown",
    }
    .to_string()
}

fn github_timestamp(value: Option<&str>) -> String {
    value
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn github_duration_seconds(start: Option<&str>, end: Option<&str>) -> Option<i64> {
    let start = DateTime::parse_from_rfc3339(start?).ok()?;
    let end = DateTime::parse_from_rfc3339(end?).ok()?;
    Some(end.signed_duration_since(start).num_seconds().max(0))
}

fn github_excerpt(value: &str, limit: Option<usize>) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    match limit {
        Some(0) => String::new(),
        Some(max) => {
            let char_count = normalized.chars().count();
            if char_count > max {
                format!(
                    "{} [truncated]",
                    normalized.chars().take(max).collect::<String>()
                )
            } else {
                normalized
            }
        }
        None => normalized,
    }
}

fn github_title_value(value: Option<&str>, detail: GithubDetail) -> String {
    github_excerpt(value.unwrap_or(""), Some(detail.title_limit()))
}

fn github_label_names(value: &Value) -> Vec<String> {
    value
        .get("labels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| label.get("name").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn github_reviewer_names(value: &Value) -> Vec<String> {
    value
        .get("requested_reviewers")
        .and_then(Value::as_array)
        .map(|reviewers| {
            reviewers
                .iter()
                .filter_map(|reviewer| reviewer.get("login").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn github_primary_assignee(value: &Value) -> Option<String> {
    value
        .get("assignee")
        .and_then(|assignee| assignee.get("login"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            value
                .get("assignees")
                .and_then(Value::as_array)
                .and_then(|assignees| assignees.first())
                .and_then(|assignee| assignee.get("login"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

fn github_milestone_title(value: &Value) -> Option<String> {
    value
        .get("milestone")
        .and_then(|milestone| milestone.get("title"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn github_pr_state(value: &Value) -> String {
    if value
        .get("merged_at")
        .is_some_and(|merged| !merged.is_null())
    {
        "merged".to_string()
    } else {
        json_value_at_path(value, &["state"])
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    }
}

fn github_pr_file_item(file: &Value, detail: GithubDetail) -> Value {
    json!({
        "path": json_value_at_path(file, &["filename"]).and_then(Value::as_str).unwrap_or(""),
        "status": json_value_at_path(file, &["status"]).and_then(Value::as_str).unwrap_or("unknown"),
        "changes": json_u64(file, &["changes"]),
        "additions": json_u64(file, &["additions"]),
        "deletions": json_u64(file, &["deletions"]),
        "diff_summary": if detail == GithubDetail::Full {
            github_excerpt(
                json_value_at_path(file, &["patch"]).and_then(Value::as_str).unwrap_or(""),
                Some(detail.diff_limit()),
            )
        } else {
            String::new()
        },
    })
}

fn github_pr_list_item(pr: &Value, detail: GithubDetail) -> Value {
    let body = json_value_at_path(pr, &["body"])
        .and_then(Value::as_str)
        .unwrap_or("");
    json!({
        "number": json_u64(pr, &["number"]),
        "title": github_title_value(json_value_at_path(pr, &["title"]).and_then(Value::as_str), detail),
        "author": json_value_at_path(pr, &["user", "login"]).and_then(Value::as_str).unwrap_or("?"),
        "state": github_pr_state(pr),
        "created_at": github_timestamp(json_value_at_path(pr, &["created_at"]).and_then(Value::as_str)),
        "body_summary": if detail.includes_normal() {
            github_excerpt(body, Some(detail.body_limit()))
        } else {
            String::new()
        },
        "labels": if detail.includes_normal() { github_label_names(pr) } else { Vec::<String>::new() },
        "reviewers": if detail.includes_normal() { github_reviewer_names(pr) } else { Vec::<String>::new() },
        "changed_files": None::<u64>,
        "additions": None::<u64>,
        "deletions": None::<u64>,
        "review_comments_count": if detail.includes_detailed() { json_u64(pr, &["review_comments"]) } else { None::<u64> },
        "merge_status": None::<String>,
        "conflicts": None::<bool>,
        "url": json_value_at_path(pr, &["html_url"]).and_then(Value::as_str).unwrap_or(""),
    })
}

fn github_issue_list_item(issue: &Value, detail: GithubDetail) -> Value {
    let body = json_value_at_path(issue, &["body"])
        .and_then(Value::as_str)
        .unwrap_or("");
    json!({
        "number": json_u64(issue, &["number"]),
        "title": github_title_value(json_value_at_path(issue, &["title"]).and_then(Value::as_str), detail),
        "state": json_value_at_path(issue, &["state"]).and_then(Value::as_str).unwrap_or("?"),
        "labels": github_label_names(issue),
        "created_at": github_timestamp(json_value_at_path(issue, &["created_at"]).and_then(Value::as_str)),
        "author": json_value_at_path(issue, &["user", "login"]).and_then(Value::as_str).unwrap_or("?"),
        "body": if detail.includes_normal() { github_excerpt(body, Some(detail.body_limit())) } else { String::new() },
        "assignee": if detail.includes_normal() { github_primary_assignee(issue) } else { None::<String> },
        "milestone": if detail.includes_normal() { github_milestone_title(issue) } else { None::<String> },
        "comment_count": if detail.includes_normal() { json_u64(issue, &["comments"]) } else { None::<u64> },
        "comments": Vec::<Value>::new(),
        "linked_prs": Vec::<Value>::new(),
        "url": json_value_at_path(issue, &["html_url"]).and_then(Value::as_str).unwrap_or(""),
    })
}

/// Extract owner/repo from a git remote line.
pub fn extract_github_owner_repo(remote_line: &str) -> Option<String> {
    let parts: Vec<&str> = remote_line.split_whitespace().collect();
    let url = parts.get(1)?;
    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if url.contains("://github.com/") {
        url.rsplit_once("github.com/")?.1
    } else {
        return None;
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.contains('/') && !path.contains(' ') {
        Some(path.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enforce_github_api_url_accepts_official_host_https() {
        assert!(enforce_github_api_url("https://api.github.com/repos/o/r").is_ok());
        assert!(enforce_github_api_url("https://api.github.com:443/user").is_ok());
    }

    #[test]
    fn enforce_github_api_url_rejects_non_https() {
        assert!(enforce_github_api_url("http://api.github.com/user").is_err());
    }

    #[test]
    fn enforce_github_api_url_rejects_non_github_host() {
        assert!(enforce_github_api_url("https://github.com/repos/o/r").is_err());
        assert!(enforce_github_api_url("https://evil.com/").is_err());
        assert!(enforce_github_api_url("https://api.github.com.evil.com/").is_err());
    }

    #[test]
    pub fn github_detail_defaults_to_brief() {
        assert_eq!(GithubDetail::parse(None).unwrap(), GithubDetail::Brief);
        assert_eq!(
            GithubDetail::parse(Some("full")).unwrap(),
            GithubDetail::Full
        );
    }

    #[test]
    pub fn github_timestamp_is_normalized() {
        assert_eq!(
            github_timestamp(Some("2026-03-04T06:47:17Z")),
            "2026-03-04 06:47"
        );
    }

    #[test]
    pub fn github_excerpt_marks_truncation() {
        assert_eq!(
            github_excerpt("one two three four", Some(7)),
            "one two [truncated]"
        );
    }

    #[test]
    pub fn github_conclusion_is_normalized() {
        assert_eq!(github_normalize_conclusion(None, Some("queued")), "pending");
        assert_eq!(
            github_normalize_conclusion(Some("success"), None),
            "success"
        );
        assert_eq!(
            github_normalize_conclusion(Some("timed_out"), None),
            "failure"
        );
    }

    #[test]
    pub fn github_repo_resolution_requires_safe_match() {
        let items = vec![
            json!({"name": "memoriax", "full_name": "someone/memoriax"}),
            json!({"name": "Memoria", "full_name": "MatrixOrigin/Memoria"}),
        ];
        assert_eq!(
            github_pick_resolved_repo("memoria", &items, &[]).unwrap(),
            "MatrixOrigin/Memoria"
        );

        let unsafe_items = vec![
            json!({"name": "memoriax", "full_name": "someone/memoriax"}),
            json!({"name": "memory", "full_name": "else/memory"}),
        ];
        assert!(github_pick_resolved_repo("memoria", &unsafe_items, &[]).is_err());
    }

    #[test]
    fn github_repo_resolution_prefers_preferred_repo_on_ambiguity() {
        // Two exact name matches — normally ambiguous
        let items = vec![
            json!({"name": "Memoria", "full_name": "Albeoris/Memoria"}),
            json!({"name": "Memoria", "full_name": "MatrixOrigin/Memoria"}),
        ];
        // Without preferred → ambiguous error
        assert!(github_pick_resolved_repo("memoria", &items, &[]).is_err());

        // With preferred → picks the preferred one
        let preferred = vec!["matrixorigin/memoria".to_string()];
        assert_eq!(
            github_pick_resolved_repo("memoria", &items, &preferred).unwrap(),
            "MatrixOrigin/Memoria"
        );
    }

    #[test]
    fn extract_github_owner_repo_ssh() {
        let line = "origin\tgit@github.com:MatrixOrigin/Memoria.git (fetch)";
        assert_eq!(
            super::extract_github_owner_repo(line),
            Some("MatrixOrigin/Memoria".to_string())
        );
    }

    #[test]
    fn extract_github_owner_repo_https() {
        let line = "origin\thttps://github.com/matrixorigin/mo-dev-agent.git (push)";
        assert_eq!(
            super::extract_github_owner_repo(line),
            Some("matrixorigin/mo-dev-agent".to_string())
        );
    }

    #[test]
    fn extract_github_owner_repo_non_github() {
        let line = "origin\thttps://gitlab.com/someone/project.git (fetch)";
        assert_eq!(super::extract_github_owner_repo(line), None);
    }

    // ── github_pick_resolved_repo edge cases ──

    #[test]
    fn pick_resolved_repo_empty_items() {
        let result = github_pick_resolved_repo("something", &[], &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Could not resolve"));
    }

    #[test]
    fn pick_resolved_repo_no_exact_name_match() {
        let items = vec![
            json!({"name": "memoria-fork", "full_name": "someone/memoria-fork"}),
            json!({"name": "memoria-v2", "full_name": "other/memoria-v2"}),
        ];
        let result = github_pick_resolved_repo("memoria", &items, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("did not resolve safely"));
    }

    #[test]
    fn pick_resolved_repo_normalized_name_match() {
        // "mo-dev-agent" normalized matches "modevagent"
        let items = vec![json!({"name": "mo-dev-agent", "full_name": "org/mo-dev-agent"})];
        assert_eq!(
            github_pick_resolved_repo("mo_dev_agent", &items, &[]).unwrap(),
            "org/mo-dev-agent"
        );
    }

    #[test]
    fn pick_resolved_repo_missing_full_name_field() {
        // Malformed response — item without full_name
        let items = vec![
            json!({"name": "memoria"}), // no full_name
        ];
        let result = github_pick_resolved_repo("memoria", &items, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn pick_resolved_repo_case_insensitive_preference() {
        let items = vec![
            json!({"name": "Memoria", "full_name": "Albeoris/Memoria"}),
            json!({"name": "Memoria", "full_name": "MatrixOrigin/Memoria"}),
        ];
        // Preference is lowercased
        let preferred = vec!["MATRIXORIGIN/MEMORIA".to_lowercase()];
        assert_eq!(
            github_pick_resolved_repo("memoria", &items, &preferred).unwrap(),
            "MatrixOrigin/Memoria"
        );
    }

    #[test]
    fn pick_resolved_repo_single_exact_match_ignores_preferences() {
        // Only one exact match — preferences don't matter
        let items = vec![json!({"name": "Memoria", "full_name": "Albeoris/Memoria"})];
        let preferred = vec!["matrixorigin/memoria".to_string()];
        // The single exact match wins regardless of preference
        assert_eq!(
            github_pick_resolved_repo("memoria", &items, &preferred).unwrap(),
            "Albeoris/Memoria"
        );
    }

    #[test]
    pub fn github_missing_repo_error_guides_recovery() {
        let response = github_error_response(
            "github_ci_status",
            "workflow_runs",
            Some(GithubDetail::Brief),
            None,
            None,
            GITHUB_MISSING_REPO_ERROR,
        );
        assert!(response.contains("missing 'repo'"));
        assert!(response.contains("recent conversation"));
        assert!(response.contains("memoria"));
    }

    // ── Helper function correctness ──

    #[test]
    fn github_detail_parse_all_valid_levels() {
        assert_eq!(
            GithubDetail::parse(Some("brief")).unwrap(),
            GithubDetail::Brief
        );
        assert_eq!(
            GithubDetail::parse(Some("normal")).unwrap(),
            GithubDetail::Normal
        );
        assert_eq!(
            GithubDetail::parse(Some("detailed")).unwrap(),
            GithubDetail::Detailed
        );
        assert_eq!(
            GithubDetail::parse(Some("full")).unwrap(),
            GithubDetail::Full
        );
        assert_eq!(GithubDetail::parse(None).unwrap(), GithubDetail::Brief);
    }

    #[test]
    fn github_detail_parse_invalid_returns_error() {
        let err = GithubDetail::parse(Some("verbose")).unwrap_err();
        assert!(err.contains("brief"), "should suggest valid options: {err}");
    }

    #[test]
    fn github_detail_list_caps() {
        // Brief should have tighter limits than Full
        assert!(GithubDetail::Brief.list_cap() <= GithubDetail::Normal.list_cap());
        assert!(GithubDetail::Normal.list_cap() <= GithubDetail::Full.list_cap());
    }

    #[test]
    fn github_requested_limit_respects_cap() {
        // Requesting 1000 but brief cap is much lower
        let result = github_requested_limit(Some(1000), GithubDetail::Brief, 10);
        assert!(result <= GithubDetail::Brief.list_cap());
    }

    #[test]
    fn github_requested_limit_uses_default() {
        assert_eq!(github_requested_limit(None, GithubDetail::Brief, 10), 10);
        assert_eq!(github_requested_limit(None, GithubDetail::Brief, 5), 5);
    }

    #[test]
    fn github_timestamp_valid_rfc3339() {
        let result = github_timestamp(Some("2025-01-15T10:30:00Z"));
        assert!(
            result.contains("2025-01-15"),
            "should format date: {result}"
        );
    }

    #[test]
    fn github_timestamp_invalid_returns_question_mark() {
        assert_eq!(github_timestamp(Some("not-a-date")), "?");
        assert_eq!(github_timestamp(None), "?");
    }

    #[test]
    fn github_duration_seconds_valid() {
        let d = github_duration_seconds(Some("2025-01-01T00:00:00Z"), Some("2025-01-01T00:05:00Z"));
        assert_eq!(d, Some(300));
    }

    #[test]
    fn github_duration_seconds_invalid_returns_none() {
        assert_eq!(
            github_duration_seconds(None, Some("2025-01-01T00:00:00Z")),
            None
        );
        assert_eq!(
            github_duration_seconds(Some("bad"), Some("2025-01-01T00:00:00Z")),
            None
        );
    }

    #[test]
    fn github_excerpt_truncates_long_text() {
        let long = "a ".repeat(100);
        let result = github_excerpt(&long, Some(10));
        assert!(result.contains("[truncated]"));
        assert!(result.len() < long.len());
    }

    #[test]
    fn github_excerpt_no_limit_preserves_all() {
        let text = "hello world  foo   bar";
        assert_eq!(github_excerpt(text, None), "hello world foo bar");
    }

    #[test]
    fn github_normalize_name_strips_special_chars() {
        assert_eq!(github_normalize_name("Mo-Dev-Agent"), "modevagent");
        assert_eq!(github_normalize_name("Memoria"), "memoria");
        assert_eq!(github_normalize_name("my_repo-v2.0"), "myrepov20");
    }

    #[test]
    fn github_normalize_conclusion_all_states() {
        assert_eq!(
            github_normalize_conclusion(Some("success"), None),
            "success"
        );
        assert_eq!(
            github_normalize_conclusion(Some("failure"), None),
            "failure"
        );
        assert_eq!(
            github_normalize_conclusion(Some("timed_out"), None),
            "failure"
        );
        assert_eq!(
            github_normalize_conclusion(Some("cancelled"), None),
            "cancelled"
        );
        assert_eq!(
            github_normalize_conclusion(Some("skipped"), None),
            "skipped"
        );
        assert_eq!(
            github_normalize_conclusion(Some("neutral"), None),
            "skipped"
        );
        assert_eq!(github_normalize_conclusion(Some("queued"), None), "pending");
        assert_eq!(
            github_normalize_conclusion(Some("in_progress"), None),
            "pending"
        );
        assert_eq!(github_normalize_conclusion(None, None), "pending");
        assert_eq!(
            github_normalize_conclusion(Some("something_else"), None),
            "unknown"
        );
    }

    #[test]
    fn github_normalize_conclusion_status_fallback() {
        // When conclusion is None, falls back to status
        assert_eq!(
            github_normalize_conclusion(None, Some("success")),
            "success"
        );
        assert_eq!(github_normalize_conclusion(None, Some("error")), "failure");
    }

    #[test]
    fn github_pr_state_merged() {
        let pr = json!({"state": "closed", "merged_at": "2025-01-01T00:00:00Z"});
        assert_eq!(github_pr_state(&pr), "merged");
    }

    #[test]
    fn github_pr_state_not_merged() {
        let pr = json!({"state": "open", "merged_at": null});
        assert_eq!(github_pr_state(&pr), "open");
    }

    // ── Error response format ──

    #[test]
    fn github_error_response_structure() {
        let response = github_error_response(
            "github_list_prs",
            "pull_requests",
            Some(GithubDetail::Brief),
            Some("matrixorigin/memoria"),
            None,
            "some error message",
        );
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["ok"], json!(false));
        assert_eq!(parsed["tool"], "github_list_prs");
        assert_eq!(parsed["error"], "some error message");
        assert_eq!(parsed["count"], 0);
        assert!(
            parsed["pull_requests"].is_null(),
            "payload key should be null"
        );
    }

    #[test]
    fn github_error_response_with_resolution() {
        let resolution = GithubRepoResolution {
            resolved_repo: "MatrixOrigin/Memoria".into(),
            resolved_by_search: true,
        };
        let response = github_error_response(
            "github_ci_status",
            "workflow_runs",
            Some(GithubDetail::Normal),
            Some("memoria"),
            Some(&resolution),
            "API failure",
        );
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["resolved_repo"], "MatrixOrigin/Memoria");
        assert_eq!(parsed["resolved_by_search"], true);
    }

    // ── Item formatting ──

    #[test]
    fn github_pr_list_item_brief() {
        let pr = json!({
            "number": 42,
            "title": "Fix bug in parser",
            "user": {"login": "alice"},
            "state": "open",
            "merged_at": null,
            "created_at": "2025-06-01T10:00:00Z",
            "body": "This fixes the parser",
            "html_url": "https://github.com/org/repo/pull/42"
        });
        let item = github_pr_list_item(&pr, GithubDetail::Brief);
        assert_eq!(item["number"], 42);
        assert_eq!(item["author"], "alice");
        assert_eq!(item["state"], "open");
        // Brief mode should not include body
        assert_eq!(item["body_summary"], "");
    }

    #[test]
    fn github_pr_list_item_normal_includes_body() {
        let pr = json!({
            "number": 1,
            "title": "Feat",
            "user": {"login": "bob"},
            "state": "closed",
            "merged_at": "2025-06-02T00:00:00Z",
            "created_at": "2025-06-01T10:00:00Z",
            "body": "Detailed description here",
            "html_url": "https://github.com/org/repo/pull/1"
        });
        let item = github_pr_list_item(&pr, GithubDetail::Normal);
        assert_eq!(item["state"], "merged"); // has merged_at
        assert!(!item["body_summary"].as_str().unwrap().is_empty());
    }

    #[test]
    fn github_issue_list_item_structure() {
        let issue = json!({
            "number": 10,
            "title": "Feature request",
            "state": "open",
            "user": {"login": "carol"},
            "created_at": "2025-05-01T00:00:00Z",
            "body": "Please add X",
            "comments": 3,
            "labels": [{"name": "enhancement"}],
            "html_url": "https://github.com/org/repo/issues/10"
        });
        let item = github_issue_list_item(&issue, GithubDetail::Brief);
        assert_eq!(item["number"], 10);
        assert_eq!(item["state"], "open");
        assert_eq!(item["author"], "carol");
    }
}
