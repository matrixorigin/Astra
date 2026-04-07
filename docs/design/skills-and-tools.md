# Skills and Tools

> **Status**: Core Design — single source of truth for skill/tool/MCP architecture
> **Last Updated**: 2026-07-10
> **Supersedes**: `skills-and-tools-v1-python.md` (pre-Rust migration, archived for reference)
> **Implementation**: Rust — `rust/crates/runtime/src/skills/`, `rust/crates/runtime/src/tool_registry/`
>
> 🟢 **Implemented**: UnifiedSkillRegistry, SKILL.md parser, 16 bundled skills, local/MCP providers,
> ToolRegistry with pinned/dynamic split, token budget system, file watcher hot-reload,
> CLI commands (/skill list|info|search|new|dev|test|doctor|validate|config|system),
> non-blocking permission checks, skill tool schema injection.
>
> 🔵 **Design Target**: Pin/Unpin mechanism (§4), Registered skills via DB (§3.2),
> Marketplace via MatrixOne Stage (§3.3), cloud skill publishing.

---

## 1. Core Concepts

### 1.1 Skill vs Tool

Skills and Tools are orthogonal systems that serve different purposes:

| Aspect       | Tool                              | Skill                                |
|--------------|-----------------------------------|--------------------------------------|
| **What**     | JSON schema for LLM function call | AI instruction set (SKILL.md)        |
| **Selection**| Token budget, pinning, TF-IDF     | Metadata budget, path activation     |
| **Injection**| `tools` array in API request      | System prompt text (or sub-agent)    |
| **Execution**| Tool call → handler → result      | Inline expand or fork sub-agent      |
| **Budget**   | Schema JSON tokens                | Metadata tokens + instruction tokens |
| **Source**    | Built-in handlers + MCP servers   | Bundled + local + DB + MCP + market  |

The **`skill` tool** is the bridge: a single JSON-schema tool that the LLM calls to invoke
any skill. When called, it returns the skill's instructions for inline execution, or
triggers a sub-agent fork.

### 1.2 What a Skill Is

A skill is a **versioned, instruction-based capability package** defined by SKILL.md:

```yaml
---
name: code-review
description: Review code for quality, security, and best practices
version: 1.2.0
context: fork          # inline (default) or fork (sub-agent)
allowed-tools:         # empty = all tools available
  - read_file
  - grep
  - glob
when-to-use: When asked to review code, PRs, or assess code quality
model: claude-sonnet-4-20250514   # optional override
max_tokens: 8000       # optional budget
paths:                 # conditional activation (glob patterns)
  - "**/*.rs"
  - "**/*.py"
triggers:              # auto-activation keywords
  - review
  - code quality
tags:
  - code-review
  - quality
category: development
arguments:
  - name: focus
    description: What aspect to focus on (security, performance, style)
    required: false
---

# Code Review Instructions

You are an expert code reviewer. Analyze the code for...
$ARGUMENTS
```

**Claude Code Compatibility**: Our SKILL.md format is a superset of Claude Code's.
We support all CC frontmatter fields (`name`, `description`, `when-to-use`,
`allowed-tools`, `arguments`, `argument-hint`, `model`, `effort`, `context`,
`hooks`, `paths`) plus extensions: `version`, `category`, `tags`, `triggers`,
`max_tokens`, `dependencies`.

### 1.3 SkillManifest (Universal Descriptor)

```rust
// rust/crates/runtime/src/skills/manifest.rs
pub struct SkillManifest {
    pub name: String,
    pub version: Version,              // Semantic version
    pub description: String,
    pub source: SkillSourceKind,       // Local | Bundled | Database | Mcp | Plugin
    pub execution_context: ExecutionContext,  // Inline | Fork
    pub user_invocable: bool,
    pub triggers: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub when_to_use: Option<String>,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub hooks: Option<SkillHooks>,
    pub paths: Vec<String>,            // Conditional activation globs
    pub arguments: Vec<SkillArgument>,
    pub dependencies: Vec<Dependency>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub metadata: HashMap<String, Value>,
}
```

