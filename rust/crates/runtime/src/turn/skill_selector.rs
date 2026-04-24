use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, LazyLock, OnceLock, RwLock};
use std::time::Instant;

use astra_core::SkillSearchSettings;
use astra_skills::traits::SkillToolInfo;
use astra_turn_core::tool_registry_state::word_boundary_match;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::skills::quality::SkillQualityTracker;
use crate::text_tokenize::tokenize;

const EMBEDDING_POOL: usize = 150;
const BUNDLED_SOURCE_BONUS: f64 = 0.10;
const EMBEDDING_BATCH_SIZE: usize = 32;

static STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "a",
        "an",
        "and",
        "are",
        "at",
        "by",
        "do",
        "does",
        "for",
        "from",
        "help",
        "how",
        "in",
        "is",
        "me",
        "my",
        "need",
        "of",
        "on",
        "or",
        "our",
        "please",
        "the",
        "to",
        "use",
        "using",
        "want",
        "we",
        "what",
        "with",
        "you",
        "your",
        "一下",
        "一个",
        "一些",
        "不一定",
        "为",
        "了",
        "从",
        "你",
        "做",
        "到",
        "前",
        "后",
        "和",
        "在",
        "如何",
        "将",
        "帮",
        "我们",
        "我",
        "把",
        "是",
        "有",
        "用",
        "给",
        "请",
        "这",
        "这个",
        "进行",
        "那个",
        "那",
        "需要",
    ])
});

static CANONICAL_GROUPS: &[(&str, &[&str])] = &[
    (
        "accessibility",
        &["无障碍", "视障", "屏幕阅读器", "a11y", "accessibility"],
    ),
    (
        "ads",
        &[
            "广告",
            "营销",
            "投放",
            "campaign",
            "ads",
            "meta",
            "googleads",
        ],
    ),
    ("api", &["接口", "api", "endpoint"]),
    (
        "auth",
        &[
            "认证",
            "鉴权",
            "授权",
            "登录",
            "auth",
            "authentication",
            "authorize",
            "login",
            "oauth",
            "jwt",
        ],
    ),
    (
        "billing",
        &["账单", "发票", "billing", "invoice", "payment", "支付"],
    ),
    (
        "build",
        &["构建", "编译", "打包", "build", "compile", "package"],
    ),
    (
        "ci",
        &[
            "持续集成",
            "持续部署",
            "流水线",
            "workflow",
            "pipeline",
            "ci",
            "cd",
        ],
    ),
    ("crm", &["客户", "线索", "crm", "lead", "sales"]),
    ("csv", &["csv", "表格", "excel", "xlsx"]),
    (
        "db",
        &[
            "数据库",
            "数仓",
            "db",
            "database",
            "sql",
            "mysql",
            "postgres",
            "postgresql",
        ],
    ),
    (
        "debug",
        &[
            "排查",
            "调试",
            "诊断",
            "debug",
            "troubleshoot",
            "investigate",
        ],
    ),
    (
        "deploy",
        &[
            "部署",
            "发布",
            "上线",
            "发版",
            "deploy",
            "deployment",
            "release",
            "ship",
        ],
    ),
    ("docker", &["容器", "docker"]),
    ("email", &["邮件", "邮箱", "email", "mail"]),
    ("embedding", &["向量", "embedding", "embeddings"]),
    ("file", &["文件", "文档", "pdf", "word", "ppt"]),
    (
        "finance",
        &["金融", "合规", "风险", "fintech", "compliance", "risk"],
    ),
    (
        "git",
        &[
            "代码库",
            "仓库",
            "git",
            "github",
            "repo",
            "repository",
            "pr",
        ],
    ),
    ("http", &["请求", "响应", "http", "https", "rest"]),
    ("image", &["图片", "图像", "ocr", "vision", "image"]),
    ("k8s", &["k8s", "kubernetes"]),
    (
        "linux",
        &[
            "服务器",
            "终端",
            "命令行",
            "linux",
            "shell",
            "bash",
            "ssh",
            "server",
        ],
    ),
    ("log", &["日志", "log", "logging", "trace"]),
    (
        "monitor",
        &["监控", "告警", "metrics", "monitor", "monitoring", "alert"],
    ),
    ("node", &["node", "nodejs", "npm", "pnpm", "yarn"]),
    ("python", &["python", "py"]),
    ("redis", &["缓存", "cache", "redis"]),
    ("rollback", &["回滚", "撤回", "rollback", "revert"]),
    ("rust", &["rust"]),
    ("search", &["搜索", "检索", "search", "find"]),
    (
        "security",
        &[
            "安全", "漏洞", "攻击", "防护", "security", "xss", "csrf", "sqli",
        ],
    ),
    ("skill", &["技能", "模块", "插件", "tool", "tools", "skill"]),
    ("test", &["测试", "验证", "test", "testing", "qa"]),
    ("vue", &["vue", "vue3"]),
    (
        "web",
        &["网页", "网站", "浏览器", "web", "html", "css", "seo"],
    ),
];

