# Skills-First: The Enterprise-Grade LLM Agent Architecture

**Status**: Design  
**Created**: 2026-02-10  
**Phase**: 2 of 6  
**Subtitle**: Building Reproducible, Auditable, and Scalable AI Agents

---

## Executive Summary

This document presents **Skills-First Architecture**: an enterprise-grade approach to building LLM agents that are:
- **Reproducible**: "10 years later, reproduce today's decision"
- **Auditable**: Every action traced to its cause
- **Scalable**: From 3 skills to 50+ without redesign
- **Safe**: Declarative permissions, graceful errors, human-in-the-loop

**Core Innovation**: Skills are first-class citizens with versioning, declarative requirements, and full lifecycle management.

---

### Core Question: What is an "Agent"?

An agent is **NOT**:
- A chatbot that answers questions
- A wrapper around LLM APIs
- A tool that executes predefined scripts

An agent **IS**:
- A system that **understands intent** and **selects actions**
- A system that **learns from context** (conversation + repo + history)
- A system that **executes skills** with **full auditability**
- A system that **reproduces decisions** years later with exact fidelity

### Design Principles

1. **Skills as First-Class Citizens**
   - Skills are NOT functions - they are **versioned, declarative capabilities**
   - Each skill declares: "I need X repo type with Y permission"
   - Framework enforces requirements, not skills themselves
   - **Skills are versioned** - replay uses the exact version from the past

2. **Context is Everything**
   - Agent decisions depend on: conversation + repo + skills + history
   - Context must be **queryable** (from MatrixOne) and **extensible** (add RAG later)
   - Prompt structure must support **future capabilities** without redesign

3. **LLM as Decision Engine, Not Executor**
   - LLM selects skill + extracts parameters
   - Skills execute logic (GitHub API, DB queries, etc.)
   - LLM does NOT write code or execute arbitrary commands

4. **Everything Flows Through Events**
   - User query → event
   - Skill selection → event (with skill version)
   - Skill execution → event (with skill version)
   - LLM response → event
   - This enables **replay** and **debugging**

5. **CLI First, Web Later**
   - CLI forces us to design clean APIs
   - CLI is the best dogfooding tool
   - Web UI is just another client

6. **Versioning Enables Time Travel**
   - Skills versioned (v1.0, v1.1, v2.0)
   - Prompts versioned (prompt_templates table)
   - LLM pricing versioned (llm_pricing table)
   - **Replay uses exact versions from the past**

4. **Everything Flows Through Events**
   - User query → event
   - Skill selection → event
   - Skill execution → event
   - LLM response → event
   - This enables **replay** and **debugging**

5. **CLI First, Web Later**
   - CLI forces us to design clean APIs
   - CLI is the best dogfooding tool
   - Web UI is just another client

---

## Architecture: Information Flow