### 1.4 LoadedSkill (Full Content)

```rust
// rust/crates/runtime/src/skills/manifest.rs
pub struct LoadedSkill {
    pub manifest: SkillManifest,
    pub instructions: String,          // Markdown body below frontmatter
    pub instruction_tokens: u32,
    pub resources: Option<SkillResources>,  // Templates, scripts
    pub skill_dir: Option<PathBuf>,
}
```

---

## 2. Skill Sources & Discovery

### 2.1 Three-Layer Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  Layer 3: Cloud Marketplace (MatrixOne Stage)     [DESIGN]      │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │
│  │ Official     │  │ Community    │  │ Private      │          │
│  │ (verified)   │  │ (published)  │  │ (user-only)  │          │
│  └──────────────┘  └──────────────┘  └──────────────┘          │
│  Storage: stage://mo_skills/   Download → local cache           │
├─────────────────────────────────────────────────────────────────┤
│  Layer 2: Registered Skills (MatrixOne Database)  [DESIGN]      │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │
│  │ Team shared  │  │ User private │  │ Installed    │          │
│  │ (org scope)  │  │ (account)    │  │ (from market)│          │
│  └──────────────┘  └──────────────┘  └──────────────┘          │
│  Storage: skills_registry table    Sync → local on demand       │
├─────────────────────────────────────────────────────────────────┤
│  Layer 1: Local Skills (Filesystem)               [IMPLEMENTED] │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │
│  │ Bundled      │  │ Project      │  │ User global  │          │
│  │ (in binary)  │  │ (.astra/     │  │ (~/.astra/   │          │
│  │ 16 skills    │  │  skills/)    │  │  skills/)    │          │
│  └──────────────┘  └──────────────┘  └──────────────┘          │
│  Always available, no network needed, hot-reload                │
└─────────────────────────────────────────────────────────────────┘
```

### 2.2 Source Priority

When multiple sources provide the same skill name, priority order determines the winner:

```
Local (0) > Bundled (1) > Database (2) > Mcp (3) > Plugin (4)
```

This means a project's local override always wins over a bundled or remote skill.

### 2.3 SkillSourceKind

```rust
// rust/crates/runtime/src/skills/manifest.rs
pub enum SkillSourceKind {
    Local,       // Filesystem: .astra/skills/, skills/, ~/.astra/skills/
    Bundled,     // Compiled into binary (16 built-in skills)
    Database,    // MatrixOne skills_registry table
    Mcp,         // MCP server resources (skill:// URIs)
    Plugin,      // External plugin
}
```

### 2.4 Skill Providers

Each source implements `SkillProvider`:

```rust
// rust/crates/runtime/src/skills/traits.rs
#[async_trait]
pub trait SkillProvider: Send + Sync {
    fn source_kind(&self) -> SkillSourceKind;
    async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError>;
    async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError>;
    async fn refresh(&self) -> Result<(), SkillError>;
}
```

| Provider              | Location                      | Status      | Notes                              |
|-----------------------|-------------------------------|-------------|------------------------------------|
| BundledSkillProvider  | `skills/providers/bundled.rs` | ✅ Implemented | 16 skills compiled in binary      |
| LocalSkillProvider    | `skills/providers/local.rs`   | ✅ Implemented | Scans 3 paths, symlink dedup      |
| DatabaseSkillProvider | `skills/providers/database.rs`| ⚠️ Adapter   | Wraps SkillService, needs tests   |
| McpSkillProvider      | `skills/providers/mcp.rs`     | ✅ Implemented | Keyed cache (server, skill) pair  |

### 2.5 Discovery Flow

```
UnifiedSkillRegistry::discover_all()
  ├─ For each provider (Local → Bundled → Database → MCP):
  │   └─ provider.discover() → Vec<SkillManifest>
  ├─ Sort by source_priority (lower = higher priority)
  ├─ Deduplicate by name (first wins)
  ├─ Apply metadata_budget (skip if exceeds token limit)
  ├─ Separate unconditional vs conditional (paths-based) skills
  ├─ Cache manifests (metadata only, instructions lazy-loaded)
  └─ Return registered skill names