static CANONICAL_MAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for (canonical, variants) in CANONICAL_GROUPS {
        for variant in *variants {
            map.insert(*variant, *canonical);
        }
    }
    map
});

static SELECTOR_CACHE: OnceLock<RwLock<HashMap<String, Arc<SelectorCatalogIndex>>>> =
    OnceLock::new();

#[derive(Clone, Debug)]
struct SelectorEntry {
    skill: SkillToolInfo,
    name_exact: String,
    alias_exact: Vec<String>,
    trigger_exact: Vec<String>,
    name_tokens: Vec<String>,
    alias_tokens: Vec<String>,
    trigger_tokens: Vec<String>,
    tag_tokens: Vec<String>,
    desc_tokens: Vec<String>,
    embed_doc: String,
}

#[derive(Debug)]
struct SelectorCatalogIndex {
    entries: Vec<SelectorEntry>,
    embeddings: RwLock<Option<Vec<Vec<f32>>>>,
}

#[derive(Clone, Debug, Default)]
struct ExactSignals {
    name_hit: bool,
    alias_hit: bool,
    trigger_hit: bool,
}

#[derive(Clone, Debug)]
struct LexicalCandidate {
    idx: usize,
    score: f64,
    exact: ExactSignals,
}

#[derive(Clone, Debug)]
struct FinalCandidate {
    idx: usize,
    final_score: f64,
    #[allow(dead_code)]
    lexical_score: f64,
    exact: ExactSignals,
}

#[derive(Clone, Debug)]
struct EmbeddingServiceConfig {
    base_url: String,
    api_key: String,
    model: String,
}

#[derive(Clone, Debug)]
struct RerankServiceConfig {
    base_url: String,
    api_key: String,
    model: String,
}

#[derive(Clone, Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Clone, Debug, Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

fn selector_cache() -> &'static RwLock<HashMap<String, Arc<SelectorCatalogIndex>>> {
    SELECTOR_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn selector_http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new())
    })
}

fn normalize_phrase(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_token(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let canonical = CANONICAL_MAP.get(token).copied().unwrap_or(token);
    if STOPWORDS.contains(canonical) {
        return None;
    }
    if canonical.is_ascii() && canonical.len() < 2 {
        return None;
    }
    if !canonical.is_ascii() && canonical.chars().count() < 2 && !CANONICAL_MAP.contains_key(token)
    {
        return None;
    }
    Some(canonical.to_string())
}

fn canonical_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for token in tokenize(text) {
        if let Some(norm) = normalize_token(&token)
            && seen.insert(norm.clone())
        {
            out.push(norm);
        }
    }
    out
}

fn skill_embed_doc(skill: &SkillToolInfo) -> String {
    let mut parts = vec![
        format!("name: {}", skill.name),
        format!("description: {}", skill.description),
    ];
    if let Some(when) = &skill.when_to_use {
        parts.push(format!("when_to_use: {when}"));
    }
    if let Some(category) = &skill.category {
        parts.push(format!("category: {category}"));
    }
    if !skill.aliases.is_empty() {
        parts.push(format!("aliases: {}", skill.aliases.join(", ")));
    }
    if !skill.tags.is_empty() {
        parts.push(format!("tags: {}", skill.tags.join(", ")));
    }
    if !skill.triggers.is_empty() {
        parts.push(format!("triggers: {}", skill.triggers.join(", ")));
    }
    parts.join("\n")
}

