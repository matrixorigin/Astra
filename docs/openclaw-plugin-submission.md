# OpenClaw Community Plugin Submission Notes

Target doc: <https://docs.openclaw.ai/plugins/community#how-to-submit>

## Access attempt

I attempted to fetch the OpenClaw docs directly from this environment:

- `curl -I -L https://docs.openclaw.ai/plugins/community`

The request is blocked in this environment with `403 Forbidden`, so the exact checklist cannot be quoted verbatim here.

## Prepared artifacts in this repo

To package the current memory layer as a plugin, this repo now contains:

- `core/context/openclaw_memory_plugin.py` — adapter that wraps the existing `ContextManager`
- `plugins/openclaw-memory/plugin.yaml` — plugin manifest
- `plugins/openclaw-memory/src/openclaw_memory_plugin.py` — entrypoint export
- `plugins/openclaw-memory/README.md` — package + validation instructions

## Suggested submission flow (to run once docs are reachable)

1. Confirm required manifest schema/fields against OpenClaw docs.
2. Update `plugins/openclaw-memory/plugin.yaml` to match exact field names.
3. Build artifact:
   - `tar -czf mo-agent-memory-plugin.tar.gz plugins/openclaw-memory`
4. Submit through the OpenClaw community plugin channel linked in docs.
5. Include:
   - plugin name: `mo-agent-memory`
   - capabilities: memory retrieval + context prompt assembly
   - source repository and usage instructions.

## Manual verification checklist

- [ ] Manifest schema validated against docs
- [ ] Entrypoint path validated by OpenClaw loader
- [ ] Entrypoint imports cleanly from packaged artifact (no repo-only `core.*` dependency)
- [ ] Plugin install test passes in OpenClaw runtime
- [ ] Submission form fields completed
- [ ] Archive uploaded successfully