```

### 2.6 Local Skill Search Paths

```rust
// rust/crates/runtime/src/skills/loader.rs
pub fn skill_search_paths() -> Vec<PathBuf> {
    vec![
        cwd/.astra/skills/,     // Project-specific
        cwd/skills/,            // Project root
        ~/.astra/skills/,       // User global
    ]
}
```

### 2.7 Bundled Skills (16)

| Name           | Context | Description                                  |
|----------------|---------|----------------------------------------------|
| debug          | inline  | Debug issues and errors                      |
| stuck          | inline  | Help when stuck or confused                  |
| verify         | inline  | Verify changes work correctly                |
| perf           | inline  | Performance optimization                     |
| simplify       | inline  | Simplify complex code                        |
| explain        | inline  | Explain code and concepts                    |
| commit-msg     | inline  | Generate commit messages                     |
| remember       | inline  | Store facts for future sessions              |
| init-project   | inline  | Initialize new projects                      |
| skillify       | inline  | Create new skills from patterns              |
| github         | inline  | GitHub operations                            |
| pr-review      | fork    | Review pull requests                         |
| refactor       | fork    | Large-scale refactoring                      |
| security-scan  | fork    | Security vulnerability scanning              |
| test-gen       | fork    | Generate test suites                         |
| batch          | fork    | Batch operations across files                |

---

## 3. Skill Layers — Detailed Design

### 3.1 Layer 1: Local Skills [IMPLEMENTED]

**Lifecycle**: Create → Discover → Auto-Pin → Use → Edit → Hot-reload → Delete

**Characteristics**:
- **No registration required** — drop SKILL.md file, immediately available
- **Weak constraints** — no version control, no permission management, no audit
- **Hot-reload** — file watcher detects changes within 500ms
- **Auto-pinned** — always in candidate list (budget-exempt)
- **Best for** — personal development, rapid iteration, project-specific skills

**File Watcher** (`skills/watcher.rs`):
- Uses `notify` crate with `RecommendedWatcher`
- 500ms debounce interval
- Monitors `.astra/skills/`, `skills/`, `~/.astra/skills/`
- Triggers `discover_all()` on SKILL.md / manifest.yaml changes
- Handle stored in REPL state, dropped on exit

### 3.2 Layer 2: Registered Skills [DESIGN TARGET]

**Lifecycle**: Create/Upload → Register → Discover → Manual Pin → Use → Update → Deregister

**Characteristics**:
- **Stored in MatrixOne** — `skills_registry` table
- **Strong constraints** — version control, scope-based access (org/user), audit trail
- **Default unpinned** — discovered via search/budget, manually pinned
- **Best for** — team sharing, production environments, compliance

**Proposed Schema**:
```sql
CREATE TABLE skills_registry (
    id           BIGINT AUTO_INCREMENT PRIMARY KEY,
    name         VARCHAR(128) NOT NULL,
    version      VARCHAR(32) NOT NULL DEFAULT '0.1.0',
    scope        ENUM('user', 'org', 'public') DEFAULT 'user',
    owner        VARCHAR(128) NOT NULL,
    manifest     JSON NOT NULL,          -- SkillManifest serialized
    content      TEXT NOT NULL,           -- SKILL.md instruction body
    pinned       BOOLEAN DEFAULT FALSE,
    enabled      BOOLEAN DEFAULT TRUE,
    created_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uk_name_version_owner (name, version, owner)
);
```

**CLI Commands**:
```bash
/skill register <name> [--scope=org|user] [--pin]
/skill deregister <name>
```

### 3.3 Layer 3: Cloud Marketplace [DESIGN TARGET]

**Lifecycle**: Browse → Preview → Install → Register (auto) → Pin (optional) → Use

**Characteristics**:
- **Stored in MatrixOne Stage** — S3/MinIO backed object storage
- **Install = Download + Register** — cached locally, registered in DB
- **Versioned** — directory-based versioning in Stage
- **Best for** — community sharing, enterprise skill stores

**Stage Layout**:
```
stage://mo_skill_marketplace/
├── official/                    # Verified skills
│   └── code-review/
│       ├── 1.0.0/
│       │   ├── manifest.yaml
│       │   ├── SKILL.md
│       │   └── resources/
│       └── 1.1.0/
├── community/                   # Community published
└── private/<account_id>/        # Per-account private skills
```

**Stage SQL**:
```sql
-- Admin creates marketplace stage
CREATE STAGE mo_skill_marketplace
  URL = 's3://mo-skills/'
  CREDENTIALS = { 'AWS_KEY_ID'='...', 'AWS_SECRET_KEY'='...' }
  COMMENT = 'Skill marketplace';

