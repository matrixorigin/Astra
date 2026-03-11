# mo-agent-memory (OpenClaw plugin package)

`plugins/openclaw-memory` now contains a real OpenClaw package surface:

- `package.json` with a single `openclaw.extensions` entry
- `openclaw.plugin.json` with the documented manifest fields, including `configSchema`
- TypeScript host entry modules for tools and hooks
- a package-local Python bridge that preserves the existing mo-agent memory adapter boundary

## Package layout

- `package.json`: OpenClaw package metadata and extension module path
- `openclaw.plugin.json`: official plugin manifest and config schema
- `plugin.yaml`: legacy compatibility manifest for the earlier Python prototype
- `src/agent-tools.ts`: registers `memory_recall`, `memory_store`, `memory_forget`, `memory_update`
- `src/hooks.ts`: registers `before_prompt_build`, `before_agent_start` (legacy alias), `agent_end`
- `src/index.ts`: single OpenClaw entrypoint that registers both tools and hooks
- `src/backend_bridge.py`: JSON bridge from the TS host layer to the Python adapter
- `src/openclaw_memory_plugin.py`: self-contained Python compatibility adapter used by the bridge

## Runtime model

OpenClaw loads the single TypeScript module declared under `openclaw.extensions`.
That module registers both the tools and hooks, then delegates memory operations to
`src/backend_bridge.py`, which in turn uses the existing Python `OpenClawMemoryPlugin`
adapter surface.

For actual mo-agent retrieval and persistence, the bridge needs access to the Python runtime bits
from this repo (`core.context.*` and `sdk`). It resolves them in this order:

1. `runtimeRoot` from plugin config
2. `MO_AGENT_RUNTIME_ROOT` from the environment
3. automatic repo-root detection when the plugin is still being run from this repo

If none of those paths exposes the mo-agent Python modules, the plugin package still loads in
OpenClaw, but memory operations fail with explicit bridge errors instead of silent no-ops.

## Config

The OpenClaw manifest defines these primary config fields:

- `autoRecall`
- `autoCapture`
- `captureAssistant`
- `recallLimit`
- `recallMaxTokens`
- `captureMaxItems`
- `defaultTaskType`
- `embeddingProvider`
- `pythonExecutable`
- `runtimeRoot`
- `defaultUserId`

## Local OpenClaw install

Official OpenClaw docs support local-folder installation with linking:

```bash
openclaw plugins install --link /home/momo/src/mo-agent-runtime/plugins/openclaw-memory
openclaw plugins list
openclaw plugins info mo-agent-memory
```

Example OpenClaw config entry:

```json
{
  "plugins": {
    "entries": {
      "mo-agent-memory": {
        "enabled": true,
        "config": {
          "autoRecall": true,
          "autoCapture": true,
          "embeddingProvider": "mock",
          "runtimeRoot": "/home/momo/src/mo-agent-runtime",
          "pythonExecutable": "/home/momo/.cache/pypoetry/virtualenvs/mo-dev-agent-trTOcoLJ-py3.12/bin/python"
        }
      }
    }
  }
}
```

The plugin id stays `mo-agent-memory` because the package now exposes a single extension entry.

## Runtime prerequisites

You need these pieces available on the machine running OpenClaw:

- a working Python interpreter with the mo-agent runtime dependencies installed
- access to the mo-agent repo checkout via `runtimeRoot` or `MO_AGENT_RUNTIME_ROOT`
- MatrixOne connection env vars for that Python process:
  - `MATRIXONE_HOST`
  - `MATRIXONE_PORT`
  - `MATRIXONE_USER`
  - `MATRIXONE_PASSWORD`
  - `MATRIXONE_DATABASE`
- the underlying schema expected by the Python runtime, especially `conversation_events` and `event_embeddings`
- `OPENAI_API_KEY` only if `embeddingProvider` is set to `openai`

`embeddingProvider: "mock"` avoids any external embedding model requirement. If you set
`embeddingProvider: "openai"`, the current Python implementation uses `text-embedding-3-small`.

## Local validation

```bash
pnpm --dir plugins/openclaw-memory install
node -e "const pkg=require('./plugins/openclaw-memory/package.json'); const manifest=require('./plugins/openclaw-memory/openclaw.plugin.json'); if(!pkg.openclaw?.extensions) throw new Error('missing openclaw.extensions'); if(!manifest.configSchema) throw new Error('missing configSchema')"
python3 -m py_compile plugins/openclaw-memory/src/openclaw_memory_plugin.py core/context/openclaw_memory_plugin.py
pnpm --dir plugins/openclaw-memory exec tsc --noEmit
pnpm --dir plugins/openclaw-memory exec vitest run
poetry run pytest tests/unit/test_openclaw_memory_plugin.py tests/unit/test_openclaw_plugin_manifest.py tests/unit/test_openclaw_plugin_hooks.py tests/unit/test_openclaw_plugin_tools.py tests/unit/test_openclaw_plugin_package.py
poetry run ruff check core/context/openclaw_memory_plugin.py tests/unit/test_openclaw_memory_plugin.py tests/unit/test_openclaw_plugin_manifest.py tests/unit/test_openclaw_plugin_hooks.py tests/unit/test_openclaw_plugin_tools.py tests/unit/test_openclaw_plugin_package.py
```

## Packaging

```bash
pnpm --dir plugins/openclaw-memory pack --pack-destination /tmp/openclaw-memory-pack
```

This verifies that the OpenClaw package artifacts are packable with the TS host modules,
manifest files, and Python bridge included.

## Remaining real-host checks

The package shape now matches the current OpenClaw loader model, but two things still require a
real OpenClaw runtime for final confirmation:

- the exact `agent_end` event payload fields available in the live host
- the exact module export contract OpenClaw uses when it loads each `openclaw.extensions` entry

The package hedges the second point by exporting default and named registration functions from the
TS modules.
