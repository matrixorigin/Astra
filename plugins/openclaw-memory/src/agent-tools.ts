import {
  DEFAULT_CONFIG,
  type MemoryBackend,
  type MemorySnippet,
  type PluginConfig,
  createPythonBackend,
  normalizeConfig,
  registerTool,
  toolError,
  toolOk
} from "./runtime";

interface ExtensionDeps {
  backend?: MemoryBackend;
}

export function registerAgentTools(
  api: any,
  rawConfig: Record<string, unknown> = {},
  deps: ExtensionDeps = {}
): { config: PluginConfig; backend: MemoryBackend } {
  const config = normalizeConfig(rawConfig);
  const backend = deps.backend ?? createPythonBackend(config);

  registerTool(api, {
    name: "memory_recall",
    description: "Recall relevant memory snippets for a query.",
    parameters: {
      type: "object",
      properties: {
        session_id: { type: "string" },
        query: { type: "string" },
        limit: { type: "integer", minimum: 1, maximum: 20 },
        max_tokens: { type: "integer", minimum: 256, maximum: 8000 },
        task_type: { type: "string" }
      },
      required: ["session_id", "query"],
      additionalProperties: false
    },
    execute: async (params: Record<string, unknown>) => handleMemoryRecall(backend, config, params)
  });

  registerTool(api, {
    name: "memory_store",
    description: "Store an important memory item.",
    parameters: {
      type: "object",
      properties: {
        session_id: { type: "string" },
        user_id: { type: "string" },
        text: { type: "string" },
        category: { type: "string" },
        importance: { type: "number", minimum: 0, maximum: 1 }
      },
      required: ["session_id", "text"],
      additionalProperties: false
    },
    execute: async (params: Record<string, unknown>) => handleMemoryStore(backend, config, params)
  });

  registerTool(api, {
    name: "memory_forget",
    description: "Delete one memory by ID or by query lookup.",
    parameters: {
      type: "object",
      properties: {
        memory_id: { type: "string" },
        session_id: { type: "string" },
        query: { type: "string" },
        task_type: { type: "string" },
        limit: { type: "integer", minimum: 1, maximum: 20 }
      },
      additionalProperties: false
    },
    execute: async (params: Record<string, unknown>) => handleMemoryForget(backend, config, params)
  });

  registerTool(api, {
    name: "memory_update",
    description: "Update text/category/importance for an existing memory item.",
    parameters: {
      type: "object",
      properties: {
        memory_id: { type: "string" },
        text: { type: "string" },
        category: { type: "string" },
        importance: { type: "number", minimum: 0, maximum: 1 }
      },
      required: ["memory_id"],
      additionalProperties: false
    },
    execute: async (params: Record<string, unknown>) => handleMemoryUpdate(backend, params)
  });

  return { config, backend };
}

export async function handleMemoryRecall(
  backend: MemoryBackend,
  config: PluginConfig,
  params: Record<string, unknown>
) {
  try {
    const sessionId = requireString(params, "session_id");
    const query = requireString(params, "query");
    const limit = clampInt(params.limit, config.recallLimit, 1, 20);
    const maxTokens = clampInt(params.max_tokens, config.recallMaxTokens, 256, 8000);
    const taskType = optionalString(params.task_type) || config.defaultTaskType;

    const memories = (await backend.recall({
      session_id: sessionId,
      query,
      max_tokens: maxTokens,
      task_type: taskType
    })).slice(0, limit);

    if (!memories.length) {
      return toolOk("No relevant memories found.", { count: 0, memories: [] });
    }

    const rows = memories.map(
      (memory: MemorySnippet, index: number) =>
        `${index + 1}. [${memory.event_type}] ${memory.content} (${memory.score.toFixed(3)})`
    );

    return toolOk(`Found ${memories.length} memories:\n\n${rows.join("\n")}`, {
      count: memories.length,
      memories,
      session_id: sessionId,
      query
    });
  } catch (error) {
    return toolError("memory_recall_failed", String(error));
  }
}