```
┌─────────────────────────────────────────────────────────────┐
│  User: "Summarize PR #123"                                  │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  1. Event Logger: Log user_query event                      │
│     - session_id, user_id, content, timestamp               │
│     - Creates causal_chain_id                               │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  2. Context Manager: Build context                          │
│     ┌─────────────────────────────────────────────────┐    │
│     │ Query MatrixOne:                                │    │
│     │ - Recent conversation (last N events)          │    │
│     │ - Repo metadata (if repo_id provided)          │    │
│     │ - Available skills (filtered by repo)          │    │
│     │ - User history (placeholder for Phase 3)       │    │
│     └─────────────────────────────────────────────────┘    │
│                                                             │
│     ┌─────────────────────────────────────────────────┐    │
│     │ Build Prompt:                                   │    │
│     │ [System]                                        │    │
│     │ You are an agent. Available skills:            │    │
│     │ - summarize_pr: Summarize a PR                 │    │
│     │ - ci_status: Check CI status                   │    │
│     │                                                 │    │
│     │ Current repo: matrixone (CODE, READ)           │    │
│     │                                                 │    │
│     │ Recent conversation:                            │    │
│     │ User: What's the status of CI?                 │    │
│     │ Agent: All workflows passing.                  │    │
│     │                                                 │    │
│     │ [User]                                          │    │
│     │ Summarize PR #123                              │    │
│     └─────────────────────────────────────────────────┘    │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  3. Agent Orchestrator: Call LLM for skill selection        │
│     ┌─────────────────────────────────────────────────┐    │
│     │ LLM Input: [System + User prompt]              │    │
│     │                                                 │    │
│     │ LLM Output (JSON):                              │    │
│     │ {                                               │    │
│     │   "skill_name": "summarize_pr",                │    │
│     │   "parameters": {"pr_number": 123},            │    │
│     │   "reasoning": "User wants PR summary"         │    │
│     │ }                                               │    │
│     └─────────────────────────────────────────────────┘    │
│                                                             │
│     Log: llm_call_logs (skill_selection step)              │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  4. Skill Registry: Resolve and validate skill              │
│     ┌─────────────────────────────────────────────────┐    │
│     │ Get skill: summarize_pr                         │    │
│     │                                                 │    │
│     │ Check requirements:                             │    │
│     │ - Needs: CODE repo + READ access               │    │
│     │ - Has: CODE repo + READ access ✓               │    │
│     │                                                 │    │
│     │ Validate input:                                 │    │
│     │ - pr_number: int ✓                             │    │
│     │ - repo_id: resolved from context ✓             │    │
│     └─────────────────────────────────────────────────┘    │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  5. Skill Execution: summarize_pr                           │
│     ┌─────────────────────────────────────────────────┐    │
│     │ Step 1: Fetch PR from GitHub                    │    │
│     │ - GET /repos/{owner}/{repo}/pulls/123          │    │
│     │ - Returns: title, body, diff, files_changed    │    │
│     │                                                 │    │
│     │ Step 2: Build LLM prompt                        │    │
│     │ - "Summarize this PR: [title] [body] [diff]"  │    │
│     │                                                 │    │
│     │ Step 3: Call LLM                                │    │
│     │ - Log to llm_call_logs (summarize_pr step)     │    │
│     │                                                 │    │
│     │ Step 4: Return result                           │    │
│     │ - summary: "This PR adds..."                   │    │
│     │ - files_changed: 5                             │    │
│     │ - cost: $0.002                                 │    │
│     └─────────────────────────────────────────────────┘    │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  6. Event Logger: Log llm_response event                    │
│     - parent_event_id: user_query event                     │
│     - causal_chain_id: same as user_query                   │
│     - content: skill result                                 │
│     - metadata: {skill: "summarize_pr", cost: 0.002}        │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  7. Response to User                                        │
│     "This PR adds feature X by modifying files A, B, C.     │
│      5 files changed. Cost: $0.002"                         │
└─────────────────────────────────────────────────────────────┘
```

**Key Insight**: Every step queries or writes to MatrixOne. This enables:
- **Replay**: Re-run any conversation with exact context
- **Debug**: Trace why agent made a decision
- **Audit**: Who did what, when, and why
- **Cost**: Track every dollar spent

---

## Design Deep Dive

### 1. Skill System: Declarative Capabilities

**Problem**: How do we make skills modular, testable, safe, and **reproducible**?

**Solution**: Skills declare requirements; framework enforces them. **Skills are versioned**.

```python
# Skill declares: "I need CODE repo with READ access"
class SummarizePRSkill(Skill):
    name = "summarize_pr"
    version = "1.0.0"  # Semantic versioning
    
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE],
        min_access=AccessScope.READ
    )
```

**Why this works**:
- Skills don't check permissions - framework does
- Skills don't resolve repos - framework does
- Skills focus on **logic**, not **infrastructure**
- **Skills are versioned** - can load old versions for replay

**Skill Lifecycle**:
```
Register → Store metadata in skills_registry (with version)
         → Keep in-memory for fast lookup
         → Archive old versions (don't delete)

Execute  → Framework validates requirements
         → Framework resolves repo_id
         → Skill executes with validated input
         → Result logged to conversation_events (with skill_version)

Replay   → Load skill version from event metadata
         → Execute with old skill logic
         → Reproduce exact behavior
```

