//! File-based GatewayStore — JSON files in a local directory.
//!
//! Zero-dependency operation: no database required. Suitable for single-user
//! setups or environments where installing MySQL/SQLite is impractical.
//!
//! Directory layout under `base_dir`:
//!   users.json        — keyed by "platform:user_id"
//!   sessions.json     — keyed by "platform:chat_id:cli_profile"
//!   cron_jobs.json    — keyed by job_id
//!   credentials.json  — keyed by "platform:user_id:type"
//!   pending/          — numbered JSON files
//!   usage/            — one YYYY-MM-DD.jsonl per day

use super::{
    CronJobRecord, CronJobSpec, DueJob, GatewayStore, PendingMessage, PlatformCredential,
    SessionRecord, StoreError, UsageRecord, UsageSummary, next_cron_run_str,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};

pub struct FileGatewayStore {
    base_dir: PathBuf,
    users: RwLock<HashMap<String, UserEntry>>,
    sessions: RwLock<HashMap<String, Vec<SessionEntry>>>,
    cron_jobs: RwLock<HashMap<String, CronJobEntry>>,
    credentials: RwLock<HashMap<String, CredentialEntry>>,
    pending_counter: AtomicI64,
    disk_write_lock: Mutex<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserEntry {
    platform: String,
    user_id: String,
    display_name: String,
    preferences: HashMap<String, String>,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionEntry {
    session_id: String,
    platform: String,
    chat_id: String,
    user_id: String,
    cli_profile: String,
    is_current: bool,
    created_at: String,
    last_active: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CronJobEntry {
    job_id: String,
    platform: String,
    chat_id: String,
    user_id: String,
    cron_expr: String,
    message: String,
    description: String,
    enabled: bool,
    last_run: Option<String>,
    next_run: Option<String>,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CredentialEntry {
    platform: String,
    user_id: String,
    credential_type: String,
    credentials: serde_json::Value,
    expires_at: Option<String>,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageEntry {
    platform: String,
    user_id: String,
    cli_profile: String,
    model: Option<String>,
    tokens_prompt: u64,
    tokens_completion: u64,
    tool_calls: u32,
    elapsed_ms: u64,
    created_at: String,
}

fn now_str() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn today_str() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

fn user_key(platform: &str, user_id: &str) -> String {
    format!("{platform}:{user_id}")
}

fn session_key(platform: &str, chat_id: &str, cli_profile: &str) -> String {
    format!("{platform}:{chat_id}:{cli_profile}")
}

fn cred_key(platform: &str, user_id: &str, credential_type: &str) -> String {
    format!("{platform}:{user_id}:{credential_type}")
}

impl FileGatewayStore {
    pub async fn open(base_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let base_dir = base_dir.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&base_dir).await?;
        tokio::fs::create_dir_all(base_dir.join("pending")).await?;
        tokio::fs::create_dir_all(base_dir.join("usage")).await?;

        let users = load_json_map(&base_dir.join("users.json")).await?;
        let sessions = load_json_map(&base_dir.join("sessions.json")).await?;
        let cron_jobs = load_json_map(&base_dir.join("cron_jobs.json")).await?;
        let credentials = load_json_map(&base_dir.join("credentials.json")).await?;

        let pending_counter = load_pending_counter(&base_dir.join("pending")).await?;

        Ok(Self {
            base_dir,
            users: RwLock::new(users),
            sessions: RwLock::new(sessions),
            cron_jobs: RwLock::new(cron_jobs),
            credentials: RwLock::new(credentials),
            pending_counter: AtomicI64::new(pending_counter),
            disk_write_lock: Mutex::new(()),
        })
    }

    async fn flush_users(&self) -> Result<(), StoreError> {
        let _guard = self.disk_write_lock.lock().await;
        let data = self.users.read().await;
        save_json_map(&self.base_dir.join("users.json"), &*data).await
    }

    async fn flush_sessions(&self) -> Result<(), StoreError> {
        let _guard = self.disk_write_lock.lock().await;
        let data = self.sessions.read().await;
        save_json_map(&self.base_dir.join("sessions.json"), &*data).await
    }

    async fn flush_cron_jobs(&self) -> Result<(), StoreError> {
        let _guard = self.disk_write_lock.lock().await;
        let data = self.cron_jobs.read().await;
        save_json_map(&self.base_dir.join("cron_jobs.json"), &*data).await
    }

    async fn flush_credentials(&self) -> Result<(), StoreError> {
        let _guard = self.disk_write_lock.lock().await;
        let data = self.credentials.read().await;
        save_json_map(&self.base_dir.join("credentials.json"), &*data).await
    }

    async fn persist_pending_counter_locked(&self, value: i64) -> Result<(), StoreError> {
        atomic_write(
            &self.base_dir.join("pending").join(".counter"),
            value.to_string().as_bytes(),
        )
        .await
    }
}

#[async_trait::async_trait]
impl GatewayStore for FileGatewayStore {
    async fn ensure_schema(&self) -> Result<(), StoreError> {
        Ok(())
    }

    // ─── Users ────────────────────────────────────────────────────────
    async fn upsert_user(
        &self,
        platform: &str,
        user_id: &str,
        display_name: &str,
    ) -> Result<(), StoreError> {
        let key = user_key(platform, user_id);
        let mut users = self.users.write().await;
        let entry = users.entry(key).or_insert_with(|| UserEntry {
            platform: platform.to_string(),
            user_id: user_id.to_string(),
            display_name: String::new(),
            preferences: HashMap::new(),
            created_at: now_str(),
        });
        entry.display_name = display_name.to_string();
        drop(users);
        self.flush_users().await
    }

    async fn set_user_preference(
        &self,
        platform: &str,
        user_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), StoreError> {
        let ukey = user_key(platform, user_id);
        let mut users = self.users.write().await;
        let Some(entry) = users.get_mut(&ukey) else {
            return Err(StoreError::NotFound(format!(
                "user not found: {platform}:{user_id}"
            )));
        };
        entry.preferences.insert(key.to_string(), value.to_string());
        drop(users);
        self.flush_users().await
    }

    async fn get_user_preference(
        &self,
        platform: &str,
        user_id: &str,
        key: &str,
    ) -> Result<Option<String>, StoreError> {
        let ukey = user_key(platform, user_id);
        let users = self.users.read().await;
        Ok(users
            .get(&ukey)
            .and_then(|e| e.preferences.get(key).cloned()))
    }

    async fn is_first_message(&self, platform: &str, user_id: &str) -> Result<bool, StoreError> {
        let key = user_key(platform, user_id);
        let users = self.users.read().await;
        Ok(!users.contains_key(&key))
    }

    // ─── Sessions ─────────────────────────────────────────────────────
    async fn get_current_session(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<Option<String>, StoreError> {
        let key = session_key(platform, chat_id, cli_profile);
        let sessions = self.sessions.read().await;
        Ok(sessions.get(&key).and_then(|entries| {
            entries
                .iter()
                .filter(|e| e.is_current)
                .max_by(|a, b| a.last_active.cmp(&b.last_active))
                .map(|e| e.session_id.clone())
        }))
    }

    async fn get_session_last_active(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<Option<String>, StoreError> {
        let key = session_key(platform, chat_id, cli_profile);
        let sessions = self.sessions.read().await;
        Ok(sessions.get(&key).and_then(|entries| {
            entries
                .iter()
                .filter(|e| e.is_current)
                .max_by(|a, b| a.last_active.cmp(&b.last_active))
                .map(|e| e.last_active.clone())
        }))
    }

    async fn set_current_session(
        &self,
        platform: &str,
        chat_id: &str,
        user_id: &str,
        session_id: &str,
        cli_profile: &str,
    ) -> Result<(), StoreError> {
        let key = session_key(platform, chat_id, cli_profile);
        let mut sessions = self.sessions.write().await;
        let entries = sessions.entry(key).or_insert_with(Vec::new);
        for e in entries.iter_mut() {
            e.is_current = false;
        }
        if let Some(existing) = entries.iter_mut().find(|e| e.session_id == session_id) {
            existing.is_current = true;
            existing.last_active = now_str();
        } else {
            entries.push(SessionEntry {
                session_id: session_id.to_string(),
                platform: platform.to_string(),
                chat_id: chat_id.to_string(),
                user_id: user_id.to_string(),
                cli_profile: cli_profile.to_string(),
                is_current: true,
                created_at: now_str(),
                last_active: now_str(),
            });
        }
        drop(sessions);
        self.flush_sessions().await
    }

    async fn touch_session(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<(), StoreError> {
        let key = session_key(platform, chat_id, cli_profile);
        let mut sessions = self.sessions.write().await;
        if let Some(entries) = sessions.get_mut(&key) {
            for e in entries.iter_mut().filter(|e| e.is_current) {
                e.last_active = now_str();
            }
        }
        drop(sessions);
        self.flush_sessions().await
    }

    async fn list_sessions(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<Vec<SessionRecord>, StoreError> {
        let key = session_key(platform, chat_id, cli_profile);
        let sessions = self.sessions.read().await;
        Ok(sessions
            .get(&key)
            .map(|entries| {
                let mut sorted: Vec<_> = entries.iter().collect();
                sorted.sort_by(|a, b| b.last_active.cmp(&a.last_active));
                sorted
                    .into_iter()
                    .take(20)
                    .map(|e| SessionRecord {
                        session_id: e.session_id.clone(),
                        is_current: e.is_current,
                        created_at: e.created_at.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn switch_session(
        &self,
        platform: &str,
        chat_id: &str,
        target_session_id: &str,
    ) -> Result<bool, StoreError> {
        let mut sessions = self.sessions.write().await;
        let mut found = false;
        for entries in sessions.values_mut() {
            if entries
                .iter()
                .any(|e| e.platform == platform && e.chat_id == chat_id)
            {
                for e in entries.iter_mut() {
                    if e.platform == platform && e.chat_id == chat_id {
                        if e.session_id == target_session_id {
                            e.is_current = true;
                            e.last_active = now_str();
                            found = true;
                        } else {
                            e.is_current = false;
                        }
                    }
                }
            }
        }
        drop(sessions);
        if found {
            self.flush_sessions().await?;
        }
        Ok(found)
    }

    async fn reset_session(
        &self,
        platform: &str,
        chat_id: &str,
        cli_profile: &str,
    ) -> Result<(), StoreError> {
        let key = session_key(platform, chat_id, cli_profile);
        let mut sessions = self.sessions.write().await;
        if let Some(entries) = sessions.get_mut(&key) {
            for e in entries.iter_mut().filter(|e| e.is_current) {
                e.is_current = false;
            }
        }
        drop(sessions);
        self.flush_sessions().await
    }

    // ─── Pending Messages ─────────────────────────────────────────────
    async fn save_pending_message(
        &self,
        platform: &str,
        chat_id: &str,
        user_id: &str,
        text: &str,
    ) -> Result<i64, StoreError> {
        let _guard = self.disk_write_lock.lock().await;
        let id = self.pending_counter.fetch_add(1, Ordering::Relaxed) + 1;
        self.persist_pending_counter_locked(id).await?;
        let entry = serde_json::json!({
            "id": id,
            "platform": platform,
            "chat_id": chat_id,
            "user_id": user_id,
            "text": text,
            "created_at": now_str(),
        });
        let path = self.base_dir.join("pending").join(format!("{id}.json"));
        let data = serde_json::to_string_pretty(&entry)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        atomic_write(&path, data.as_bytes()).await?;
        Ok(id)
    }

    async fn list_pending_messages(
        &self,
        platform: Option<&str>,
    ) -> Result<Vec<PendingMessage>, StoreError> {
        let dir = self.base_dir.join("pending");
        let mut messages = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let data = tokio::fs::read_to_string(&path).await?;
            let v: serde_json::Value = serde_json::from_str(&data)
                .map_err(|e| StoreError::Serialization(e.to_string()))?;
            let msg_platform = v["platform"].as_str().unwrap_or_default();
            if let Some(filter_platform) = platform
                && msg_platform != filter_platform
            {
                continue;
            }
            messages.push(PendingMessage {
                id: v["id"].as_i64().unwrap_or(0),
                platform: msg_platform.to_string(),
                chat_id: v["chat_id"].as_str().unwrap_or_default().to_string(),
                user_id: v["user_id"].as_str().unwrap_or_default().to_string(),
                text: v["text"].as_str().unwrap_or_default().to_string(),
            });
        }
        messages.sort_by_key(|m| m.id);
        if messages.len() > 50 {
            messages.truncate(50);
        }
        Ok(messages)
    }

    async fn delete_pending_message(&self, id: i64) -> Result<u64, StoreError> {
        let path = self.base_dir.join("pending").join(format!("{id}.json"));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(1),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    // ─── Cron Jobs ────────────────────────────────────────────────────
    async fn create_cron_job(&self, spec: &CronJobSpec) -> Result<(), StoreError> {
        let next = next_cron_run_str(&spec.cron_expr);
        let entry = CronJobEntry {
            job_id: spec.job_id.to_string(),
            platform: spec.platform.to_string(),
            chat_id: spec.chat_id.to_string(),
            user_id: spec.user_id.to_string(),
            cron_expr: spec.cron_expr.to_string(),
            message: spec.message.to_string(),
            description: spec.description.to_string(),
            enabled: true,
            last_run: None,
            next_run: Some(next),
            created_at: now_str(),
        };
        let mut jobs = self.cron_jobs.write().await;
        jobs.insert(spec.job_id.to_string(), entry);
        drop(jobs);
        self.flush_cron_jobs().await
    }

    async fn list_cron_jobs(
        &self,
        platform: &str,
        chat_id: &str,
    ) -> Result<Vec<CronJobRecord>, StoreError> {
        let jobs = self.cron_jobs.read().await;
        Ok(jobs
            .values()
            .filter(|j| j.platform == platform && j.chat_id == chat_id)
            .map(|j| CronJobRecord {
                job_id: j.job_id.clone(),
                cron_expr: j.cron_expr.clone(),
                description: j.description.clone(),
                enabled: j.enabled,
            })
            .collect())
    }

    async fn delete_cron_job(&self, job_id: &str) -> Result<bool, StoreError> {
        let mut jobs = self.cron_jobs.write().await;
        let removed = jobs.remove(job_id).is_some();
        drop(jobs);
        if removed {
            self.flush_cron_jobs().await?;
        }
        Ok(removed)
    }

    async fn get_due_jobs(&self) -> Result<Vec<DueJob>, StoreError> {
        let now = now_str();
        let jobs = self.cron_jobs.read().await;
        Ok(jobs
            .values()
            .filter(|j| {
                j.enabled
                    && j.next_run
                        .as_ref()
                        .map(|nr| nr.as_str() <= now.as_str())
                        .unwrap_or(true)
            })
            .map(|j| DueJob {
                job_id: j.job_id.clone(),
                platform: j.platform.clone(),
                chat_id: j.chat_id.clone(),
                message: j.message.clone(),
                cron_expr: j.cron_expr.clone(),
            })
            .collect())
    }

    async fn mark_job_run(&self, job_id: &str, cron_expr: &str) -> Result<(), StoreError> {
        let next = next_cron_run_str(cron_expr);
        let mut jobs = self.cron_jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            job.last_run = Some(now_str());
            job.next_run = Some(next);
        }
        drop(jobs);
        self.flush_cron_jobs().await
    }

    async fn update_cron_next_run(&self, job_id: &str, next_run: &str) -> Result<(), StoreError> {
        let mut jobs = self.cron_jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            job.next_run = Some(next_run.to_string());
        }
        drop(jobs);
        self.flush_cron_jobs().await
    }

    async fn get_cron_job_user_id(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let jobs = self.cron_jobs.read().await;
        Ok(jobs.get(job_id).map(|j| j.user_id.clone()))
    }

    // ─── Platform Credentials ─────────────────────────────────────────
    async fn save_credential(
        &self,
        platform: &str,
        user_id: &str,
        credential_type: &str,
        credentials: &serde_json::Value,
        expires_at: Option<&str>,
    ) -> Result<(), StoreError> {
        let key = cred_key(platform, user_id, credential_type);
        let entry = CredentialEntry {
            platform: platform.to_string(),
            user_id: user_id.to_string(),
            credential_type: credential_type.to_string(),
            credentials: credentials.clone(),
            expires_at: expires_at.map(String::from),
            updated_at: now_str(),
        };
        let mut creds = self.credentials.write().await;
        creds.insert(key, entry);
        drop(creds);
        self.flush_credentials().await
    }

    async fn get_credential(
        &self,
        platform: &str,
        user_id: &str,
        credential_type: &str,
    ) -> Result<Option<PlatformCredential>, StoreError> {
        let key = cred_key(platform, user_id, credential_type);
        let creds = self.credentials.read().await;
        Ok(creds.get(&key).map(|e| PlatformCredential {
            platform: e.platform.clone(),
            user_id: e.user_id.clone(),
            credential_type: e.credential_type.clone(),
            credentials: e.credentials.clone(),
            expires_at: e.expires_at.clone(),
        }))
    }

    async fn list_credentials(
        &self,
        platform: &str,
    ) -> Result<Vec<PlatformCredential>, StoreError> {
        let creds = self.credentials.read().await;
        Ok(creds
            .values()
            .filter(|e| e.platform == platform)
            .map(|e| PlatformCredential {
                platform: e.platform.clone(),
                user_id: e.user_id.clone(),
                credential_type: e.credential_type.clone(),
                credentials: e.credentials.clone(),
                expires_at: e.expires_at.clone(),
            })
            .collect())
    }

    async fn delete_credential(
        &self,
        platform: &str,
        user_id: &str,
        credential_type: &str,
    ) -> Result<bool, StoreError> {
        let key = cred_key(platform, user_id, credential_type);
        let mut creds = self.credentials.write().await;
        let removed = creds.remove(&key).is_some();
        drop(creds);
        if removed {
            self.flush_credentials().await?;
        }
        Ok(removed)
    }

    // ─── Usage ────────────────────────────────────────────────────────
    async fn record_usage(&self, record: &UsageRecord) -> Result<(), StoreError> {
        let day = today_str();
        let path = self.base_dir.join("usage").join(format!("{day}.jsonl"));
        let entry = UsageEntry {
            platform: record.platform.clone(),
            user_id: record.user_id.clone(),
            cli_profile: record.cli_profile.clone(),
            model: record.model.clone(),
            tokens_prompt: record.tokens_prompt,
            tokens_completion: record.tokens_completion,
            tool_calls: record.tool_calls,
            elapsed_ms: record.elapsed_ms,
            created_at: now_str(),
        };
        let mut line =
            serde_json::to_string(&entry).map_err(|e| StoreError::Serialization(e.to_string()))?;
        line.push('\n');
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    async fn get_usage_today(
        &self,
        platform: &str,
        user_id: &str,
    ) -> Result<UsageSummary, StoreError> {
        let day = today_str();
        let path = self.base_dir.join("usage").join(format!("{day}.jsonl"));
        load_usage_summary(&path, platform, user_id).await
    }

    async fn get_usage_total(
        &self,
        platform: &str,
        user_id: &str,
    ) -> Result<UsageSummary, StoreError> {
        let dir = self.base_dir.join("usage");
        let mut total = UsageSummary {
            messages: 0,
            tokens_prompt: 0,
            tokens_completion: 0,
            tool_calls: 0,
        };
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => return Ok(total),
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let day = load_usage_summary(&path, platform, user_id).await?;
                total.messages += day.messages;
                total.tokens_prompt += day.tokens_prompt;
                total.tokens_completion += day.tokens_completion;
                total.tool_calls += day.tool_calls;
            }
        }
        Ok(total)
    }
}

// ─── File I/O helpers ─────────────────────────────────────────────────────

async fn load_json_map<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<HashMap<String, T>, StoreError> {
    match tokio::fs::read_to_string(path).await {
        Ok(data) => {
            if data.trim().is_empty() {
                return Ok(HashMap::new());
            }
            serde_json::from_str(&data).map_err(|e| StoreError::Serialization(e.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e.into()),
    }
}

async fn save_json_map<T: serde::Serialize>(
    path: &Path,
    data: &HashMap<String, T>,
) -> Result<(), StoreError> {
    let json =
        serde_json::to_string_pretty(data).map_err(|e| StoreError::Serialization(e.to_string()))?;
    atomic_write(path, json.as_bytes()).await?;
    Ok(())
}

async fn atomic_write(path: &Path, data: &[u8]) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::Io(std::io::Error::other("path has no parent directory")))?;
    tokio::fs::create_dir_all(parent).await?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| StoreError::Io(std::io::Error::other("path has no valid file name")))?;
    let tmp_path = parent.join(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));

    let mut file = tokio::fs::File::create(&tmp_path).await?;
    file.write_all(data).await?;
    file.sync_all().await?;
    drop(file);

    tokio::fs::rename(&tmp_path, path).await?;
    if let Ok(dir) = tokio::fs::File::open(parent).await {
        let _ = dir.sync_all().await;
    }
    Ok(())
}

async fn load_pending_counter(dir: &Path) -> Result<i64, StoreError> {
    let from_counter = match tokio::fs::read_to_string(dir.join(".counter")).await {
        Ok(value) => value.trim().parse::<i64>().unwrap_or(0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };
    Ok(from_counter.max(count_pending_files(dir).await))
}

async fn count_pending_files(dir: &Path) -> i64 {
    let mut max_id = 0i64;
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return 0,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(id) = entry
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<i64>().ok())
        {
            max_id = max_id.max(id);
        }
    }
    max_id
}

async fn load_usage_summary(
    path: &Path,
    platform: &str,
    user_id: &str,
) -> Result<UsageSummary, StoreError> {
    let mut summary = UsageSummary {
        messages: 0,
        tokens_prompt: 0,
        tokens_completion: 0,
        tool_calls: 0,
    };
    let data = match tokio::fs::read_to_string(path).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(summary),
        Err(e) => return Err(e.into()),
    };
    for line in data.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<UsageEntry>(line)
            && entry.platform == platform
            && entry.user_id == user_id
        {
            summary.messages += 1;
            summary.tokens_prompt += entry.tokens_prompt;
            summary.tokens_completion += entry.tokens_completion;
            summary.tool_calls += entry.tool_calls as u64;
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> (tempfile::TempDir, FileGatewayStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileGatewayStore::open(dir.path()).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn user_roundtrip() {
        let (_dir, store) = test_store().await;
        assert!(store.is_first_message("wx", "u1").await.unwrap());
        store.upsert_user("wx", "u1", "Test").await.unwrap();
        assert!(!store.is_first_message("wx", "u1").await.unwrap());
    }

    #[tokio::test]
    async fn preference_roundtrip() {
        let (_dir, store) = test_store().await;
        store.upsert_user("wx", "u1", "Test").await.unwrap();
        store
            .set_user_preference("wx", "u1", "theme", "dark")
            .await
            .unwrap();
        let val = store
            .get_user_preference("wx", "u1", "theme")
            .await
            .unwrap();
        assert_eq!(val.as_deref(), Some("dark"));
    }

    #[tokio::test]
    async fn setting_preference_for_missing_user_fails() {
        let (_dir, store) = test_store().await;
        let err = store
            .set_user_preference("wx", "missing", "theme", "dark")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("user not found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let (_dir, store) = test_store().await;
        assert!(
            store
                .get_current_session("wx", "c1", "astra")
                .await
                .unwrap()
                .is_none()
        );
        store
            .set_current_session("wx", "c1", "u1", "s1", "astra")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_current_session("wx", "c1", "astra")
                .await
                .unwrap()
                .as_deref(),
            Some("s1")
        );
        store.reset_session("wx", "c1", "astra").await.unwrap();
        assert!(
            store
                .get_current_session("wx", "c1", "astra")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn cron_job_lifecycle() {
        let (_dir, store) = test_store().await;
        let spec = CronJobSpec {
            job_id: "j1".into(),
            platform: "wx".into(),
            chat_id: "c1".into(),
            user_id: "u1".into(),
            cron_expr: "0 9 * * *".into(),
            message: "hello".into(),
            description: "greeting".into(),
        };
        store.create_cron_job(&spec).await.unwrap();
        let jobs = store.list_cron_jobs("wx", "c1").await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_id, "j1");
        assert!(store.delete_cron_job("j1").await.unwrap());
        assert!(store.list_cron_jobs("wx", "c1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_message_lifecycle() {
        let (_dir, store) = test_store().await;
        let id = store
            .save_pending_message("wx", "c1", "u1", "hello")
            .await
            .unwrap();
        let msgs = store.list_pending_messages(Some("wx")).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "hello");
        assert_eq!(store.delete_pending_message(id).await.unwrap(), 1);
        assert!(
            store
                .list_pending_messages(Some("wx"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn credential_roundtrip() {
        let (_dir, store) = test_store().await;
        let creds = serde_json::json!({"token": "abc"});
        store
            .save_credential("wx", "u1", "bot_token", &creds, None)
            .await
            .unwrap();
        let got = store
            .get_credential("wx", "u1", "bot_token")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.credentials["token"], "abc");
        assert!(
            store
                .delete_credential("wx", "u1", "bot_token")
                .await
                .unwrap()
        );
    }

    // ── GAP 3: persistence roundtrip ──────────────────────────────

    #[tokio::test]
    async fn persistence_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();

        // Write
        {
            let store = FileGatewayStore::open(dir.path()).await.unwrap();
            store.upsert_user("wx", "u1", "Alice").await.unwrap();
            store
                .set_user_preference("wx", "u1", "theme", "dark")
                .await
                .unwrap();
            store
                .set_current_session("wx", "c1", "u1", "s1", "astra")
                .await
                .unwrap();
            store
                .create_cron_job(&CronJobSpec {
                    job_id: "j1".into(),
                    platform: "wx".into(),
                    chat_id: "c1".into(),
                    user_id: "u1".into(),
                    cron_expr: "0 9 * * *".into(),
                    message: "hello".into(),
                    description: "daily".into(),
                })
                .await
                .unwrap();
        }
        // Store dropped -- all in-memory state gone

        // Reopen
        {
            let store = FileGatewayStore::open(dir.path()).await.unwrap();
            assert!(
                !store.is_first_message("wx", "u1").await.unwrap(),
                "user should persist"
            );
            assert_eq!(
                store
                    .get_user_preference("wx", "u1", "theme")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("dark"),
                "preference should persist"
            );
            assert_eq!(
                store
                    .get_current_session("wx", "c1", "astra")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("s1"),
                "session should persist"
            );
            let jobs = store.list_cron_jobs("wx", "c1").await.unwrap();
            assert_eq!(jobs.len(), 1, "cron job should persist");
            assert_eq!(jobs[0].job_id, "j1");
        }
    }

    #[tokio::test]
    async fn credentials_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = FileGatewayStore::open(dir.path()).await.unwrap();
            store
                .save_credential("wx", "u1", "token", &serde_json::json!({"k": "v"}), None)
                .await
                .unwrap();
        }
        {
            let store = FileGatewayStore::open(dir.path()).await.unwrap();
            let cred = store
                .get_credential("wx", "u1", "token")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(cred.credentials["k"], "v");
        }
    }

    #[tokio::test]
    async fn pending_messages_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let msg_id;
        {
            let store = FileGatewayStore::open(dir.path()).await.unwrap();
            msg_id = store
                .save_pending_message("wx", "c1", "u1", "persist me")
                .await
                .unwrap();
        }
        {
            let store = FileGatewayStore::open(dir.path()).await.unwrap();
            let msgs = store.list_pending_messages(Some("wx")).await.unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].text, "persist me");
            assert_eq!(msgs[0].id, msg_id);
        }
    }

    #[tokio::test]
    async fn pending_message_ids_remain_monotonic_after_reopen_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let second_id;
        {
            let store = FileGatewayStore::open(dir.path()).await.unwrap();
            let first_id = store
                .save_pending_message("wx", "c1", "u1", "first")
                .await
                .unwrap();
            second_id = store
                .save_pending_message("wx", "c1", "u1", "second")
                .await
                .unwrap();
            assert!(second_id > first_id);
            assert_eq!(store.delete_pending_message(second_id).await.unwrap(), 1);
        }

        let store = FileGatewayStore::open(dir.path()).await.unwrap();
        let next_id = store
            .save_pending_message("wx", "c1", "u1", "third")
            .await
            .unwrap();
        assert!(
            next_id > second_id,
            "pending ids must be monotonic across restarts even after deleting the highest file"
        );
    }

    #[tokio::test]
    async fn usage_recording() {
        let (_dir, store) = test_store().await;
        let record = UsageRecord {
            platform: "wx".into(),
            user_id: "u1".into(),
            cli_profile: "astra".into(),
            model: None,
            tokens_prompt: 100,
            tokens_completion: 50,
            tool_calls: 2,
            elapsed_ms: 3000,
        };
        store.record_usage(&record).await.unwrap();
        let today = store.get_usage_today("wx", "u1").await.unwrap();
        assert_eq!(today.messages, 1);
        assert_eq!(today.tokens_prompt, 100);
        let total = store.get_usage_total("wx", "u1").await.unwrap();
        assert_eq!(total.messages, 1);
    }
}