-- Install: download from stage
LOAD DATA INFILE 'stage://mo_skill_marketplace/official/code-review/1.1.0/SKILL.md'
  INTO TABLE skill_download_cache ...;

-- Publish: upload to stage
SELECT manifest INTO OUTFILE 'stage://mo_skill_marketplace/private/<account>/my-skill/1.0.0/manifest.yaml'
  FROM skill_export WHERE name = 'my-skill';
```

**CLI Commands**:
```bash
/skill browse [category]
/skill install <name>[@version] [--pin]
/skill uninstall <name>
/skill publish <name> [--scope=private|community]
/skill update <name>
```

---

## 4. Pin Mechanism [DESIGN TARGET]

### 4.1 What is Pin?

**Pin = always in candidate list, budget-exempt.**

Pinned skills are always included in the skill listing sent to the LLM, regardless
of token budget pressure. Unpinned skills are subject to budget-based truncation
and require `/skill search` or trigger-based activation to surface.

Analogy to Claude Code: `alwaysLoad: true` ≈ our Pin.

### 4.2 Pin Rules by Source

| Source          | Default  | Can Change?     | Notes                        |
|-----------------|----------|-----------------|------------------------------|
| Bundled         | Always   | No (permanent)  | Core functionality           |
| Local (project) | Pinned   | Yes (unpin)     | Zero-config best experience  |
| Local (user)    | Pinned   | Yes (unpin)     | User's global skills         |
| Registered      | Unpinned | Yes (pin/unpin) | Discovered via search/budget |
| Marketplace     | Unpinned | Yes (pin/unpin) | Pin on install with `--pin`  |
| MCP             | Unpinned | Yes (pin/unpin) | Discovered via search/budget |

### 4.3 Budget Interaction

```
Skill Listing Budget: ~2000 tokens (1% of 200K context)
┌──────────────────────────────────────────┐
│ Pinned skills (always shown, full desc)  │ ← Fixed cost
│  bundled:  16 × ~30 tokens = 480        │
│  local:     4 × ~30 tokens = 120        │
│  user-pin:  N × ~30 tokens              │
├──────────────────────────────────────────┤
│ Remaining budget for unpinned            │ ← Dynamic
│  (truncated descriptions → names-only)   │
└──────────────────────────────────────────┘
```

**Warning Threshold**: When pinned skills consume >70% of listing budget:
```
⚠ 45 pinned skills using 1400/2000 tokens of skill listing budget.
  Consider: /skill unpin <name>  or  /config skill_listing_budget 4000
```

### 4.4 SkillManifest Pin Field

```rust
// Addition to SkillManifest
pub struct SkillManifest {
    // ... existing fields ...
    pub pinned: PinState,  // Always | Pinned | Unpinned
}