**Skill Versioning Schema**:
```sql
-- skills_registry table (already exists in Phase 1)
CREATE TABLE skills_registry (
    skill_id INT AUTO_INCREMENT PRIMARY KEY,
    skill_name VARCHAR(255) NOT NULL,
    version VARCHAR(32) NOT NULL,           -- e.g., "1.0.0", "1.1.0"
    description TEXT,
    requirements JSON,                       -- {repo_types, min_access, llm_required}
    code_hash VARCHAR(64),                   -- SHA256 of skill code (for verification)
    is_active BOOLEAN DEFAULT TRUE,          -- Current version is active
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY (skill_name, version)
);

-- conversation_events table (add skill_version)
ALTER TABLE conversation_events 
ADD COLUMN skill_name VARCHAR(255),
ADD COLUMN skill_version VARCHAR(32);

-- Example data
INSERT INTO skills_registry (skill_name, version, description, requirements) VALUES
('summarize_pr', '1.0.0', 'Summarize PR with basic info', '{"repo_types": ["code"], "min_access": "read"}'),
('summarize_pr', '1.1.0', 'Summarize PR with diff analysis', '{"repo_types": ["code"], "min_access": "read"}'),
('summarize_pr', '2.0.0', 'Summarize PR with AI insights', '{"repo_types": ["code"], "min_access": "read"}');
```

**Skill Version Management**:
```python
class SkillRegistry:
    def register(self, skill: Skill, is_active: bool = True) -> None:
        """Register a skill version"""
        
        # 1. Deactivate old versions if this is active
        if is_active:
            self.db.execute("""
                UPDATE skills_registry 
                SET is_active = FALSE 
                WHERE skill_name = ?
            """, (skill.name,))
        
        # 2. Insert new version
        self.db.execute("""
            INSERT INTO skills_registry 
            (skill_name, version, description, requirements, code_hash, is_active)
            VALUES (?, ?, ?, ?, ?, ?)
        """, (
            skill.name,
            skill.version,
            skill.description,
            json.dumps(skill.requirements.model_dump()),
            self._compute_code_hash(skill),
            is_active
        ))
        
        # 3. Store in memory
        self._skills[f"{skill.name}@{skill.version}"] = skill
        if is_active:
            self._skills[skill.name] = skill  # Shortcut to active version
    
    def get(self, skill_name: str, version: str = None) -> Optional[Skill]:
        """Get skill by name and optional version"""
        if version:
            return self._skills.get(f"{skill_name}@{version}")
        else:
            return self._skills.get(skill_name)  # Active version
    
    def _compute_code_hash(self, skill: Skill) -> str:
        """Compute SHA256 hash of skill code for verification"""
        import hashlib
        import inspect
        
        code = inspect.getsource(skill.__class__)
        return hashlib.sha256(code.encode()).hexdigest()
```

**Replay with Skill Versioning**:
```python
class ReplayEngine:
    """Replay conversations with exact skill versions"""
    
    def replay_conversation(self, session_id: str, replay_timestamp: str = None):
        """Replay a conversation using skills from that time"""
        
        # 1. Fetch events
        events = self.db.execute("""
            SELECT event_id, event_type, content, skill_name, skill_version, created_at
            FROM conversation_events
            WHERE session_id = ?
            ORDER BY created_at
        """, (session_id,)).fetchall()
        
        # 2. Replay each event
        for event in events:
            if event["event_type"] == "skill_exec":
                # Load skill version from event
                skill = self.registry.get(
                    event["skill_name"],
                    version=event["skill_version"]
                )
                
                if not skill:
                    print(f"⚠️  Skill {event['skill_name']}@{event['skill_version']} not found")
                    continue
                
                # Execute with old skill logic
                result = await skill.execute(...)
                
                print(f"✓ Replayed {event['skill_name']}@{event['skill_version']}")
```

**Example: Skill Evolution**:
```
2026-02-01: User asks "Summarize PR #123"
- Agent uses summarize_pr@1.0.0 (basic summary)
- Event logged: {skill_name: "summarize_pr", skill_version: "1.0.0"}

2026-03-01: Skill upgraded to v1.1.0 (adds diff analysis)
- New users get v1.1.0
- Old events still reference v1.0.0

2036-02-01: Replay conversation from 2026-02-01
- System loads summarize_pr@1.0.0 (not v2.5.0 current version)
- Reproduces exact behavior from 2026
- Result matches original output
```

