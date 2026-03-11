import {
  type MemoryBackend,
  type PluginConfig,
  createPythonBackend,
  extractMessageTexts,
  extractPrompt,
  extractSessionId,
  extractUserId,
  formatMemoryBlock,
  normalizeConfig,
  registerHook
} from "./runtime";

interface ExtensionDeps {
  backend?: MemoryBackend;
}

export function registerHooks(
  api: any,
  rawConfig: Record<string, unknown> = {},
  deps: ExtensionDeps = {}
): { config: PluginConfig; backend: MemoryBackend } {
  const config = normalizeConfig(rawConfig);
  const backend = deps.backend ?? createPythonBackend(config);

  registerHook(api, "before_prompt_build", async (event: unknown, ctx?: unknown) =>
    handleBeforePromptBuild(backend, config, event, ctx)
  );
  registerHook(api, "before_agent_start", async (event: unknown, ctx?: unknown) =>
    handleBeforeAgentStart(backend, config, event, ctx)
  );
  registerHook(api, "agent_end", async (event: unknown, ctx?: unknown) =>
    handleAgentEnd(backend, config, event, ctx)
  );

  return { config, backend };
}

export async function handleBeforePromptBuild(
  backend: MemoryBackend,
  config: PluginConfig,
  event: unknown,
  ctx?: unknown
): Promise<Record<string, unknown> | null> {
  if (!config.autoRecall) {
    return null;
  }

  const sessionId = extractSessionId(event, ctx);
  const prompt = extractPrompt(event, ctx);
  if (!sessionId || !prompt) {
    return null;
  }

  const snippets = await backend.recall({
    session_id: sessionId,
    query: prompt,
    max_tokens: config.recallMaxTokens,
    task_type: config.defaultTaskType
  });

  if (!snippets.length) {
    return null;
  }

  return {
    prependContext: formatMemoryBlock(snippets.slice(0, config.recallLimit))
  };
}

export async function handleBeforeAgentStart(
  backend: MemoryBackend,
  config: PluginConfig,
  event: unknown,
  ctx?: unknown
): Promise<Record<string, unknown> | null> {
  const promptResult = await handleBeforePromptBuild(backend, config, event, ctx);
  if (!promptResult?.prependContext) {
    return null;
  }

  return {
    prependContext: promptResult.prependContext
  };
}

export async function handleAgentEnd(
  backend: MemoryBackend,
  config: PluginConfig,
  event: unknown,
  ctx?: unknown
): Promise<Record<string, unknown> | null> {
  const success = readField(event, "success");
  if (success === false || !config.autoCapture) {
    return null;
  }

  const sessionId = extractSessionId(event, ctx);
  if (!sessionId) {
    return null;
  }

  const userId = extractUserId(event, ctx, config.defaultUserId);
  const uniqueTexts = new Set(
    extractMessageTexts(event, config.captureAssistant)
      .map((text: string) => text.trim())
      .filter(Boolean)
  );

  const memoryIds: string[] = [];
  for (const text of Array.from(uniqueTexts).slice(0, config.captureMaxItems)) {
    const memoryId = await backend.store({
      session_id: sessionId,
      user_id: userId,
      text,
      category: "other",
      importance: 0.7,
      source: "hook.agent_end"
    });
    memoryIds.push(memoryId);
  }

  return { captured: memoryIds.length, memoryIds };
}

export function createHooksExtension(deps: ExtensionDeps = {}) {
  return (api: any, rawConfig: Record<string, unknown> = {}) => registerHooks(api, rawConfig, deps);
}

export const register = registerHooks;
export const loadPlugin = registerHooks;
export default registerHooks;

function readField(source: unknown, key: string): unknown {
  if (source && typeof source === "object" && key in (source as Record<string, unknown>)) {
    return (source as Record<string, unknown>)[key];
  }
  return undefined;
}
