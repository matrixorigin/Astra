# Shared Context Cache

Cross-agent knowledge sharing system for Mo-Agent. Enables multiple agents in the same session to share file reads, discoveries, and findings.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                     Shared Context Cache                        │
│                                                                 │
│  ┌───────────────┐  ┌───────────────┐  ┌───────────────────┐   │
│  │  File Cache   │  │   Knowledge   │  │  Agent Findings   │   │
│  │  (DashMap)    │  │    Store      │  │     Store         │   │
│  │               │  │   (DashMap)   │  │    (DashMap)      │   │
│  │ path→content  │  │  key→value    │  │  agent_id→result  │   │
│  │ +mtime+TTL    │  │  +metadata    │  │  +findings        │   │
│  └───────────────┘  └───────────────┘  └───────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
                              ▲
          ┌───────────────────┼───────────────────┐
          │                   │                   │
     ┌────┴────┐         ┌────┴────┐         ┌────┴────┐
     │ Agent 1 │         │ Agent 2 │         │ Agent 3 │
     │ explore │         │ review  │         │  task   │
     └─────────┘         └─────────┘         └─────────┘
```

## Three Tiers of Caching

### 1. File Content Cache

Avoids redundant disk reads across agents:

```rust
// Agent 1 reads a file (cache miss)
let content = cache.get_file(&path);  // None - reads from disk
cache.put_file(path, content, mtime, "agent-1");

// Agent 2 reads same file (cache hit)
let content = cache.get_file(&path);  // Some(content) - from cache
```

Features:
- **TTL-based expiration** (default: 30s)
- **Mtime validation** - invalidates if file modified
- **Reader tracking** - which agents read which files
- **Size-limited** (default: 100MB) with LRU eviction

### 2. Knowledge Store

Semantic key-value store for agent discoveries:

```rust
// Agent 1 discovers JWT configuration
cache.share_knowledge(
    "auth/jwt-config",
    json!({"algorithm": "HS256", "expiry": 3600}),
    "explore-agent"
);

// Agent 2 queries the knowledge
let jwt_config = cache.get_knowledge("auth/jwt-config");
let all_auth = cache.search_knowledge("auth/");  // prefix search
```

### 3. Agent Findings

Structured results from completed agents:

```rust
cache.store_agent_findings(AgentFindings {
    agent_id: "code-review-1".into(),
    agent_type: "code-review".into(),
    summary: "Found 3 security issues in auth module".into(),
    findings: vec![
        Finding {
            category: FindingCategory::Security,
            title: "SQL Injection".into(),
            detail: "Unsanitized input in user_query()".into(),
            confidence: 0.95,
            related_files: vec!["src/db.rs".into()],
        }
    ],
    completed_at: SystemTime::now(),
});

// Query findings
let security_issues = cache.get_findings_by_category(&FindingCategory::Security);
let summary = cache.summarize_findings();  // For prompt injection
```

## Tool Integration

### share_context

Share knowledge with sibling agents:

```json
{
  "tool": "share_context",
  "args": {
    "key": "jwt-config",
    "value": {"algorithm": "HS256"},
    "category": "security"
  }
}
```

Returns:
```json
{
  "status": "shared",
  "key": "security/jwt-config",
  "source_agent": "explore-1"
}
```

### query_context

Query shared knowledge:

```json
// Exact key lookup
{"tool": "query_context", "args": {"key": "security/jwt-config"}}

// Prefix search
{"tool": "query_context", "args": {"prefix": "auth/"}}

// List all keys
{"tool": "query_context", "args": {"list_keys": true}}

// Include agent findings summary
{"tool": "query_context", "args": {"prefix": "", "include_findings": true}}
```

## Finding Categories

| Category | Description |
|----------|-------------|
| `code_pattern` | Code patterns or anti-patterns |
| `dependency` | Dependency information |
| `architecture` | Architecture insights |
| `security` | Security observations |
| `performance` | Performance findings |
| `documentation` | Documentation gaps |
| `custom` | User-defined categories |

## Configuration

```rust
let cache = SharedContextCache::new(
    Duration::from_secs(60),    // file TTL
    200 * 1024 * 1024,          // max 200MB file cache
);
```

Default values:
- File TTL: 30 seconds
- Max file cache: 100MB

## Usage in DynamicAgentSpawner

The cache is automatically shared across all agents spawned by the same parent:

```rust
let spawner = DynamicAgentSpawner::new(/*...*/);
let cache = spawner.context_cache().clone();

// All child agents share the same cache instance
// Parent can query findings from children:
let all_findings = cache.summarize_findings();
```

## Statistics

```rust
let stats = cache.stats();
// CacheStats {
//     file_count: 15,
//     file_cache_bytes: 524288,
//     knowledge_count: 8,
//     agent_findings_count: 3,
// }
```

## Thread Safety

All cache operations are thread-safe using `DashMap`:
- Lock-free concurrent reads
- Fine-grained locking for writes
- Atomic counters for size tracking

## Best Practices

1. **Use semantic keys** - `auth/jwt-config` not `config1`
2. **Apply categories** - Helps filtering and discovery
3. **Include related files** - In findings for traceability
4. **Set confidence scores** - Helps prioritization
5. **Check cache before file reads** - Reduces I/O

## Related

- [Permission Sync](permission-sync.md) - Cross-agent permission inheritance
- [Multi-Agent Delegation Guide](design/multi-agent-delegation-guide.md)