**Why Skill Versioning is Critical**:
1. **Reproducibility**: "10 years later, reproduce today's decision"
2. **Debugging**: "Why did the agent do X?" → Check skill version used
3. **A/B Testing**: Run v1.0 vs v2.0 side-by-side
4. **Rollback**: If v2.0 has bugs, revert to v1.9
5. **Compliance**: Audit trail shows exact code executed

**Skill Types**:
1. **Read-only** (summarize_pr, ci_status, list_prs)
   - No side effects
   - Can run without approval
   
2. **Write** (create_pr, merge_pr) - Phase 3
   - Has side effects
   - Requires approval or dry-run first

3. **Dangerous** (delete_branch, force_push) - Phase 4+
   - Requires explicit permission
   - Always logged

---

### 2. Context System: Extensible Prompt Structure

**Problem**: How do we build prompts that work today AND support future features (RAG, cross-session memory)?

**Solution**: Prompt has **slots** that can be filled independently.

```
┌─────────────────────────────────────────────────────────────┐
│                      System Prompt                          │
│                                                             │
│  [Slot 1: Available Skills]                                │
│  - Queried from skills_registry                            │
│  - Filtered by current repo                                │
│                                                             │
│  [Slot 2: Repo Context]                                    │
│  - Queried from repos table                                │
│  - Includes: type, access, metadata                        │
│                                                             │
│  [Slot 3: Recent Conversation]                             │
│  - Queried from conversation_events                        │
│  - Last N turns in this session                            │
│                                                             │
│  [Slot 4: User History]                                    │
│  - Placeholder for Phase 3                                 │
│  - Will query: past sessions, preferences, patterns        │
│                                                             │
│  [Slot 5: RAG Context]                                     │
│  - Placeholder for Phase 3                                 │
│  - Will query: relevant docs, code, issues                 │
└─────────────────────────────────────────────────────────────┘
```

**Why slots**:
- Each slot is **independent** - can be added/removed without affecting others
- Each slot is **queryable** - data comes from MatrixOne
- Each slot is **testable** - can mock each slot independently

**Phase 2**: Slots 1-3 implemented  
**Phase 3**: Add slots 4-5 (no prompt redesign needed)

---

### 3. Agent Orchestrator: Decision Flow

**Problem**: How does the agent decide what to do?

**Solution**: Two-phase LLM call:
1. **Phase 1**: Skill selection (LLM decides)
2. **Phase 2**: Skill execution (code executes)

```
User Query
    ↓
Context Manager builds prompt
    ↓
LLM Call #1: "Which skill should I use?"
    ↓
    ├─ Skill selected → Execute skill
    │                   ↓
    │                   LLM Call #2 (if skill needs LLM)
    │                   ↓
    │                   Return result
    │
    └─ No skill → Conversational response
                  ↓
                  LLM Call #2: "Answer conversationally"
                  ↓
                  Return response
```

**Why two phases**:
- **Separation of concerns**: Decision vs execution
- **Auditability**: Can see what agent decided and why
- **Cost control**: Can limit LLM calls per skill
- **Testability**: Can mock skill selection

**Example**:

```
User: "Summarize PR #123"

LLM Call #1 (skill selection):
Input: [System prompt with skills] + "Summarize PR #123"
Output: {"skill_name": "summarize_pr", "parameters": {"pr_number": 123}}
Cost: $0.0001

Skill Execution (summarize_pr):
- Fetch PR from GitHub (no cost)
- LLM Call #2 (summarization):
  Input: "Summarize: [PR content]"
  Output: "This PR adds..."
  Cost: $0.002

Total cost: $0.0021
```

---

### 4. Event System: Full Auditability

**Problem**: How do we debug "Why did the agent do X?"

**Solution**: Every action is an event in MatrixOne.

```
conversation_events table:
┌──────────┬─────────────┬──────────────┬─────────────────────┐
│ event_id │ event_type  │ content      │ metadata            │
├──────────┼─────────────┼──────────────┼─────────────────────┤
│ 1        │ user_query  │ Summarize... │ {}                  │
│ 2        │ llm_call    │ (skill sel)  │ {step: "selection"} │
│ 3        │ skill_exec  │ (fetch PR)   │ {skill: "summ..."}  │
│ 4        │ llm_call    │ (summarize)  │ {step: "execute"}   │
│ 5        │ llm_response│ This PR...   │ {cost: 0.0021}      │
└──────────┴─────────────┴──────────────┴─────────────────────┘
```