fn build_selector_entry(skill: &SkillToolInfo) -> SelectorEntry {
    let desc_text = format!(
        "{} {} {}",
        skill.description,
        skill.when_to_use.as_deref().unwrap_or_default(),
        skill.category.as_deref().unwrap_or_default()
    );
    SelectorEntry {
        skill: skill.clone(),
        name_exact: normalize_phrase(&skill.name),
        alias_exact: skill.aliases.iter().map(|s| normalize_phrase(s)).collect(),
        trigger_exact: skill.triggers.iter().map(|s| normalize_phrase(s)).collect(),
        name_tokens: canonical_tokens(&skill.name),
        alias_tokens: canonical_tokens(&skill.aliases.join(" ")),
        trigger_tokens: canonical_tokens(&skill.triggers.join(" ")),
        tag_tokens: canonical_tokens(&format!(
            "{} {}",
            skill.tags.join(" "),
            skill.category.as_deref().unwrap_or_default()
        )),
        desc_tokens: canonical_tokens(&desc_text),
        embed_doc: skill_embed_doc(skill),
    }
}

fn catalog_key(skills: &[SkillToolInfo]) -> String {
    let mut hasher = Sha256::new();
    for skill in skills {
        hasher.update(skill.name.as_bytes());
        hasher.update([0]);
        hasher.update(skill.description.as_bytes());
        hasher.update([0]);
        hasher.update(skill.when_to_use.as_deref().unwrap_or_default().as_bytes());
        hasher.update([0]);
        hasher.update(skill.category.as_deref().unwrap_or_default().as_bytes());
        hasher.update([0]);
        for alias in &skill.aliases {
            hasher.update(alias.as_bytes());
            hasher.update([0]);
        }
        for tag in &skill.tags {
            hasher.update(tag.as_bytes());
            hasher.update([0]);
        }
        for trigger in &skill.triggers {
            hasher.update(trigger.as_bytes());
            hasher.update([0]);
        }
    }
    format!("{:x}", hasher.finalize())
}

fn catalog_index(skills: &[SkillToolInfo]) -> Arc<SelectorCatalogIndex> {
    let key = catalog_key(skills);
    if let Ok(cache) = selector_cache().read()
        && let Some(existing) = cache.get(&key)
    {
        return existing.clone();
    }
    let built = Arc::new(SelectorCatalogIndex {
        entries: skills.iter().map(build_selector_entry).collect(),
        embeddings: RwLock::new(None),
    });
    if let Ok(mut cache) = selector_cache().write() {
        cache.insert(key, built.clone());
    }
    built
}

fn overlap_count(query_tokens: &HashSet<String>, field_tokens: &[String]) -> usize {
    field_tokens
        .iter()
        .filter(|token| query_tokens.contains(token.as_str()))
        .count()
}

fn exact_signals(query_lower: &str, query_chars: &[char], entry: &SelectorEntry) -> ExactSignals {
    let name_hit = !entry.name_exact.is_empty()
        && word_boundary_match(query_lower, query_chars, &entry.name_exact);
    let alias_hit = entry
        .alias_exact
        .iter()
        .any(|alias| !alias.is_empty() && word_boundary_match(query_lower, query_chars, alias));
    let trigger_hit = entry.trigger_exact.iter().any(|trigger| {
        !trigger.is_empty() && word_boundary_match(query_lower, query_chars, trigger)
    });
    ExactSignals {
        name_hit,
        alias_hit,
        trigger_hit,
    }
}

fn lexical_score(
    query_tokens: &[String],
    entry: &SelectorEntry,
    quality_tracker: Option<&SkillQualityTracker>,
    exact: &ExactSignals,
) -> f64 {
    let query_set: HashSet<String> = query_tokens.iter().cloned().collect();
    let name_overlap = overlap_count(&query_set, &entry.name_tokens);
    let alias_overlap = overlap_count(&query_set, &entry.alias_tokens);
    let trigger_overlap = overlap_count(&query_set, &entry.trigger_tokens);
    let tag_overlap = overlap_count(&query_set, &entry.tag_tokens);
    let desc_overlap = overlap_count(&query_set, &entry.desc_tokens);
    let mut matched_any = HashSet::new();
    for token in entry
        .name_tokens
        .iter()
        .chain(entry.alias_tokens.iter())
        .chain(entry.trigger_tokens.iter())
        .chain(entry.tag_tokens.iter())
        .chain(entry.desc_tokens.iter())
    {
        if query_set.contains(token) {
            matched_any.insert(token.as_str());
        }
    }

    let mut score = 0.0;
    if exact.name_hit {
        score += 30.0;
    }
    if exact.alias_hit {
        score += 24.0;
    }
    if exact.trigger_hit {
        score += 18.0;
    }
    score += 10.0 * name_overlap as f64;
    score += 8.0 * alias_overlap as f64;
    score += 6.0 * trigger_overlap as f64;
    score += 3.0 * tag_overlap as f64;
    score += 2.0 * desc_overlap as f64;
    score += 2.0 * matched_any.len() as f64;
    if matches!(
        entry.skill.source,
        crate::skills::manifest::SkillSourceKind::Bundled
    ) {
        score += BUNDLED_SOURCE_BONUS;
    }
    if let Some(tracker) = quality_tracker {
        score += tracker.selection_boost(&entry.skill.name) * 0.5;
    }
    score
}

