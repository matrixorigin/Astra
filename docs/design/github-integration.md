# GitHub Integration Design

**Version**: 1.0  
**Status**: Draft  
**Last Updated**: 2026-02-10

## 1. Vision and Goals

### 1.1 Vision

Enable mo-dev-agent to act as a **virtual employee** that can perform nearly all GitHub operations that a human developer would do, with:
- **Full state tracking** in MatrixOne (reproducibility)
- **Fine-grained permission control** (security)
- **Risk mitigation** (safety)
- **Multi-repository coordination** (productivity)

### 1.2 Goals

1. **Support Vision**: Enable all capabilities described in vision-and-mission.md
2. **Virtual Employee**: Cover ~95% of daily GitHub operations
3. **Permission Control**: Role-based access with audit trail
4. **Risk Control**: Rate limiting, approval workflows, rollback
5. **State in MatrixOne**: All operations logged as events
6. **Sensitive Data**: Secure token management (Vault integration)

## 2. Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                        mo-dev-agent                          │
├─────────────────────────────────────────────────────────────┤
│  Skills Layer                                                │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ PR Review    │  │ Issue Triage │  │ CI Analysis  │ ...  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘      │
│         │                  │                  │              │
├─────────┴──────────────────┴──────────────────┴─────────────┤
│  GitHub Client Layer                                         │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  GitHubClient (unified interface)                    │   │
│  │  - Token resolution (priority fallback)              │   │
│  │  - Rate limiting (per-token tracking)                │   │
│  │  - Error handling (401 → deactivate token)           │   │
│  │  - Audit logging (all API calls → events)            │   │
│  └──────────────────────────────────────────────────────┘   │
├─────────────────────────────────────────────────────────────┤
│  Repository Management                                       │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ RepoRegistry │  │TokenResolver │  │PermissionMgr │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
├─────────────────────────────────────────────────────────────┤
│  MatrixOne (State Store)                                     │
│  - repos, tokens, github_operations, rate_limits            │
│  - conversation_events (all operations logged)               │
└─────────────────────────────────────────────────────────────┘
```

## 3. Core Capabilities

### 3.1 Repository Operations

#### 3.1.1 Read Operations
- **List repositories**: User/org repos, with filters
- **Get repository**: Metadata, stats, topics
- **List branches**: All branches, default branch
- **Get file content**: Single file, directory tree
- **Get commit**: Commit details, diff, files changed
- **List commits**: Commit history, by author/date
- **Search code**: Code search across repos

#### 3.1.2 Write Operations (Require Approval)
- **Create repository**: New repo with template
- **Update repository**: Settings, topics, description
- **Delete repository**: With confirmation
- **Create branch**: From commit/branch
- **Delete branch**: With protection check
- **Create/update file**: Commit file changes
- **Delete file**: Remove file with commit

### 3.2 Pull Request Operations

#### 3.2.1 Read Operations
- **List PRs**: Open/closed/merged, by author/label
- **Get PR**: Details, files changed, commits
- **Get PR reviews**: All reviews, by reviewer
- **Get PR comments**: Review comments, issue comments
- **Get PR checks**: CI status, required checks
- **Get PR diff**: Unified diff, patch format

#### 3.2.2 Write Operations
- **Create PR**: From branch, with template
- **Update PR**: Title, body, labels, assignees
- **Close PR**: Without merge
- **Merge PR**: Merge/squash/rebase strategies
- **Request review**: Assign reviewers
- **Submit review**: Approve/request changes/comment
- **Add comment**: Review comment, issue comment
- **React to comment**: Emoji reactions

### 3.3 Issue Operations

#### 3.3.1 Read Operations
- **List issues**: Open/closed, by label/assignee
- **Get issue**: Details, comments, timeline
- **Search issues**: Advanced search queries

#### 3.3.2 Write Operations
- **Create issue**: With template, labels
- **Update issue**: Title, body, labels, assignees
- **Close issue**: With reason
- **Reopen issue**: Reactivate closed issue
- **Add comment**: Issue comment
- **Add label**: Apply labels
- **Assign**: Assign to user

### 3.4 CI/CD Operations

#### 3.4.1 Workflow Operations
- **List workflows**: All workflows in repo
- **Get workflow**: Workflow details, runs
- **List workflow runs**: By status/branch/event
- **Get workflow run**: Run details, jobs, logs
- **Download logs**: Workflow run logs
- **Re-run workflow**: Trigger re-run

#### 3.4.2 Actions Operations
- **Trigger workflow**: Manual dispatch
- **Cancel workflow run**: Stop running workflow
- **Get job logs**: Individual job logs

### 3.5 Release Operations

#### 3.5.1 Read Operations
- **List releases**: All releases, latest
- **Get release**: Release details, assets

#### 3.5.2 Write Operations
- **Create release**: Tag, notes, assets
- **Update release**: Edit release notes
- **Delete release**: Remove release

### 3.6 Collaboration Operations

#### 3.6.1 Team Operations
- **List teams**: Org teams
- **Get team**: Team details, members
- **List team repos**: Repos accessible to team

#### 3.6.2 Notification Operations
- **List notifications**: Unread notifications
- **Mark as read**: Mark notification read

## 4. Token Management

### 4.1 Token Types and Scopes

```sql
CREATE TABLE tokens (
  token_id            VARCHAR(64) PRIMARY KEY,
  type                VARCHAR(32) NOT NULL,  -- 'repo' | 'llm'
  provider            VARCHAR(64),           -- 'github' | 'openai'
  scope_user_id       VARCHAR(255),          -- User-scoped token
  scope_tenant_id     VARCHAR(255),          -- Tenant-scoped token
  scope_repo          VARCHAR(255),          -- Repo-specific token
  secret_ref          VARCHAR(255),          -- Vault path (PREFERRED)
  encrypted_value     TEXT,                  -- Fallback if no Vault
  is_active           BOOLEAN DEFAULT TRUE,
  expires_at          TIMESTAMP,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  rotation_policy     VARCHAR(64),           -- 'manual' | '90d'
  metadata            JSON,                  -- {scopes: ['repo', 'workflow']}
  
  INDEX idx_tokens_scope_user (scope_user_id, type),
  INDEX idx_tokens_scope_tenant (scope_tenant_id, type)
);
```

### 4.2 Token Resolution Priority

**Priority Chain** (NULL token_id triggers fallback):

```
1. Repo-specific token (repos.token_id)
   ↓ (if NULL or inactive)
