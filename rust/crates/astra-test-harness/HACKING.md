# Hacking on astra-test-harness

## TL;DR for adding a session-based criterion

Before you write code: **verify what real journals actually contain**.
Four independent review rounds caught harness code making optimistic
assumptions about journal wire shape. Each time, the code looked right
in isolation but disagreed with the runtime's actual output — invisible
until someone compared against a real file.

The rule: whenever you add or change any criterion that reads
`SessionCapture`, walk this checklist first.

### 1. Inventory the real shapes

```bash
# Every event type across all local legacy journals.
grep -oE '"type":"[a-z_]+"' ~/.astra/sessions/*.jsonl \
  | sort -u
```

```bash
# Every event type in the step-events layout.
for d in ~/.astra/sessions/*/; do
  if [ -f "$d/step_events.jsonl" ]; then
    grep -oE '"event_type":"[A-Za-z]+"' "$d/step_events.jsonl"
  fi
done | sort -u
```

Your criterion's `event_type` MUST appear in at least one of those
lists. If it doesn't, the criterion will fail on every real session —
that's the exact bug R4 caught.

### 2. Inventory nested fields

Many tool names live inside `llm_round.tool_calls[]`, not as top-level
events:

```bash
python3 -c "
import json
for line in open('<real-jsonl>'):
    d = json.loads(line)
    if d.get('type') == 'llm_round' and d.get('tool_calls'):
        for c in d['tool_calls']:
            print(c['name'])
" | sort -u
```

Step-events equivalent: tool names live inside
`ToolCallCompleted.payload.tool_name`.

### 3. Add a fixture, not just a unit test

Copy the relevant pattern into `tests/fixtures/fixture_realistic_*.jsonl`
and add an integration test under `tests/real_journal_wire_shape.rs`
that asserts ground truth. This is the TDD-red step: if the assertion
fails, the code is wrong. If the assertion passes, commit the fixture
as the regression guard.

### 4. Refreshing a fixture when the runtime changes

If the runtime changes the wire shape, the existing fixture tests MUST
fail. That is the point — silent drift is the failure mode. When that
happens:

1. Confirm the runtime change is intentional (read the PR that changed
   `astra-turn-core::session_journal` or the step-events writer).
2. Update `tests/fixtures/fixture_realistic_*.jsonl` to match.
3. Update the test's ground truth inline values.
4. Document the delta in your commit message.

Do NOT update only the code — that's how the four review rounds
happened.

## Common pitfalls caught in review

| Round | Pitfall | What it looked like |
|------|---------|---------------------|
| R2 | `ForkCacheEvent` field `outcome`, not `class` | Criterion read `class`; every real event had `outcome`. Silent zero matches. |
| R3 | `fork_cache_outcome: [hit]` on a case that SHOULDN'T emit an event | Criterion FAILed the "no event" success path; judger was gated on the FAIL so it never ran. |
| R4 | `tool_invocation` doesn't exist; tool calls nest in `llm_round.tool_calls[]` | Criterion returned empty on every real session. Two shipped cases failed every run. |
| R4 | Loader returned early on legacy file; step_events dead | `ToolCallCompleted` unmatchable on any session that also had legacy output (all of them). |

The common pattern: **comments describe intended behavior, code
drifted from wire shape**. The fixture-driven integration tests
(`tests/real_journal_wire_shape.rs`) are the only thing that would
have caught these at PR time. Run them. Extend them when you add
criteria.

## Stderr prefix discipline

Every `eprintln!` in this crate MUST begin with `[astra-test]`. The
`every_eprintln_uses_harness_prefix` pin test enforces this by
grepping the source tree. Reasoning: a case writing
`stderr_matches { pattern: '^\[astra-test\]' }` should match ALL
harness self-log lines and NONE of the subprocess's output. Breaking
this invariant means a case could inadvertently match our own
warnings.

## Wire-format types are `#[non_exhaustive]`

`CaseRunReport`, `RunOutcome`, `JudgerScore`, `DigestArtifact`,
`SessionCapture` all serialize to JSON as part of `--format json`
output. They're marked `#[non_exhaustive]` so fields can be added
without a SemVer break. Consequences:

- In-crate construction uses struct literals (unaffected).
- External tests / embedders use `RunOutcome::new(model).with_*()`
  setters or `Default` + field-level mutation.
- Never pattern-match them without `..`.

## Running the suite end-to-end

```bash
# Unit + integration tests (no subprocess to astra)
cargo test -p astra-test-harness

# Live suite against a running astra server
./rust/target/release/astra-test \
    --suite rust/crates/astra-test-harness/cases \
    --models qwen-flash \
    --no-judger

# With the Makefile shortcut
make test-harness MODELS=qwen-flash
```