fn lexical_candidates(
    skills: &[SkillToolInfo],
    query: &str,
    quality_tracker: Option<&SkillQualityTracker>,
) -> Vec<LexicalCandidate> {
    let index = catalog_index(skills);
    let query_lower = query.trim().to_lowercase();
    let query_chars: Vec<char> = query_lower.chars().collect();
    let query_tokens = canonical_tokens(query);
    let mut out = index
        .entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let exact = exact_signals(&query_lower, &query_chars, entry);
            LexicalCandidate {
                idx,
                score: lexical_score(&query_tokens, entry, quality_tracker, &exact),
                exact,
            }
        })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.idx.cmp(&b.idx))
    });
    out
}

fn select_base_top_k(skill_count: usize, surface_cap: usize) -> usize {
    let cap = surface_cap.clamp(5, 20);
    let base = (skill_count * 2).div_ceil(100);
    base.clamp(5, cap)
}

fn choose_top_k(skill_count: usize, surface_cap: usize, ranked: &[FinalCandidate]) -> usize {
    let cap = surface_cap.clamp(5, 20);
    let base = select_base_top_k(skill_count, surface_cap);
    let exact_unique = ranked.first().is_some_and(|top| {
        let exact_count = ranked
            .iter()
            .filter(|c| c.exact.name_hit || c.exact.alias_hit || c.exact.trigger_hit)
            .take(2)
            .count();
        exact_count == 1 && (top.exact.name_hit || top.exact.alias_hit || top.exact.trigger_hit)
    });
    if exact_unique {
        return 5.min(cap);
    }
    if ranked.len() > base {
        let top = ranked
            .first()
            .map(|c| c.final_score)
            .unwrap_or(0.0)
            .max(1e-6);
        let boundary_gap = (ranked[base - 1].final_score - ranked[base].final_score) / top;
        if boundary_gap < 0.05 {
            return cap;
        }
    }
    base
}

fn l2_normalize(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm as f32;
        }
    }
    vector
}

fn dot_similarity(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum::<f64>()
}

fn embed_service_from_env() -> Option<EmbeddingServiceConfig> {
    // Reuse the shared Astra embedding configuration (MEMORIA_EMBEDDING_*).
    let base_url = std::env::var("MEMORIA_EMBEDDING_BASE_URL").ok()?;
    let api_key = std::env::var("MEMORIA_EMBEDDING_API_KEY").ok()?;
    let model = std::env::var("MEMORIA_EMBEDDING_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "bge-m3".to_string());
    if base_url.trim().is_empty() || api_key.trim().is_empty() {
        return None;
    }
    Some(EmbeddingServiceConfig {
        base_url,
        api_key,
        model,
    })
}

fn rerank_service_from_env() -> Option<RerankServiceConfig> {
    let base_url = std::env::var("ASTRA_SKILL_SELECTOR_RERANK_BASE_URL").ok()?;
    let api_key = std::env::var("ASTRA_SKILL_SELECTOR_RERANK_API_KEY").ok()?;
    let model = std::env::var("ASTRA_SKILL_SELECTOR_RERANK_MODEL").ok()?;
    if base_url.trim().is_empty() || api_key.trim().is_empty() || model.trim().is_empty() {
        return None;
    }
    Some(RerankServiceConfig {
        base_url,
        api_key,
        model,
    })
}

fn embeddings_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/embeddings") {
        base.to_string()
    } else {
        format!("{base}/embeddings")
    }
}

fn chat_completions_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

