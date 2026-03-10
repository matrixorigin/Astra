# mo-agent-memory (OpenClaw plugin package)

This package exposes the existing `ContextManager` memory-selection logic from `mo-agent-runtime` as an OpenClaw plugin.

## What is included

- `plugin.yaml`: plugin metadata/manifest
- `src/openclaw_memory_plugin.py`: OpenClaw entrypoint exporting `OpenClawMemoryPlugin`

## Mapped capabilities

- `memory.retrieve`: returns selected memory snippets from `conversation_events`
- `memory.context_prompt`: builds assembled prompt text from selected memory

## Local validation

```bash
python -m pytest tests/unit/test_openclaw_memory_plugin.py
python -m pytest tests/unit/test_openclaw_plugin_package.py
```

The package smoke test verifies the plugin entrypoint can load in an isolated
package directory (without relying on repo-level `core.*` imports).

## Packaging

```bash
tar -czf mo-agent-memory-plugin.tar.gz plugins/openclaw-memory
```

The resulting archive can be attached/uploaded during OpenClaw community plugin submission.
