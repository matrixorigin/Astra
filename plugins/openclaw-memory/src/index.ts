import { registerAgentTools } from "./agent-tools";
import { registerHooks } from "./hooks";
import type { MemoryBackend } from "./runtime";

export const id = "mo-agent-memory";

interface ExtensionDeps {
  backend?: MemoryBackend;
}

export function register(
  api: unknown,
  rawConfig: Record<string, unknown> = {},
  deps: ExtensionDeps = {}
) {
  const tools = registerAgentTools(api, rawConfig, deps);
  const hooks = registerHooks(api, rawConfig, { backend: deps.backend ?? tools.backend });

  return {
    id,
    config: hooks.config,
    backend: tools.backend,
  };
}

export const loadPlugin = register;
export default register;

export * from "./runtime";
export * from "./agent-tools";
export * from "./hooks";