2. User default token (scope_user_id, no scope_repo)
   ↓ (if not found or inactive)
3. Tenant default token (scope_tenant_id, no scope_repo)
   ↓ (if not found or inactive)
4. Global fallback (if config.allow_global_repo_token = true)
   ↓ (if not found)
5. Return None (operation fails)
```

**Example**: New tenant with no tokens
- `repos.token_id = NULL` (no repo-specific token)
- No user token found
- No tenant token found
- Global fallback enabled → Use global token
- Operation succeeds

### 4.3 Token Security

**Storage**:
- **Preferred**: Vault integration (`secret_ref = "vault://github/token1"`)
- **Fallback**: Encrypted in database (`encrypted_value`, key from env)
- **Never**: Plain text in logs or API responses

**Rotation**:
- Manual rotation: Admin updates token
- Automatic rotation: `rotation_policy = '90d'` triggers job
- On rotation: Old token deactivated, new token created

**Failure Handling**:
- **401 Unauthorized**: Deactivate token, alert admin, fallback to next priority
- **403 Forbidden**: Log permission error, do not deactivate
- **Rate limit**: Track per-token, switch to backup token

## 5. Permission Control

### 5.1 Access Scopes

```sql
CREATE TABLE repos (
  repo_id             VARCHAR(64) PRIMARY KEY,
  repo_url            VARCHAR(500) NOT NULL,
  repo_type           VARCHAR(50) NOT NULL,  -- 'code' | 'ci' | 'tester' | 'docs'
  owner_id            VARCHAR(255) NOT NULL,
  owner_type          VARCHAR(50) NOT NULL,  -- 'user' | 'tenant'
  repo_group          VARCHAR(255),          -- Logical grouping
  token_id            VARCHAR(64),           -- NULL = use priority fallback
  access_scope        VARCHAR(50) NOT NULL,  -- 'read' | 'write' | 'admin'
  metadata            JSON,
  is_active           BOOLEAN DEFAULT TRUE,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  UNIQUE KEY idx_repo_url_owner (repo_url, owner_id),
  INDEX idx_owner (owner_id, owner_type),
  INDEX idx_repo_type (repo_type),
  INDEX idx_repo_group (repo_group)
);
```

**Note**: `token_id` is nullable to support priority fallback chain.

### 5.2 Operation Permissions

| Operation | Read | Write | Admin |
|-----------|------|-------|-------|
| List repos | ✓ | ✓ | ✓ |
| Get file | ✓ | ✓ | ✓ |
| List PRs | ✓ | ✓ | ✓ |
| Create PR | ✗ | ✓ | ✓ |
| Merge PR | ✗ | ✓ | ✓ |
| Create issue | ✗ | ✓ | ✓ |
| Trigger workflow | ✗ | ✗ | ✓ |
| Delete repo | ✗ | ✗ | ✓ |

### 5.3 Approval Workflows

**High-Risk Operations** (require approval):
- Merge PR to protected branch
- Delete branch
- Trigger production workflow
- Create/delete repository

**Approval Process**:
1. Agent proposes operation → creates `approval_request`
2. Human reviewer approves/rejects
3. On approval: Execute operation, log to `github_operations`
4. On rejection: Log rejection reason, notify agent

```sql
CREATE TABLE approval_requests (
  request_id          VARCHAR(64) PRIMARY KEY,
  operation_type      VARCHAR(100) NOT NULL,  -- 'merge_pr' | 'delete_branch'
  repo_id             VARCHAR(64) NOT NULL,
  operation_params    JSON NOT NULL,          -- {pr_number: 123, strategy: 'squash'}
  requested_by        VARCHAR(255) NOT NULL,  -- agent_id
  requested_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  status              VARCHAR(50) DEFAULT 'pending',  -- 'pending' | 'approved' | 'rejected'
  reviewed_by         VARCHAR(255),
  reviewed_at         TIMESTAMP,
  review_comment      TEXT,
  
  INDEX idx_status (status, requested_at)
);
```

## 6. Risk Control

### 6.1 Rate Limiting

**GitHub API Limits**:
- **Authenticated**: 5,000 requests/hour per token
- **Search API**: 30 requests/minute
- **GraphQL**: 5,000 points/hour

**Strategy**:
```sql
CREATE TABLE rate_limits (
  token_id            VARCHAR(64) PRIMARY KEY,
  requests_remaining  INT NOT NULL,
  reset_at            TIMESTAMP NOT NULL,
  last_updated        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_reset_at (reset_at)
);
```

**Implementation**:
- Track remaining requests per token
- When limit reached: Switch to backup token or wait
- Alert when approaching limit (< 100 remaining)

### 6.2 Operation Logging

**All GitHub operations logged**:
```sql
CREATE TABLE github_operations (
  operation_id        VARCHAR(64) PRIMARY KEY,
  event_id            VARCHAR(64) NOT NULL,  -- Link to conversation_events
  repo_id             VARCHAR(64) NOT NULL,
  operation_type      VARCHAR(100) NOT NULL,
  operation_params    JSON NOT NULL,
  token_id            VARCHAR(64) NOT NULL,
  is_dry_run          BOOLEAN DEFAULT FALSE, -- Distinguish simulation from real execution
  status              VARCHAR(50) NOT NULL,  -- 'success' | 'failed' | 'pending'
  response_code       INT,
  response_body       JSON,
  error_message       TEXT,
  executed_at         TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  duration_ms         INT,
  
  INDEX idx_event_id (event_id),
  INDEX idx_repo_id (repo_id, executed_at),
  INDEX idx_status (status),
  INDEX idx_dry_run (is_dry_run)
);
```

**Audit Trail**:
- Who: `event_id` → `conversation_events.user_id`
- What: `operation_type`, `operation_params`
- When: `executed_at`
- Where: `repo_id` → `repos.repo_url`
- How: `token_id`, `response_code`, `is_dry_run`
- Why: `event_id` → `causal_chain_id` (full context)

**Replay Filtering**:
- On replay: Filter `WHERE is_dry_run = FALSE` to exclude simulations
- Ensures replay reproduces only real execution chain

### 6.3 Rollback Capability

**Rollback Strategies**:
1. **PR merge**: Revert commit
2. **Branch delete**: Restore from snapshot (if enabled)
3. **File change**: Revert commit
4. **Issue close**: Reopen issue

**Implementation**:
- Store operation metadata in `github_operations`
- Provide `rollback_operation(operation_id)` API
- Log rollback as new operation

### 6.4 Safety Checks

**Pre-execution Checks**:
- **Permission check**: Verify access_scope
- **Rate limit check**: Ensure quota available
- **Branch protection**: Check if branch is protected
- **Approval check**: Verify approval for high-risk ops
- **Dry-run mode**: Simulate operation without execution

**Dry-run Implementation**:
```python
def execute_github_operation(operation, dry_run=False):
    """Execute GitHub operation with dry-run support."""
    if dry_run:
        # Simulate operation
        simulated_result = simulate(operation)
        log_to_github_operations(
            operation, 
            is_dry_run=True,
            status='success',
            response_body=simulated_result
        )
        return simulated_result
    else:
        # Real execution
        actual_result = github_client.execute(operation)
        log_to_github_operations(
            operation,
            is_dry_run=False,
            status='success',
            response_body=actual_result
        )
        return actual_result
