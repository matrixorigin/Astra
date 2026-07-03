---
name: optimize-prompt
description: "Analyze and reduce Astra prompt/context bloat across system prompt, tool surface, history, skills, and budget pressure using session digest plus prompt checkpoints."
user_invocable: true
when_to_use: "When the user wants to reduce LLM prompt size, find token waste, debug budget pressure, tune tool schema visibility, or optimize skill/context assembly."
arguments:
  - name: TARGET
    description: "Session ID, debug JSON path, or 'this'/'last'. Omit for most recent."
    required: false
  - name: COMPONENT
    description: "Focus: system, tools, history, skills, budget, or all. Default: all."
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---

# Optimize Prompt

Optimize only from evidence. Prefer exact prompt checkpoints and digest metrics;
use byte/token estimates only when exact tokenizer data is unavailable and label them
as estimates.

## Task

$ARGUMENTS

## Phase 1: Load Evidence

Start with digest:

```bash
astra journal digest last --format json
astra journal digest <SESSION_ID> --format json
astra journal digest <SESSION_ID> --focus summary --format json
```

Then locate exact prompt payloads when needed:

```bash
ls -lt ~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json 2>/dev/null | head
ls -lt /tmp/debug-*-turn*-full.json 2>/dev/null | head
```

Evidence priority:

| Source               | Use                                                                       |
| -------------------- | ------------------------------------------------------------------------- |
| Heavy checkpoint     | Exact message array sent to the model                                     |
| Debug full turn dump | Full turn prompt/tool snapshot when present                               |
| Journal digest       | Per-turn tokens, visible tools, selected skills, budget pressure, latency |
| Source code          | Owner and intended assembly rule                                          |

Do not optimize from vague impressions such as "prompt feels long".

## Phase 2: Map Component Owner

| Component                           | Owner                                                                                                                |
| ----------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| System prompt strings/builders      | `rust/crates/runtime/src/prompts/`, `rust/crates/astra-prompts/src/`                                                 |
| Context budget and token estimation | `rust/crates/runtime/src/prompts/context.rs`, runtime turn budget modules                                            |
| Tool schema surface                 | `rust/crates/runtime/src/tool_registry/`, `rust/crates/runtime/src/capabilities.rs`, `astra-turn-core::tool_surface` |
| Skill instructions/listing          | `rust/crates/astra-prompts/src/skills.rs`, `rust/crates/astra-skills/src/`, `.claude/skills/`, `.agent/skills/`      |
| Learning/context pipeline           | `rust/crates/astra-pipeline/src/`                                                                                    |
| Turn telemetry                      | `rust/crates/services/src/session_journal.rs`, `rust/crates/astra-cli/src/cli/journal_digest.rs`                     |

## Phase 3: Measure Waste

System:

- Identify repeated static sections, task-irrelevant rules, or conditional sections emitted without the matching capability/tool.
- Verify source before recommending removal; many sections are cache-stable and cheap after cache hits.

Tools:

- Compare `visible_tools_count`, `tools_used_count`, `activated_tools_count`, and actual tool calls.
- Waste signal: many visible tools plus low usage across repeated turns, or deferred tools activated but never called.
- Owner is tool surface/capability metadata, not ad hoc prompt text.

Skills:

- Check `selected_skills` and the actual user task.
- Waste signal: selected skill unrelated to the task or large skill instructions repeatedly injected.
- Fix by tightening trigger/description or deleting low-ROI skill content.

History/tool results:

- Inspect message sizes in heavy checkpoints.
- Waste signal: repeated file reads, huge tool outputs retained across turns, stale reasoning/tool results after compaction.

Budget:

- Use `budget_pressure`, `context_ms`, `ttft_ms`, compaction events, and turn token counts.
- Healthy sessions show pressure relief after compaction; sustained high pressure after compaction needs prompt/history/tool-result work.

Optional checkpoint size scan:

```bash
python3 - <<'PY'
import json, sys
path = sys.argv[1]
msgs = json.load(open(path, encoding="utf-8"))
for i, m in enumerate(msgs):
    role = m.get("role", "?")
    size = len(json.dumps(m, ensure_ascii=False))
    content = m.get("content", "")
    preview = content[:80].replace("\n", " ") if isinstance(content, str) else type(content).__name__
    print(f"{i:03d} {role:10s} {size:8d} bytes {preview}")
PY
```

## Phase 4: Recommend Changes

Every recommendation needs:

- observed metric or checkpoint evidence;
- owning file/module;
- expected effect;
- verification command or digest metric to re-check.

Avoid:

- invented exact token savings;
- removing safety-critical instructions just because they are large;
- adding another prompt layer when the real issue is tool/skill selection metadata.

## Output Contract

```text
Observed:
- session=<id>, turns=<n>, pressure=<pattern>, visible_tools=<pattern>, selected_skills=<pattern>

Top savings:
1. <component> - <evidence> - owner=<file> - expected effect=<bounded estimate>
2. ...

Do not change:
- <large but necessary/cache-stable section, if any>

Verify:
- <digest/checkpoint/test command>
```

```skill-diagnosis
{
  "schema_version": 2,
  "skill": "optimize_prompt",
  "cause": "budget_pressure",
  "headline": "system prompt and tool surface contribute 60% of token budget with low tool utilization",
  "findings": ["visible_tools_count=45 but only 3 tools used across 12 turns"],
  "recommended_action": "defer rarely-used tools and trim system prompt static sections",
  "success_criteria": [
    {
      "metric": "budget_pressure",
      "operator": "lte",
      "threshold": 0.85,
      "window_turns": 3,
      "description": "sustained budget pressure drops below threshold"
    }
  ],
  "source": "real_skill"
}
```