pub enum PinState {
    Always,    // Bundled — cannot be unpinned
    Pinned,    // User/auto pinned — budget-exempt
    Unpinned,  // Subject to budget truncation
}
```

### 4.5 CLI Commands

```bash
/skill pin <name>           # Pin a skill (registered/marketplace/MCP)
/skill unpin <name>         # Unpin (bundled cannot be unpinned)
/skill pinned               # Show all pinned skills with budget usage
```

---

## 5. Tool Selection Pipeline [IMPLEMENTED]

### 5.1 ToolRegistry Architecture

```rust
// rust/crates/runtime/src/tool_registry/registry.rs
pub struct ToolRegistry {
    all_schemas: Vec<Value>,                // All tool JSON schemas
    budget_tokens: u32,                     // Token budget for dynamic tools
    measured_costs: HashMap<String, u32>,   // Real token costs per tool
    schema_index: HashMap<String, usize>,   // O(1) name→index lookup
    pinned_schemas: Vec<(String, Value)>,   // Always-included (budget-exempt)
    plugin_tool_names: Vec<String>,         // Dynamically registered tools
}
```

### 5.2 Tool Selection Layers

1. **Pinned Tools** — Always included, no budget cost (bash, read_file, write_file, etc.)
2. **Injected Tools** — Runtime-injected (e.g., `skill` tool), pinned by default
3. **Dynamic Tools** — Ranked by TF-IDF + intent signals, selected within budget

### 5.3 Selection Methods (Progressive Quality)

| Method                | Use Case              | Features                              |
|-----------------------|-----------------------|---------------------------------------|
| `select()`            | Basic selection       | TF-IDF + intent                       |
| `select_with_report()`| With telemetry       | + SelectionReport                     |
| `select_with_quality()`| Quality-aware       | + ToolQualityTracker                  |
| `select_calibrated()` | Full pipeline         | + confidence calibration + boost      |
| `select_routed()`     | With routing decision | Uses RoutingDecision from pipeline    |

### 5.4 Skill Tool Integration

The `skill` tool is injected into the ToolRegistry as a pinned schema:

```rust
// rust/crates/runtime/src/turn/skill_tool.rs
pub fn skill_tool_schema(skills: &[SkillToolInfo]) -> Value {
    // Generates JSON schema with skill names as enum
    // Budget-aware: format_skills_within_budget() trims descriptions
}

// Injected via:
registry.inject_schema("skill", schema, /* pinned = */ true);
```

### 5.5 Token Budget for Skill Listing

```rust
const DEFAULT_SKILL_LISTING_BUDGET: usize = 8_000;  // ~1% of 200K context
const MAX_LISTING_DESC_CHARS: usize = 250;           // Per-skill cap

fn format_skills_within_budget(skills: &[SkillToolInfo], budget: usize)
    -> (Vec<String>, Vec<String>)
{
    // Tier 1: Bundled skills — always full description
    // Tier 2: Other sources — proportionally truncated
    // Tier 3: Names-only when under extreme budget pressure
}
```

---

## 6. Conditional Skill Activation [IMPLEMENTED]

### 6.1 Path-Based Activation

Skills with `paths` globs are **conditional** — only visible after matching files are touched:

```yaml
paths:
  - "**/*.rs"
  - "Cargo.toml"
```

### 6.2 Activation Tracking

```rust
// rust/crates/runtime/src/skills/activation.rs
pub struct ConditionalSkillTracker {
    activated: HashSet<String>,     // Skill names that matched
    seen_paths: HashSet<String>,    // Dedup: don't re-check same path
}
```

When a file is read/written during a turn, `registry.record_file_path()` checks
all conditional skills and activates matches.

### 6.3 Trigger-Based Activation

Skills with `triggers` keywords are auto-detected from user messages:

```rust
pub fn detect_triggers(skills: &[SkillManifest], message: &str) -> Vec<String> {
    // Word-level matching, case-insensitive
    // Supports CJK character matching
    // Sorted by trigger specificity (longer triggers first)
}
```

---

## 7. MCP Integration [IMPLEMENTED]

### 7.1 MCP Dual Role

MCP servers provide both **tools** (function call schemas) and **skills** (instruction sets):

```
MCP Server
├── tools/       → ToolRegistry (function calling)
│   ├── search_code
│   └── query_db
└── resources/   → McpSkillProvider → UnifiedSkillRegistry
    └── skill://code-review → SKILL.md content