**Replay**:
```sql
-- What did the agent see when it made this decision?
SELECT * FROM conversation_events 
WHERE session_id = 'xxx' 
  AND created_at <= '2026-02-10 23:00:00'
ORDER BY created_at;

-- What was the exact prompt?
SELECT content FROM conversation_events
WHERE event_type = 'llm_call' 
  AND metadata->>'step' = 'selection';

-- How much did this session cost?
SELECT SUM(cost) FROM llm_call_logs
WHERE session_id = 'xxx';
```

---

### 5. CLI Design: Minimal but Complete

**Problem**: How do we make the agent usable without building a full UI?

**Solution**: Interactive CLI with 2 commands.

```bash
# Command 1: Interactive chat
$ mo-dev-agent chat --repo 123

Starting chat (session: session-1234)
Repo: matrixone (CODE, READ)

> summarize pr #123
[Agent thinking...]
This PR adds feature X by modifying files A, B, C.
5 files changed, +120 -30 lines.
Cost: $0.002

> what's the ci status?
[Agent thinking...]
All workflows passing. Last run: 2 hours ago.
Cost: $0.0001

> exit
Session saved. Total cost: $0.0021
```

```bash
# Command 2: List skills
$ mo-dev-agent skills --repo 123

Available Skills:
- summarize_pr: Summarize a GitHub PR
  Requirements: CODE repo, READ access
  
- ci_status: Check CI workflow status
  Requirements: CI/CODE repo, READ access
  
- list_prs: List open/closed PRs
  Requirements: CODE repo, READ access
```

**Why this is enough**:
- Interactive mode for exploration
- Skills command for discovery
- All state persisted (can resume later)
- Cost tracking built-in

**What's NOT needed** (yet):
- TUI (terminal UI framework)
- History navigation
- Auto-completion
- Syntax highlighting

These are **nice-to-haves** for Phase 3+.

---

## Data Model: No New Tables

Phase 1 already has everything we need:

```
✅ conversation_events - audit trail
✅ sessions - session state
✅ skills_registry - skill metadata
✅ repos - multi-repo registry
✅ llm_call_logs - cost tracking
✅ tokens - GitHub tokens
```

**Why no new tables**:
- Skills metadata → `skills_registry` (already exists)
- Skill execution logs → `conversation_events` (event_type = 'skill_exec')
- Context queries → existing tables
- CLI state → `sessions` table

**This validates Phase 1 design**: We designed the schema to support future features.

---

## API Design: REST Endpoints

```
POST /query
Request:
{
  "user_id": "alice",
  "session_id": "session-123",
  "query": "summarize pr #123",
  "repo_id": 1
}

Response:
{
  "success": true,
  "response": "This PR adds...",
  "cost": 0.002,
  "metadata": {
    "skill": "summarize_pr",
    "execution_time": 1.5
  }
}
```

```
GET /skills?repo_id=1
Response:
{
  "skills": [
    {
      "name": "summarize_pr",
      "description": "Summarize a GitHub PR",
      "requirements": {
        "repo_types": ["code"],
        "min_access": "read"
      }
    }
  ]
}
```

**Why REST**:
- Simple to implement
- Easy to test
- CLI can use it
- Web UI can use it (Phase 3)

**What's NOT needed** (yet):
- GraphQL
- WebSocket (for streaming)
- gRPC

---

## Success Criteria

**Functional**:
- ✅ User can chat with agent via CLI
- ✅ Agent selects correct skill based on query
- ✅ Skills execute and return results
- ✅ Multi-turn conversations work
- ✅ Cost tracked for every LLM call

**Non-Functional**:
- ✅ All interactions logged to MatrixOne
- ✅ Can replay any conversation
- ✅ Can debug skill selection
- ✅ Skills are testable independently
- ✅ Prompt structure supports future features

**MVP Checkpoint**:
- ✅ Can summarize PRs
- ✅ Can check CI status
- ✅ Can list PRs
- ✅ Usable for daily work

---

## What's NOT in Phase 2

**Deferred to Phase 3+**:
- Sandboxed execution (Docker)
- Write skills (create PR, merge PR)
- RAG (cross-session memory)
- Web UI
- RBAC enforcement
- Multi-tenancy
- Streaming responses
- Skill approval workflow