fn block_on_future<F, T>(future: F) -> Result<T, String>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| "selector online path requires a Tokio runtime".to_string())?;
    match handle.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        _ => std::thread::scope(|scope| {
            scope
                .spawn(move || handle.block_on(future))
                .join()
                .map_err(|_| "selector online worker panicked".to_string())?
        }),
    }
}

async fn request_embeddings(
    config: EmbeddingServiceConfig,
    inputs: Vec<String>,
) -> Result<Vec<Vec<f32>>, String> {
    let response = selector_http_client()
        .post(embeddings_url(&config.base_url))
        .bearer_auth(config.api_key)
        .json(&json!({
            "model": config.model,
            "input": inputs,
        }))
        .send()
        .await
        .map_err(|e| format!("embedding request failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<body unavailable>".to_string());
        return Err(format!("embedding request returned {status}: {body}"));
    }
    let mut parsed: EmbeddingResponse = response
        .json()
        .await
        .map_err(|e| format!("embedding response decode failed: {e}"))?;
    parsed.data.sort_by_key(|item| item.index);
    Ok(parsed
        .data
        .into_iter()
        .map(|item| l2_normalize(item.embedding))
        .collect())
}

fn ensure_skill_embeddings(
    index: &SelectorCatalogIndex,
    config: &EmbeddingServiceConfig,
) -> Result<Vec<Vec<f32>>, String> {
    if let Ok(read) = index.embeddings.read()
        && let Some(cached) = read.as_ref()
    {
        return Ok(cached.clone());
    }
    let inputs = index
        .entries
        .iter()
        .map(|entry| entry.embed_doc.clone())
        .collect::<Vec<_>>();
    let mut all = Vec::new();
    for chunk in inputs.chunks(EMBEDDING_BATCH_SIZE) {
        let batch = block_on_future(request_embeddings(config.clone(), chunk.to_vec()))?;
        all.extend(batch);
    }
    if let Ok(mut write) = index.embeddings.write() {
        *write = Some(all.clone());
    }
    Ok(all)
}

fn embedding_rank_map(
    skills: &[SkillToolInfo],
    query: &str,
) -> Result<HashMap<usize, usize>, String> {
    let Some(config) = embed_service_from_env() else {
        return Ok(HashMap::new());
    };
    let started = Instant::now();
    let index = catalog_index(skills);
    let skill_embeddings = ensure_skill_embeddings(&index, &config)?;
    let mut query_vecs = block_on_future(request_embeddings(config, vec![query.to_string()]))?;
    let query_vec = query_vecs
        .pop()
        .ok_or_else(|| "embedding query returned no vectors".to_string())?;
    let mut ranked = skill_embeddings
        .iter()
        .enumerate()
        .map(|(idx, vec)| (idx, dot_similarity(&query_vec, vec)))
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let map: HashMap<usize, usize> = ranked
        .into_iter()
        .take(EMBEDDING_POOL)
        .enumerate()
        .map(|(rank, (idx, _))| (idx, rank + 1))
        .collect();
    debug!(
        target: "astra::skill_selector",
        catalog_size = skills.len(),
        pool_size = map.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "skill_selector embedding rank built",
    );
    Ok(map)
}