```

### 7.2 McpSkillProvider

```rust
// rust/crates/runtime/src/skills/providers/mcp.rs
pub struct McpSkillProvider {
    cache: RwLock<HashMap<(String, String), McpSkillEntry>>,
    // Keyed by (server_name, skill_name) to prevent cross-server collisions
}
```

**Key operations**:
- `register_mcp_skill(server_name, skill_md_content)` — parse and cache
- `remove_server_skills(server_name)` — cleanup on disconnect
- Integrated into `UnifiedSkillRegistry` as a provider

---

## 8. Execution Model [IMPLEMENTED]

### 8.1 Execution Contexts

| Context  | Behavior                                   | Use Case                    |
|----------|--------------------------------------------|-----------------------------|
| `inline` | Instructions appended to conversation      | Quick tasks, advice, prompts|
| `fork`   | Sub-agent with isolated context            | Large tasks, code review    |

### 8.2 Execution Flow

```
LLM calls skill tool → SkillResolver.resolve(name) → ResolvedSkill
  │
  ├─ Inline: Return instructions as tool result → LLM continues with instructions
  │
  └─ Fork: Spawn sub-agent with:
     ├─ Isolated conversation context
     ├─ Skill instructions as system prompt
     ├─ allowed_tools filter applied
     ├─ Optional model override
     └─ Return sub-agent result to parent conversation
```

### 8.3 Permission & Approval

Tool execution goes through a 6-step permission pipeline:

1. **Deny rules** — bypass-immune, checked first
2. **Session overrides** — per-session allow/deny
3. **Side-effect classification** — Read (auto-allow) vs Write/Execute
4. **Git safety** — force-push, history rewrite protection
5. **Dangerous path** — system file protection
6. **Mode-based** — Auto/Ask/Deny based on configuration

**Non-blocking in SSE consumer**: `check_nonblocking()` avoids blocking the async
runtime. For normal REPL, `spawn_blocking()` used for terminal prompt.

---

## 9. Error Handling [IMPLEMENTED]

```rust
// rust/crates/runtime/src/skills/traits.rs
pub enum SkillError {
    NotFound(String),          // Skill not found
    LoadFailed(String),        // Disk read failure
    ParseFailed(String),       // YAML/frontmatter parse error
    VersionConflict(String),   // Semver mismatch
    DependencyError(String),   // Dependency resolution failure
    ExecutionFailed(String),   // Runtime execution error
    PermissionDenied(String),  // Access denied (path traversal, restricted)
    BudgetExceeded(String),    // Token budget exceeded
    Internal(String),          // Infrastructure errors (lock poisoning)
}

