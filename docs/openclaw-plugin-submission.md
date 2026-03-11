# OpenClaw Community Plugin Submission Notes

Target docs:

- <https://docs.openclaw.ai/plugins/community>
- <https://docs.openclaw.ai/plugins/plugin-manifest>
- <https://docs.openclaw.ai/cli/plugins>

## Doc status

As of March 10, 2026, `https://docs.openclaw.ai/plugins/community` is reachable from the host environment:

- `curl -I -L https://docs.openclaw.ai/plugins/community`
- current host-level response: `HTTP/2 200`

The earlier `403 Forbidden` result appears to have been environmental or transient. This shell also had
`http_proxy` and `https_proxy` pointed at `127.0.0.1:7890`, which can produce misleading failures when
that local proxy is unavailable.

## Current package state in this repo

This repo now contains a host-loadable OpenClaw package scaffold under `plugins/openclaw-memory`:

- `package.json` with a single `openclaw.extensions` entry
- `openclaw.plugin.json` with the official manifest fields plus `configSchema`
- `src/agent-tools.ts` with the 4 memory tools
- `src/hooks.ts` with `before_prompt_build`, legacy `before_agent_start`, and `agent_end`
- `src/index.ts` as the single extension entrypoint
- `src/backend_bridge.py` to preserve the existing Python memory adapter boundary
- `src/openclaw_memory_plugin.py` as the package-local Python compatibility layer

## Verified official expectations

### Community submission

The community plugin flow is package-based:

1. publish the plugin package to `npm`
2. host the source publicly on GitHub
3. add the plugin to the community plugins table in the OpenClaw docs and submit a PR

### Loader expectations

The current docs indicate that OpenClaw expects:

- a root `openclaw.plugin.json`
- a root `package.json`
- `package.json` entries under `openclaw.extensions`
- JS or TS extension modules loaded by OpenClaw via `jiti`

### Manifest expectations

The official manifest docs list these required fields in `openclaw.plugin.json`:

- `id`
- `name`
- `description`
- `version`
- `configSchema`

### Tool and hook API shape

The docs describe:

- `api.registerTool(() => ({ ... }))` for tools
- `api.on(...)` for lifecycle hooks
- `before_prompt_build` as the modern prompt-injection hook
- `before_agent_start` as legacy compatibility

The documented prompt hook return fields include:

- `prependPrompt`
- `appendPrompt`
- `modelOverride`
- `providerOverride`

Search results and hook-doc references also point to `agent_end` as the relevant end-of-run lifecycle hook, so this package currently uses `agent_end` for auto-capture.

## What is now implemented

The package now matches the documented loader model at the packaging level:

- OpenClaw loads one TS entry module from `openclaw.extensions`
- the package declares `configSchema`
- the prompt injection path is implemented on `before_prompt_build`
- the plugin keeps a legacy `before_agent_start` alias so the old Python-facing behavior boundary remains available
- tool and hook handlers return deterministic payloads and are covered by both repo-side pytest and package-local Vitest tests

## Remaining real-host verification risks

The remaining uncertainty is narrower and should be verified in a real OpenClaw runtime:

- the exact callback payload shape for `agent_end`
- the exact module export contract the loader expects from each `openclaw.extensions` module
- how the host should configure `runtimeRoot` when the packaged Python bridge needs access to the mo-agent Python runtime outside this repo

The current package hedges the export-contract point by exporting both default and named registration functions from the TS modules.

## Practical submission checklist

- [x] `openclaw.plugin.json` includes the required official fields, including `configSchema`
- [x] `package.json` declares `openclaw.extensions` in the documented array form
- [x] JS/TS host entry modules exist and register tools/hooks against the documented API shape
- [x] modern prompt injection is implemented with `before_prompt_build`
- [x] legacy `before_agent_start` compatibility is preserved intentionally
- [x] package-local tests cover the TS host layer
- [x] repo-side tests cover the Python compatibility adapter and manifest/package consistency
- [ ] verify `agent_end` against a live OpenClaw host payload
- [ ] verify plugin loading in a live OpenClaw runtime
- [ ] publish the package to `npm`
- [ ] link a public GitHub source repo
- [ ] open and land the community plugins docs PR