fn rerank_candidates(
    query: &str,
    skills: &[SkillToolInfo],
    candidates: &[FinalCandidate],
    top_k: usize,
) -> Result<Vec<usize>, String> {
    let Some(config) = rerank_service_from_env() else {
        return Ok(candidates.iter().take(top_k).map(|c| c.idx).collect());
    };
    let started = Instant::now();
    let model_label = config.model.clone();
    // Use the full embedding-recall pool as rerank input; the cheap LLM compresses it to top_k.
    let pool = candidates.iter().take(EMBEDDING_POOL).collect::<Vec<_>>();
    let candidate_text = pool
        .iter()
        .enumerate()
        .map(|(rank, cand)| {
            let skill = &skills[cand.idx];
            let mut desc = match &skill.when_to_use {
                Some(when) => format!("{} (use when: {})", skill.description, when),
                None => skill.description.clone(),
            };
            if !skill.aliases.is_empty() {
                desc.push_str(&format!(" [aliases: {}]", skill.aliases.join(", ")));
            }
            format!("{}. {}: {}", rank + 1, skill.name, desc)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let payload = json!({
        "model": config.model,
        "messages": [
            {
                "role": "system",
                "content": "You are a cheap skill selector reranker. Return ONLY JSON. Rank the most likely matching skills best-first. Do not invent candidates."
            },
            {
                "role": "user",
                "content": format!(
                    "User request:\n{query}\n\nCandidate skills:\n{candidate_text}\n\nReturn JSON exactly like {{\"ranked_candidate_numbers\":[1,2,3]}}.\nRules:\n- Use only candidate numbers shown above.\n- Return exactly {} unique integers.\n- Sort best-first.\n- Prefer recall over precision.\n",
                    top_k.min(pool.len())
                )
            }
        ],
        "temperature": 0,
        "max_tokens": 256,
        // DashScope/Qwen-specific knob to disable chain-of-thought; ignored by other OpenAI-compatible providers.
        "enable_thinking": false,
    });
    let body = match block_on_future(async move {
        let response = selector_http_client()
            .post(chat_completions_url(&config.base_url))
            .bearer_auth(config.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("selector rerank request failed: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .await
                .unwrap_or_else(|_| "<body unavailable>".to_string());
            return Err(format!("selector rerank returned {status}: {text}"));
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("selector rerank decode failed: {e}"))?;
        value["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "selector rerank response missing message content".to_string())
    }) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                target: "astra::skill_selector",
                model = %model_label,
                pool_size = pool.len(),
                top_k,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = %e,
                "skill_selector rerank failed; falling back to embedding order",
            );
            return Ok(candidates.iter().take(top_k).map(|c| c.idx).collect());
        }
    };
    let parsed = serde_json::from_str::<serde_json::Value>(body.trim())
        .ok()
        .and_then(|value| value["ranked_candidate_numbers"].as_array().cloned())
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut reranked = Vec::new();
    for item in parsed {
        if let Some(num) = item.as_u64() {
            let idx = num as usize;
            if idx == 0 || idx > pool.len() || !seen.insert(idx) {
                continue;
            }
            reranked.push(pool[idx - 1].idx);
            if reranked.len() >= top_k {
                break;
            }
        }
    }
    if reranked.is_empty() {
        warn!(
            target: "astra::skill_selector",
            model = %model_label,
            pool_size = pool.len(),
            top_k,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "skill_selector rerank returned empty ranking; falling back to embedding order",
        );
        return Ok(candidates.iter().take(top_k).map(|c| c.idx).collect());
    }
    debug!(
        target: "astra::skill_selector",
        model = %model_label,
        pool_size = pool.len(),
        returned = reranked.len(),
        top_k,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "skill_selector rerank ok",
    );
    Ok(reranked)
}

pub fn select_skill_indices(
    all_skills: &[SkillToolInfo],
    user_message: &str,
    quality_tracker: Option<&SkillQualityTracker>,
    cfg: &SkillSearchSettings,
) -> Vec<usize> {
    let started = Instant::now();
    let lexical = lexical_candidates(all_skills, user_message, quality_tracker);
    if lexical.is_empty() {
        debug!(
            target: "astra::skill_selector",
            catalog_size = all_skills.len(),
            "skill_selector lexical produced no candidates; returning empty",
        );
        return Vec::new();
    }
    let embedding_ranks = embedding_rank_map(all_skills, user_message).unwrap_or_else(|e| {
        warn!(
            target: "astra::skill_selector",
            error = %e,
            "skill_selector embedding ranking failed; falling back to lexical",
        );
        HashMap::new()
    });
    let rerank_enabled = rerank_service_from_env().is_some();
    let tier = if embedding_ranks.is_empty() {
        "lexical"
    } else if rerank_enabled {
        "embedding+rerank"
    } else {
        "embedding"
    };
    let ranked = if embedding_ranks.is_empty() {
        // Tier 1: no embedding service available — fall back to pure lexical ranking.
        lexical
            .iter()
            .map(|cand| FinalCandidate {
                idx: cand.idx,
                final_score: cand.score,
                lexical_score: cand.score,
                exact: cand.exact.clone(),
            })
            .collect::<Vec<_>>()
    } else {
        // Tier 2/3: embedding available — rank purely by embedding similarity.
        // If the cheap-LLM reranker is configured, it will further compress this pool below.
        let mut pool = embedding_only_candidates(&embedding_ranks);
        // Apply learned quality boost so embedding/rerank tiers honor SkillQualityTracker
        // (lexical tier already absorbs the boost inside lexical_score).
        apply_quality_boost(all_skills, &mut pool, quality_tracker);
        pool
    };
    let top_k = choose_top_k(all_skills.len(), cfg.effective_surface_cap(), &ranked);
    let result = rerank_candidates(user_message, all_skills, &ranked, top_k)
        .unwrap_or_else(|_| ranked.iter().take(top_k).map(|c| c.idx).collect());
    debug!(
        target: "astra::skill_selector",
        tier,
        catalog_size = all_skills.len(),
        ranked_pool = ranked.len(),
        returned = result.len(),
        top_k,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "skill_selector select_skill_indices done",
    );
    result
}

fn embedding_only_candidates(embedding_ranks: &HashMap<usize, usize>) -> Vec<FinalCandidate> {
    let mut by_rank: Vec<(usize, usize)> = embedding_ranks
        .iter()
        .map(|(idx, rank)| (*idx, *rank))
        .collect();
    by_rank.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    by_rank
        .into_iter()
        .map(|(idx, rank)| FinalCandidate {
            idx,
            final_score: 1.0 / (rank as f64 + 1.0),
            lexical_score: 0.0,
            exact: Default::default(),
        })
        .collect()
}

/// Apply per-skill quality boost to a ranked candidate pool and re-sort.
///
/// `selection_boost` returns [0.5, 1.5] (1.0 = neutral, <3 invocations = neutral),
/// so this is an effective no-op for cold-start skills while letting learned
/// success/failure history reshape the embedding/rerank tier orderings — closing
/// the regression where the embedding path silently bypassed quality tracking.
fn apply_quality_boost(
    catalog: &[SkillToolInfo],
    candidates: &mut [FinalCandidate],
    quality_tracker: Option<&SkillQualityTracker>,
) {
    let Some(tracker) = quality_tracker else {
        return;
    };
    let mut adjusted = false;
    for cand in candidates.iter_mut() {
        let boost = tracker.selection_boost(&catalog[cand.idx].name);
        if (boost - 1.0).abs() > f64::EPSILON {
            cand.final_score *= boost;
            adjusted = true;
        }
    }
    if adjusted {
        candidates.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.idx.cmp(&b.idx))
        });
    }
}