impl From<std::io::Error> for SkillError {
    // NotFound → SkillError::NotFound
    // PermissionDenied → SkillError::PermissionDenied
    // Other → SkillError::Internal
}
```

---

## 10. CLI Commands [IMPLEMENTED]

### Discovery & Inspection
```bash
/skill                          # Show subcommand help
/skill list [query] [--source=local|bundled|mcp] [--category=X]
/skill search <query>           # Keyword match on catalog (not vector search)
/skill surfacing …              # Agent catalog listing vs discover_skills thresholds
/skill info <name> [--raw]      # Manifest + preview; --raw = YAML frontmatter
/skill pinned                   # [DESIGN] Show pinned skills + budget
```

### Skill Development
```bash
/skill new <name>               # Scaffold .astra/skills/<name>/
/skill dev <name|off>           # Dev mode (source injected each turn)
/skill test <name> [json_args]  # API test or local manifest + hooks
/skill system <name|list>       # System prompt integration
```

### Health
```bash
/skill health                   # Catalog + on-disk SKILL.md checks (API when logged in)
```

### Registration & Marketplace [DESIGN TARGET]
```bash
/skill register <name> [--scope=org|user] [--pin]
/skill deregister <name>
/skill pin <name>
/skill unpin <name>
/skill browse [category]
/skill install <name>[@version] [--pin]
/skill uninstall <name>
/skill publish <name> [--scope=private|community]
/skill update <name>
```

---

## 11. Search & Discovery [IMPLEMENTED]

### 11.1 `/skill search` — Relevance Scoring

Multi-field fuzzy search across all skill metadata:

| Field         | Exact Match | Contains | Word Match |
|---------------|-------------|----------|------------|
| name          | 20 points   | 10       | 5          |
| tags          | 8           | 4        | 4          |
| description   | —           | 6        | 2          |
| category      | —           | 5        | —          |
| when_to_use   | —           | 4        | 1          |
| triggers      | —           | 3        | 3          |

Results ranked by score with star ratings (★★★ ≥10, ★★ ≥5, ★ <5).

### 11.2 `/skill list` — Filtered Listing

Supports combined text + flag filtering:
```bash
/skill list review                    # Text search
/skill list --source=local            # Source filter
/skill list --category=code-review    # Category filter
/skill list debug --source=bundled    # Combined
```

---

## 12. MatrixOne Stage Integration [DESIGN TARGET]

### 12.1 What is Stage?

MatrixOne Stage is a storage abstraction layer for data import/export:
- Supports S3, HDFS, local filesystem backends
- Hierarchical: sub-stages can reference parent stages
- Account-scoped with credential management
- SQL-native: `stage://` URLs in LOAD DATA / SELECT INTO OUTFILE

### 12.2 Skill Marketplace on Stage

```sql
-- Create marketplace stage (admin)
CREATE STAGE mo_skill_marketplace
  URL = 's3://mo-skills-registry/'
  CREDENTIALS = {...}
  COMMENT = 'Official skill marketplace';

-- Sub-stages for organization
CREATE STAGE marketplace_official URL = 'stage://mo_skill_marketplace/official/';
CREATE STAGE marketplace_private  URL = 'stage://mo_skill_marketplace/private/';
```

### 12.3 Install Flow

```
/skill install code-review@1.1.0
  │
  ├─ 1. Resolve: Query skill_marketplace_index for package URL
  ├─ 2. Download: LOAD DATA INFILE 'stage://marketplace_official/code-review/1.1.0/*'
  ├─ 3. Cache: Store in ~/.astra/cache/skills/code-review/1.1.0/
  ├─ 4. Register: INSERT INTO skills_registry (name, version, manifest, content, ...)
  ├─ 5. Optional Pin: if --pin flag specified
  └─ 6. Discover: registry.discover_all() picks up new skill
```

### 12.4 Publish Flow

```
/skill publish my-skill --scope=private
  │
  ├─ 1. Validate: manifest.yaml + SKILL.md format check
  ├─ 2. Package: Bundle skill directory
  ├─ 3. Upload: INTO OUTFILE 'stage://marketplace_private/<account>/my-skill/1.0.0/'
  ├─ 4. Index: INSERT INTO skill_marketplace_index (name, version, scope, stage_url, ...)
  └─ 5. Notify: "Published my-skill@1.0.0 (private)"
```

### 12.5 Local vs Cloud Private Skills