**Why defer**:
- MVP first - validate core design
- These features depend on Phase 2 working
- Avoid over-engineering

---

## Design Validation

**Question 1**: Can we add RAG without redesigning prompts?  
**Answer**: Yes - add Slot 5 to prompt structure.

**Question 2**: Can we add write skills without changing framework?  
**Answer**: Yes - skills already declare requirements.

**Question 3**: Can we debug "Why did agent do X?"  
**Answer**: Yes - query conversation_events for full trace.

**Question 4**: Can we track costs accurately?  
**Answer**: Yes - llm_call_logs has every call with historical pricing.

**Question 5**: Can we build Web UI without changing backend?  
**Answer**: Yes - REST API is client-agnostic.

**Conclusion**: Design is **extensible** and **future-proof**.

---

## Design Refinements: Edge Cases & Production Concerns

### 1. Error Handling & Recovery

**Problem**: What happens when things go wrong?

**Scenarios**:
```
1. LLM selects non-existent skill
   → Return error to user: "Skill 'xyz' not found. Available: [list]"
   → Log error event to conversation_events
   
2. LLM returns malformed JSON
   → Retry with explicit format instruction (max 2 retries)
   → If still fails: fallback to conversational mode
   → Log parsing error
   
3. GitHub API timeout/rate limit
   → Return error to user with retry suggestion
   → Log API error with status code
   → Don't charge user for failed LLM calls
   
4. Skill execution fails (e.g., PR not found)
   → Skill returns SkillOutput(success=False, error="PR #123 not found")
   → Agent explains error to user
   → Log failure event
```

**Error Handler Architecture**:
```
┌─────────────────────────────────────────────────────────────┐
│                    Error Handler                            │
│                                                             │
│  Catches:                                                   │
│  - LLM errors (timeout, rate limit, malformed response)    │
│  - Skill errors (not found, validation failed, exec failed)│
│  - API errors (GitHub timeout, auth failed)                │
│                                                             │
│  Actions:                                                   │
│  - Log error event to conversation_events                  │
│  - Return user-friendly error message                      │
│  - Suggest recovery action (retry, check permissions)      │
│  - Don't charge for failed operations                      │
└─────────────────────────────────────────────────────────────┘
```

**Error Event Schema**:
```python
{
  "event_type": "error",
  "error_type": "skill_not_found" | "llm_timeout" | "api_error",
  "error_message": "Skill 'xyz' not found",
  "recovery_action": "List available skills",
  "metadata": {
    "attempted_skill": "xyz",
    "available_skills": ["summarize_pr", "ci_status"]
  }
}
```

**Key Principle**: **Fail gracefully, explain clearly, log everything**.

---

### 2. Context Window Management

**Problem**: Prompt can grow unbounded → exceed LLM context limit or explode costs.

**Strategy**: **Adaptive Context Truncation**

```python
class ContextManager:
    MAX_TOKENS = 8000  # Reserve for response
    
    def build_context(self, session_id: str) -> dict:
        # 1. Fixed slots (always included)
        skills = self._format_skills()  # ~500 tokens
        repo = self._format_repo()      # ~200 tokens
        
        # 2. Variable slot (truncate if needed)
        conversation = self._format_conversation(
            session_id,
            max_tokens=self.MAX_TOKENS - 700  # Remaining budget
        )
        
        return {
            "skills": skills,
            "repo": repo,
            "conversation": conversation
        }
    
    def _format_conversation(self, session_id: str, max_tokens: int) -> str:
        """Adaptive truncation strategy"""
        
        # Fetch recent events
        events = self._fetch_recent_events(session_id, limit=50)
        
        # Build conversation from newest to oldest
        lines = []
        token_count = 0
        
        for event in reversed(events):  # Newest first
            line = f"{event.role}: {event.content}"
            line_tokens = self._estimate_tokens(line)
            
            if token_count + line_tokens > max_tokens:
                # Truncation point reached
                if len(lines) < 4:  # Keep at least 2 turns
                    lines.append(line)
                else:
                    lines.append("[Earlier conversation truncated]")
                break
            
            lines.append(line)
            token_count += line_tokens
        
        return "\n".join(reversed(lines))  # Chronological order
```

