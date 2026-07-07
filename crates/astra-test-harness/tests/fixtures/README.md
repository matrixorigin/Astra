# Real-journal wire-shape fixtures

These jsonl fixtures mirror the shape of real `~/.astra/sessions/<id>.jsonl`
files as captured on 2026-05-01 across multiple sessions. They are
**synthetic** — no real session id, no real token counts, no real prompts —
but structurally identical to what the astra runtime emits on disk.

## Why

Four review rounds caught harness code that made optimistic assumptions
about journal wire shape without ever checking against a real file. See
review rounds R2, R3, R4 for specifics. Fixtures exist so:

1. **TDD before writing new session criteria**: run the relevant
   fixture through your new logic BEFORE claiming it works.
2. **Regression lock**: if the runtime ever changes the wire shape, the
   fixture-driven tests fail loudly instead of silently mismatching real
   journals.
3. **Ground-truth comparison**: each fixture has an `expected.jsonl` or
   inline expected values in the test that were derived by `jq` over the
   original real file.

## Refreshing a fixture

If the runtime's wire shape changes, update the fixture AND the matching
test. Do NOT update only the code — the whole point of these fixtures is
to surface drift as a test failure.

To confirm a fixture still matches reality:

```bash
# Types seen in a real journal
grep -oE '"type":"[a-z_]+"' ~/.astra/sessions/*.jsonl | sort -u

# Tool names seen via llm_round.tool_calls nesting
python3 -c "
import json
for line in open('<real-jsonl>'):
    d = json.loads(line)
    if d.get('type') == 'llm_round' and d.get('tool_calls'):
        for c in d['tool_calls']:
            print(c['name'])
" | sort -u
```

Compare against `fixture_realistic_legacy.jsonl` — if the shapes drift,
update the fixture, update the test's expected values, document the
delta in the commit message.