| Feature         | Local (Layer 1)       | Cloud Private (Layer 3)   |
|-----------------|-----------------------|---------------------------|
| Storage         | Filesystem            | MatrixOne Stage (S3)      |
| Availability    | This machine only     | Any logged-in device      |
| Version control | Git (manual)          | Stage directory versioning|
| Sharing         | Manual copy           | `/skill share <name>`     |
| Backup          | None (user manages)   | Stage persistence         |
| Constraints     | Weak (no validation)  | Medium (schema validated) |
| Best for        | Development iteration | Production use            |

---

## 13. Design Decisions

### Q1: Do local skills need database registration?
**No.** Local skills are weak-constraint, zero-config. Drop file → use immediately.
Matches Claude Code behavior and provides best development experience.

### Q2: Is Pin a weight or a switch?
**Switch.** Pin = always in list (budget-exempt). Unpinned = subject to budget
truncation + search discovery. Simple, predictable, no weight-tuning complexity.

### Q3: What happens after marketplace install?
**Becomes a Registered skill (Layer 2).** Downloaded to local cache + registered in DB.
Works offline via cache. Updates check marketplace for newer versions.

### Q4: How many pinned skills are reasonable?
**~30-40 under default budget.** 16 bundled + 4-5 local + 10-20 user-pinned.
Warning at 70% budget usage. User can increase budget via config.

### Q5: How do MCP tools differ from MCP skills?
**MCP tools** are JSON schemas → direct function calling.
**MCP skills** are `skill://` resources → SKILL.md instructions.
Both come from same MCP server but enter different systems.

---

## 14. Implementation Status

| Component                    | Status      | Location                                  | Tests |
|------------------------------|-------------|-------------------------------------------|-------|
| UnifiedSkillRegistry         | ✅ Done     | `runtime/src/skills/registry.rs`          | 30+   |
| SKILL.md Parser              | ✅ Done     | `runtime/src/skills/loader.rs`            | 15+   |
| BundledSkillProvider (16)    | ✅ Done     | `runtime/src/skills/providers/bundled.rs` | 10+   |
| LocalSkillProvider           | ✅ Done     | `runtime/src/skills/providers/local.rs`   | 10+   |
| McpSkillProvider             | ✅ Done     | `runtime/src/skills/providers/mcp.rs`     | 5+    |
| DatabaseSkillProvider        | ⚠️ Adapter  | `runtime/src/skills/providers/database.rs`| 2     |
| ToolRegistry (pinned/dynamic)| ✅ Done     | `runtime/src/tool_registry/registry.rs`   | 50+   |
| Skill tool schema + budget   | ✅ Done     | `runtime/src/turn/skill_tool.rs`          | 10+   |
| Conditional activation       | ✅ Done     | `runtime/src/skills/activation.rs`        | 15+   |
| File watcher hot-reload      | ✅ Done     | `runtime/src/skills/watcher.rs`           | 3     |
| CLI /skill commands          | ✅ Done     | `rust/crates/astra-cli/src/cli/slash_skill.rs`    | 12    |
| Non-blocking permission      | ✅ Done     | `rust/crates/astra-cli/src/cli/stream_render.rs`  | —     |
| Pin/Unpin mechanism          | 🔵 Design  | —                                          | —     |
| Registered skills (DB)       | 🔵 Design  | —                                          | —     |
| Marketplace (Stage)          | 🔵 Design  | —                                          | —     |
| Skill sandbox mode           | 🔵 Design  | —                                          | —     |

---

## 15. References

- [Claude Code skill system](~/claudecode) — reference implementation for CC compatibility
- [skills-and-tools-v1-python.md](skills-and-tools-v1-python.md) — archived Python-era design (conceptual reference)
- [skill-system-review-2026-03-31.md](../../plans/skill-system-review-2026-03-31.md) — authoritative audit
- [tool-discovery-claude-code.md](tool-discovery-claude-code.md) — CC tool selection gap analysis
- [context-window-management.md](context-window-management.md) — context budget architecture
- [isolated-skill-execution-design.md](../review/isolated-skill-execution-design.md) — fork execution design
