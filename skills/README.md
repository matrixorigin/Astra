# Skills

Developer and debugging skills for the astra agent platform. These skills help
coding agents (astra CLI, Claude Code, or other Agent Skills-compatible tools)
understand, analyze, and debug astra sessions.

## Agent Skills Compatibility

Astra skills follow the [Agent Skills](https://agentskills.io) open standard —
the same format used by Claude Code. This means:

- **Astra reads Claude Code skills**: `.claude/skills/` directories are
  automatically discovered alongside `.astra/skills/`. No copying needed.
- **Claude Code reads astra skills**: Skills in this repo work in Claude Code
  when placed in `.claude/skills/` (or symlinked).
- **Shared SKILL.md format**: YAML frontmatter (`name`, `description`,
  `allowed-tools`, etc.) + Markdown body.

### Cursor IDE

Cursor loads project skills from **`.cursor/skills/<name>/SKILL.md`**. This repo keeps a single source of truth under **`skills/`** at the repository root and exposes it to Cursor via a symlink:

- `.cursor/skills` → `../skills`

If skills still do not appear, reload the window (**Developer: Reload Window**) or ensure you opened the repository root as the workspace folder.

### Search order

Astra discovers skills from these paths (high → low priority):

1. Walk-up from cwd: `{ancestor}/.astra/skills/`
2. Walk-up from cwd: `{ancestor}/.claude/skills/`
3. `{cwd}/skills/` (project-level, legacy)
4. `~/.astra/skills/` (user global)
5. `~/.claude/skills/` (Claude Code user global)

When names collide, the higher-priority path wins.

### Using Claude Code skills in astra

If a project already has `.claude/skills/`, astra picks them up automatically.
To use a Claude Code skill as a starting point for an astra-specific version:

```bash
# Copy and customize
cp -r .claude/skills/some-skill .astra/skills/some-skill
# Edit .astra/skills/some-skill/SKILL.md to add astra-specific features
```

### Frontmatter compatibility

| Field | Agent Skills standard | Claude Code | Astra |
|-------|----------------------|-------------|-------|
| `name` | ✅ required | ✅ | ✅ |
| `description` | ✅ required | ✅ | ✅ |
| `allowed-tools` / `allowed_tools` | ✅ optional | ✅ | ✅ (both forms) |
| `user-invocable` / `user_invocable` | — | ✅ | ✅ (both forms) |
| `when_to_use` | — | — | ✅ astra extension |
| `arguments` | — | via `$ARGUMENTS` | ✅ structured |
| `context: fork` | — | ✅ | ✅ |
| `disable-model-invocation` | — | ✅ | — (use `user_invocable: false`) |
| `hooks` | — | ✅ | ✅ |
| `paths` | — | ✅ | ✅ |
| `model` | — | ✅ | ✅ |
| `effort` | — | ✅ | ✅ |

## Skills in this repo

| Skill | Purpose |
|-------|---------|
| `analyze_session` | Diagnostic analysis and debugging of astra sessions (includes stall/escalation forensics) |
| `audit_cloud_sync` | Audit edge-cloud sync: events, learning, checkpoints, tasks |
| `batch_parallel` | Execute independent tasks in parallel using git worktrees |
| `evaluate_session` | Evaluate session performance metrics and optimization |
| `optimize_prompt` | Analyze and optimize LLM prompt assembly and token usage |
| `review_changes` | Context-aware code review with git diffs + code intelligence |
| `trace_delegation` | Trace multi-agent delegation flows and verification gates |
| `verify_task` | Verify task completion using the 8-type verification engine |

## Skill structure

Each skill is a directory with at minimum a `SKILL.md`:

```
skill-name/
├── SKILL.md          # Required: frontmatter + instructions
├── manifest.yaml     # Optional: astra registry metadata
├── metadata.json     # Optional: input/output schemas, examples
└── README.md         # Optional: human-readable docs
```