export async function handleMemoryStore(
  backend: MemoryBackend,
  config: PluginConfig,
  params: Record<string, unknown>
) {
  try {
    const sessionId = requireString(params, "session_id");
    const text = requireString(params, "text");
    const userId = optionalString(params.user_id) || config.defaultUserId;
    const category = optionalString(params.category) || "other";
    const importance = clampNumber(params.importance, 0.7, 0, 1);

    const memoryId = await backend.store({
      session_id: sessionId,
      user_id: userId,
      text,
      category,
      importance,
      source: "tool.memory_store"
    });

    return toolOk(`Stored memory ${memoryId}.`, {
      action: "created",
      memory_id: memoryId,
      session_id: sessionId,
      user_id: userId,
      category,
      importance
    });
  } catch (error) {
    return toolError("memory_store_failed", String(error));
  }
}

export async function handleMemoryForget(
  backend: MemoryBackend,
  config: PluginConfig,
  params: Record<string, unknown>
) {
  try {
    const explicitMemoryId = optionalString(params.memory_id);
    const sessionId = optionalString(params.session_id);
    const query = optionalString(params.query);
    const limit = clampInt(params.limit, 1, 1, 20);
    const taskType = optionalString(params.task_type) || config.defaultTaskType;

    let candidateIds = explicitMemoryId ? [explicitMemoryId] : [];

    if (!candidateIds.length && sessionId && query) {
      const recalled = await backend.recall({
        session_id: sessionId,
        query,
        max_tokens: config.recallMaxTokens,
        task_type: taskType
      });
      candidateIds = recalled.map((memory: MemorySnippet) => memory.event_id);
    }

    if (!candidateIds.length && sessionId && query) {
      candidateIds = await backend.searchIds({ session_id: sessionId, query, limit });
    }

    if (!candidateIds.length) {
      return toolOk("No memory candidate found to delete.", { action: "not_found", memory_ids: [] });
    }

    const deletedIds: string[] = [];
    for (const candidateId of candidateIds.slice(0, limit)) {
      if (await backend.forget(candidateId)) {
        deletedIds.push(candidateId);
      }
    }

    if (!deletedIds.length) {
      return toolOk("No memory was deleted.", { action: "not_found", memory_ids: [] });
    }

    return toolOk(`Deleted ${deletedIds.length} memory item(s).`, {
      action: "deleted",
      memory_ids: deletedIds
    });
  } catch (error) {
    return toolError("memory_forget_failed", String(error));
  }
}

export async function handleMemoryUpdate(backend: MemoryBackend, params: Record<string, unknown>) {
  try {
    const memoryId = requireString(params, "memory_id");
    const text = optionalString(params.text);
    const category = optionalString(params.category);
    const importance = Object.prototype.hasOwnProperty.call(params, "importance")
      ? clampNumber(params.importance, 0.7, 0, 1)
      : undefined;

    if (text === undefined && category === undefined && importance === undefined) {
      throw new Error("Provide at least one field to update: text/category/importance");
    }

    const updated = await backend.update({
      memory_id: memoryId,
      text,
      category,
      importance
    });

    if (!updated) {
      return toolOk("Memory not found.", { action: "not_found", memory_id: memoryId });
    }

    return toolOk(`Updated memory ${memoryId}.`, {
      action: "updated",
      memory_id: memoryId,
      updated_fields: [
        ...(text !== undefined ? ["text"] : []),
        ...(category !== undefined ? ["category"] : []),
        ...(importance !== undefined ? ["importance"] : [])
      ]
    });
  } catch (error) {
    return toolError("memory_update_failed", String(error));
  }
}

export function createAgentToolsExtension(deps: ExtensionDeps = {}) {
  return (api: any, rawConfig: Record<string, unknown> = {}) => registerAgentTools(api, rawConfig, deps);
}

export const register = registerAgentTools;
export const loadPlugin = registerAgentTools;
export const defaultConfig = DEFAULT_CONFIG;
export default registerAgentTools;

function requireString(params: Record<string, unknown>, key: string): string {
  const value = params[key];
  if (typeof value === "string" && value.trim()) {
    return value.trim();
  }
  throw new Error(`Missing required parameter: ${key}`);
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value.trim() : undefined;
}

function clampInt(value: unknown, fallback: number, minValue: number, maxValue: number): number {
  const parsed = Number.parseInt(String(value), 10);
  if (Number.isNaN(parsed)) {
    return fallback;
  }
  return Math.max(minValue, Math.min(maxValue, parsed));
}

function clampNumber(value: unknown, fallback: number, minValue: number, maxValue: number): number {
  const parsed = Number.parseFloat(String(value));
  if (Number.isNaN(parsed)) {
    return fallback;
  }
  return Math.max(minValue, Math.min(maxValue, parsed));
}