**Truncation Rules**:
1. **Always keep**: Last 2 turns (1 user + 1 agent)
2. **Prefer recent**: Newer messages over older
3. **Summarize old**: If session > 20 turns, summarize turns 1-10 (Phase 3)
4. **Warn user**: "Earlier conversation truncated" in prompt

**Cost Control**:
```python
# Before calling LLM
estimated_cost = self._estimate_cost(prompt_tokens, max_response_tokens)
if estimated_cost > user_budget:
    return "This query would cost ${estimated_cost:.4f}, exceeding your budget. Simplify?"
```

---

### 3. Skill Selection Scalability

**Problem**: 50 skills in prompt → LLM confused, slow, expensive.

**Solution**: **Two-tier skill selection** (Phase 3, but design now)

```
Phase 2 (MVP): All skills in prompt
- Works for 3-10 skills
- Simple, no extra complexity

Phase 3: Skill retrieval + selection
- Step 1: Retrieve top-K relevant skills (semantic search)
- Step 2: LLM selects from top-K only
```

**Skill Retrieval Design** (Phase 3):
```python
class SkillRetriever:
    """Retrieve relevant skills before LLM selection"""
    
    def retrieve(self, query: str, repo_id: int, top_k: int = 5) -> list[Skill]:
        # 1. Filter by repo requirements
        candidates = self.registry.list_available(repo_id)
        
        # 2. Semantic search (embed query + skill descriptions)
        embeddings = self._embed([query] + [s.description for s in candidates])
        scores = cosine_similarity(embeddings[0], embeddings[1:])
        
        # 3. Return top-K
        top_indices = np.argsort(scores)[-top_k:]
        return [candidates[i] for i in top_indices]
```

**Why defer to Phase 3**:
- MVP has 3 skills → no retrieval needed
- Retrieval adds complexity (embeddings, vector DB)
- Validate core design first

**Design hook** (Phase 2):
```python
# In PromptBuilder
def _format_skills(self, repo_id: int, query: str = None) -> str:
    if query and len(self.registry._skills) > 10:
        # Phase 3: Use retrieval
        skills = self.retriever.retrieve(query, repo_id, top_k=5)
    else:
        # Phase 2: All skills
        skills = self.registry.list_available(repo_id)
    
    return "\n".join([f"- {s.name}: {s.description}" for s in skills])
```

---

### 4. Latency & User Experience

**Problem**: Multi-step flow → slow response → bad UX.

**Latency Breakdown**:
```
User query
  ↓ 50ms   - Context building (DB query)
  ↓ 2s     - LLM skill selection
  ↓ 500ms  - GitHub API call
  ↓ 3s     - LLM summarization
  ↓ 50ms   - Log response
Total: ~6s
```

**Solution 1: Progress Indicators** (Phase 2)
```typescript
// CLI shows detailed progress
> summarize pr #123

[1/4] Building context...
[2/4] Selecting skill... (LLM thinking)
[3/4] Fetching PR from GitHub...
[4/4] Generating summary... (LLM thinking)

This PR adds feature X...
Cost: $0.002 | Time: 5.8s
```

**Solution 2: Streaming** (Phase 3)
```typescript
// Stream LLM responses as they arrive
> summarize pr #123

[Agent] Fetching PR #123...
[Agent] This PR adds feature X by modifying|
[Agent] This PR adds feature X by modifying files A, B, C.|
[Agent] This PR adds feature X by modifying files A, B, C. The main|
...
```

**Solution 3: Caching** (Phase 3)
```python
# Cache GitHub API responses
@cache(ttl=300)  # 5 minutes
def fetch_pr(repo_id: int, pr_number: int):
    return github.get_pr(repo_id, pr_number)

# Cache skill selection for similar queries
@cache(key=lambda query, repo: f"{query[:50]}:{repo}")
def select_skill(query: str, repo_id: int):
    return llm.select_skill(query, repo_id)
```

**Phase 2 Minimum**:
- ✅ Progress indicators in CLI
- ✅ Show "thinking" state
- ✅ Display time + cost after response

**Phase 3 Enhancements**:
- Streaming responses
- Response caching
- Parallel skill execution (if multiple skills needed)

---

### 5. Security & Authentication

**Problem**: Who can do what? How do we verify?