```

**Replay Behavior**:
- Dry-run operations marked with `is_dry_run=TRUE`
- On replay: Filter `WHERE is_dry_run = FALSE`
- Ensures replay reproduces only real execution chain, not simulations

## 7. Skills and Use Cases

### 7.1 PR Review Skill

**Capability**: Automated PR review with context

**Workflow**:
1. **Trigger**: Webhook on PR opened/updated
2. **Fetch PR**: Get PR details, files changed, commits
3. **Analyze**: 
   - Code quality (linting, complexity)
   - Test coverage (from CI)
   - Breaking changes (API diff)
   - Security issues (dependency scan)
4. **Context**: Load related issues, previous PRs
5. **Review**: Submit review comment with suggestions
6. **Log**: All operations → `conversation_events`

**Permissions**: `write` (to submit review)

**Documentation Storage**: 
- Skill documentation stored in `skills_registry.documentation` (Markdown format)
- On replay: `github_operations.operation_id` → `conversation_events.event_id` → `skills_registry` (by version)
- Enables audit: "Why did the agent review this way on 2026-02-10?" → Retrieve skill documentation from that timestamp

### 7.2 Issue Triage Skill

**Capability**: Automated issue triage and labeling

**Workflow**:
1. **Trigger**: Webhook on issue opened
2. **Fetch issue**: Get issue details, body, author
3. **Classify**:
   - Bug vs Feature vs Question
   - Priority (P0/P1/P2)
   - Component (based on keywords)
4. **Label**: Apply labels
5. **Assign**: Assign to team member (if rules match)
6. **Comment**: Add triage summary
7. **Log**: All operations → `conversation_events`

**Permissions**: `write` (to add labels/comments)

### 7.3 CI Failure Analysis Skill

**Capability**: Diagnose CI failures across repos

**Workflow**:
1. **Trigger**: Webhook on workflow run failed
2. **Fetch workflow**: Get workflow run, jobs, logs
3. **Correlate**:
   - Get commit from code repo
   - Get test results from tester repo
   - Get previous runs from CI repo
4. **Analyze**:
   - Parse error logs
   - Identify flaky tests
   - Find related issues
5. **Report**: Comment on PR with diagnosis
6. **Log**: Cross-repo operations → `conversation_events`

**Permissions**: `read` (code, ci, tester repos)

### 7.4 Release Automation Skill

**Capability**: Automated release creation

**Workflow**:
1. **Trigger**: Manual command or schedule
2. **Validate**:
   - All tests passing
   - No open P0 issues
   - Changelog updated
3. **Create release**:
   - Generate release notes (from PRs)
   - Tag commit
   - Upload assets
4. **Notify**: Post to Slack/Discord
5. **Log**: All operations → `conversation_events`

**Permissions**: `admin` (to create release)

### 7.5 Cross-Repo Sync Skill

**Capability**: Sync changes across related repos

**Workflow**:
1. **Trigger**: PR merged in code repo
2. **Detect changes**: API changes, config changes
3. **Propagate**:
   - Update CI repo (workflow changes)
   - Update docs repo (API docs)
   - Update tester repo (test cases)
4. **Create PRs**: In each affected repo
5. **Log**: All operations → `conversation_events`

**Permissions**: `write` (all repos in group)

## 8. Implementation Phases

### Phase 1: Foundation (Current)
- ✅ Repository registry
- ✅ Token resolver
- ✅ Permission model
- ⏳ GitHub client (basic)

### Phase 2: Core Operations
- Read operations (repos, PRs, issues)
- Write operations (comments, labels)
- Rate limiting
- Operation logging

### Phase 3: Advanced Operations
- PR review automation
- Issue triage
- CI analysis
- Approval workflows

### Phase 4: Cross-Repo Coordination
- Multi-repo skills
- Dependency tracking
- Sync automation

### Phase 5: Intelligence
- Context-aware reviews
- Predictive triage
- Failure pattern detection
- Auto-remediation

## 9. Security Considerations

### 9.1 Token Security
- **Never log tokens**: Mask in logs, audit trail
- **Vault integration**: Preferred storage method
- **Rotation**: Automatic rotation every 90 days
- **Least privilege**: Minimal scopes per token

### 9.2 Access Control
- **RBAC**: Role-based access (read/write/admin)
- **Audit trail**: All operations logged
- **Approval workflows**: High-risk operations require approval
- **Tenant isolation**: User/tenant-scoped tokens

### 9.3 Data Privacy
- **PII handling**: Mask sensitive data in logs
- **Compliance**: GDPR, SOC2 considerations
- **Retention**: Configurable data retention policy

## 10. Monitoring and Observability

### 10.1 Metrics
- **Operation success rate**: By operation type, repo
- **Token usage**: Requests per token, rate limit hits
- **Approval latency**: Time from request to approval
- **Error rate**: By error type, repo

### 10.2 Alerts
- **Token expiring**: < 7 days to expiration
- **Rate limit**: < 100 requests remaining
- **Operation failure**: > 5% failure rate
- **Approval backlog**: > 10 pending requests

### 10.3 Dashboards
- **Operations dashboard**: Real-time operation stats
- **Token dashboard**: Token health, usage
- **Repo dashboard**: Per-repo activity, health
- **Skill dashboard**: Skill execution stats

## 11. Future Enhancements

### 11.1 Advanced Features
- **GraphQL API**: More efficient queries
- **Webhook management**: Auto-configure webhooks
- **GitHub Apps**: Native GitHub App integration
- **Fine-grained tokens**: Per-repo, per-operation tokens

### 11.2 Intelligence
- **Learning from feedback**: Improve reviews over time
- **Anomaly detection**: Detect unusual patterns
- **Predictive analytics**: Predict CI failures
- **Auto-remediation**: Fix common issues automatically

### 11.3 Integrations
- **Slack/Discord**: Notifications, commands
- **Jira**: Issue sync
- **Datadog**: Metrics export
- **PagerDuty**: Incident management

---

**Document Status**: Draft  
**Next Review**: After Phase 2 implementation  
**Owner**: mo-dev-agent team