pub fn discover_skill_indices(
    catalog: &[SkillToolInfo],
    query: &str,
    excluded_lowercase: &HashSet<String>,
    quality_tracker: Option<&SkillQualityTracker>,
    limit: usize,
) -> Vec<usize> {
    let started = Instant::now();
    let lexical = lexical_candidates(catalog, query, quality_tracker);
    let embedding_ranks = embedding_rank_map(catalog, query).unwrap_or_else(|e| {
        warn!(
            target: "astra::skill_selector",
            error = %e,
            "skill_selector discover embedding ranking failed; falling back to lexical",
        );
        HashMap::new()
    });
    let tier = if embedding_ranks.is_empty() {
        "lexical"
    } else {
        "embedding"
    };
    let ranked = if embedding_ranks.is_empty() {
        lexical
            .into_iter()
            .map(|cand| FinalCandidate {
                idx: cand.idx,
                final_score: cand.score,
                lexical_score: cand.score,
                exact: cand.exact,
            })
            .collect::<Vec<_>>()
    } else {
        let mut pool = embedding_only_candidates(&embedding_ranks);
        apply_quality_boost(catalog, &mut pool, quality_tracker);
        pool
    };
    let result: Vec<usize> = ranked
        .into_iter()
        .filter(|cand| {
            let skill = &catalog[cand.idx];
            !excluded_lowercase.contains(&skill.name.to_lowercase())
                && !skill
                    .aliases
                    .iter()
                    .any(|alias| excluded_lowercase.contains(&alias.to_lowercase()))
        })
        .filter(|cand| cand.final_score > 0.0)
        .take(limit)
        .map(|cand| cand.idx)
        .collect();
    debug!(
        target: "astra::skill_selector",
        tier,
        catalog_size = catalog.len(),
        returned = result.len(),
        limit,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "skill_selector discover_skill_indices done",
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::manifest::SkillSourceKind;

    fn skill(name: &str, description: &str) -> SkillToolInfo {
        SkillToolInfo {
            name: name.into(),
            description: description.into(),
            source: SkillSourceKind::Local,
            ..Default::default()
        }
    }

    #[test]
    fn canonical_tokens_bridge_cn_en() {
        let tokens = canonical_tokens("帮我部署并查看日志");
        assert!(tokens.iter().any(|t| t == "deploy"));
        assert!(tokens.iter().any(|t| t == "log"));
    }

    #[test]
    fn lexical_exact_hit_beats_loose_overlap() {
        let skills = vec![
            SkillToolInfo {
                name: "deploy".into(),
                description: "Deploy workloads".into(),
                aliases: vec!["ship-it".into()],
                source: SkillSourceKind::Local,
                ..Default::default()
            },
            skill("debug", "Debug issues"),
        ];
        let ranked = select_skill_indices(
            &skills,
            "请帮我 deploy 一下",
            None,
            &SkillSearchSettings::default(),
        );
        assert_eq!(ranked.first().copied(), Some(0));
    }

    #[test]
    fn choose_top_k_prefers_small_exact_hit() {
        let ranked = vec![
            FinalCandidate {
                idx: 0,
                final_score: 1.0,
                lexical_score: 30.0,
                exact: ExactSignals {
                    name_hit: true,
                    alias_hit: false,
                    trigger_hit: false,
                },
            },
            FinalCandidate {
                idx: 1,
                final_score: 0.2,
                lexical_score: 2.0,
                exact: ExactSignals::default(),
            },
        ];
        assert_eq!(choose_top_k(1000, 20, &ranked), 5);
    }

    #[test]
    fn discover_filters_excluded_aliases() {
        let catalog = vec![
            SkillToolInfo {
                name: "deploy".into(),
                description: "Deploy apps".into(),
                aliases: vec!["ship-it".into()],
                source: SkillSourceKind::Local,
                ..Default::default()
            },
            SkillToolInfo {
                name: "debug".into(),
                description: "Debug issues".into(),
                source: SkillSourceKind::Local,
                ..Default::default()
            },
        ];
        let mut excluded = HashSet::new();
        excluded.insert("ship-it".to_string());
        let found = discover_skill_indices(&catalog, "debug issue", &excluded, None, 8);
        assert_eq!(found, vec![1]);
    }

    #[test]
    fn apply_quality_boost_reorders_embedding_pool() {
        use crate::skills::quality::{SkillOutcome, SkillQualityTracker};
        let catalog = vec![skill("alpha", "first"), skill("beta", "second")];
        let mut tracker = SkillQualityTracker::new();
        // 5 failures for alpha → boost < 1.0; 5 successes for beta → boost > 1.0.
        for _ in 0..5 {
            tracker.record_outcome(&SkillOutcome {
                skill_name: "alpha".into(),
                tokens_used: 0,
                duration_ms: 0,
                all_required_passed: false,
                partial: false,
            });
            tracker.record_outcome(&SkillOutcome {
                skill_name: "beta".into(),
                tokens_used: 0,
                duration_ms: 0,
                all_required_passed: true,
                partial: false,
            });
        }
        // Simulate embedding tier: alpha ranked first, beta second, similar scores.
        let mut pool = vec![
            FinalCandidate {
                idx: 0,
                final_score: 1.0,
                lexical_score: 0.0,
                exact: Default::default(),
            },
            FinalCandidate {
                idx: 1,
                final_score: 0.95,
                lexical_score: 0.0,
                exact: Default::default(),
            },
        ];
        apply_quality_boost(&catalog, &mut pool, Some(&tracker));
        // beta's quality boost (>1.0) should overcome alpha's small lead (and alpha is penalized <1.0).
        assert_eq!(pool[0].idx, 1, "high-quality beta should be reranked first");
        assert_eq!(pool[1].idx, 0);
    }

    #[test]
    fn apply_quality_boost_no_tracker_is_noop() {
        let catalog = vec![skill("a", "x"), skill("b", "y")];
        let mut pool = vec![
            FinalCandidate {
                idx: 0,
                final_score: 1.0,
                lexical_score: 0.0,
                exact: Default::default(),
            },
            FinalCandidate {
                idx: 1,
                final_score: 0.5,
                lexical_score: 0.0,
                exact: Default::default(),
            },
        ];
        apply_quality_boost(&catalog, &mut pool, None);
        assert_eq!(pool[0].idx, 0);
        assert_eq!(pool[1].idx, 1);
    }
}