**Authentication Flow**:
```
┌─────────────────────────────────────────────────────────────┐
│  CLI                                                        │
│  - Reads API_KEY from env or ~/.mo-dev-agent/config        │
│  - Sends in Authorization header                           │
└────────────────────┬────────────────────────────────────────┘
                     │ Authorization: Bearer <api_key>
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  FastAPI Backend                                            │
│  - Middleware validates API key                            │
│  - Extracts user_id from key                               │
│  - Checks user permissions                                 │
└────────────────────┬────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────┐
│  MatrixOne                                                  │
│  - users table: user_id, api_key_hash, permissions         │
└─────────────────────────────────────────────────────────────┘
```

**Phase 2 (MVP)**: Single-user mode
```python
# Simple auth: API key in env
API_KEY = os.getenv("MO_DEV_AGENT_API_KEY", "dev-key-123")

@app.middleware("http")
async def auth_middleware(request: Request, call_next):
    if request.url.path.startswith("/api"):
        auth = request.headers.get("Authorization")
        if not auth or auth != f"Bearer {API_KEY}":
            return JSONResponse({"error": "Unauthorized"}, status_code=401)
    
    return await call_next(request)
```

**Phase 3**: Multi-user + RBAC
```python
# users table
CREATE TABLE users (
    user_id VARCHAR(255) PRIMARY KEY,
    api_key_hash VARCHAR(255) NOT NULL,
    role VARCHAR(50),  -- admin, developer, viewer
    permissions JSON,  -- {"repos": [1,2,3], "skills": ["read_only"]}
    created_at TIMESTAMP
);

# Permission check
def check_permission(user_id: str, skill: Skill, repo_id: int) -> bool:
    user = db.get_user(user_id)
    
    # Check repo access
    if repo_id not in user.permissions["repos"]:
        return False
    
    # Check skill access
    if skill.requirements.min_access == "write":
        if "write" not in user.permissions["skills"]:
            return False
    
    return True
```

**Human-in-the-Loop** (Phase 3+):
```python
class WriteSkill(Skill):
    """Skills that modify repos require approval"""
    
    async def execute(self, input: SkillInput) -> SkillOutput:
        # 1. Generate preview
        preview = self._generate_preview(input)
        
        # 2. Request approval
        approval = await self._request_approval(
            user_id=input.user_id,
            action=f"Create PR: {preview.title}",
            details=preview.description
        )
        
        if not approval.approved:
            return SkillOutput(
                success=False,
                error="User declined approval"
            )
        
        # 3. Execute with approval logged
        result = self._execute_write(input)
        self._log_approval(approval.approval_id, result)
        
        return result
```

**Approval Flow**:
```
Agent: "I will create a PR with title 'Fix bug X'. Approve? [y/n]"
User: "y"
Agent: "PR created: https://github.com/..."
```

**Security Checklist** (Phase 2):
- ✅ API key authentication
- ✅ GitHub tokens stored encrypted (Phase 1)
- ✅ Read-only skills only (no writes)
- ✅ All actions logged to conversation_events

**Security Checklist** (Phase 3+):
- Multi-user RBAC
- Human-in-the-loop for write operations
- Rate limiting per user
- Audit log for sensitive operations

---

## Updated Success Criteria

**Functional**:
- ✅ User can chat with agent via CLI
- ✅ Agent selects correct skill based on query
- ✅ Skills execute and return results
- ✅ Multi-turn conversations work
- ✅ Cost tracked for every LLM call
- ✅ **Errors handled gracefully with clear messages**
- ✅ **Progress indicators show agent state**

**Non-Functional**:
- ✅ All interactions logged to MatrixOne
- ✅ Can replay any conversation
- ✅ Can debug skill selection
- ✅ Skills are testable independently
- ✅ Prompt structure supports future features
- ✅ **Context truncation prevents token overflow**
- ✅ **Response time < 10s for typical queries**
- ✅ **API key authentication enforced**

---

## References

- [Roadmap](../../memo/docs/mo-dev-agent/roadmap.md)
- [Phase 1 Summary](./PHASE1_SUMMARY.md)
- [Vision and Mission](./vision-and-mission.md)
- [GitHub Integration](./github-integration.md)
- [LLM Integration](./llm-integration.md)
