import { describe, expect, it } from "vitest";

import { createAgentToolsExtension } from "../src/agent-tools";
import type { MemoryBackend, RecallParams, StoreParams, UpdateParams } from "../src/runtime";

class FakeBackend implements MemoryBackend {
  public recalls: RecallParams[] = [];
  public stores: StoreParams[] = [];
  public forgets: string[] = [];
  public updates: UpdateParams[] = [];

  async recall(params: RecallParams) {
    this.recalls.push(params);
    if (params.query === "fallback") {
      return [];
    }
    return [
      {
        event_id: "evt-1",
        event_type: "user_message",
        content: "Remember the DSN override.",
        score: 0.91
      }
    ];
  }

  async store(params: StoreParams) {
    this.stores.push(params);
    return `mem-${this.stores.length}`;
  }

  async forget(memoryId: string) {
    this.forgets.push(memoryId);
    return memoryId !== "missing";
  }

  async update(params: UpdateParams) {
    this.updates.push(params);
    return params.memory_id !== "missing";
  }

  async searchIds() {
    return ["mem-9"];
  }
}

class FakeApi {
  public tools = new Map<string, Record<string, any>>();

  registerTool(factory: () => Record<string, any>) {
    const tool = factory();
    this.tools.set(tool.name, tool);
  }
}

describe("agent tools extension", () => {
  it("registers all four tools and returns deterministic payloads", async () => {
    const api = new FakeApi();
    const backend = new FakeBackend();

    createAgentToolsExtension({ backend })(api, { defaultUserId: "u-default" });

    expect([...api.tools.keys()]).toEqual([
      "memory_recall",
      "memory_store",
      "memory_forget",
      "memory_update"
    ]);

    const recall = await api.tools.get("memory_recall")?.execute({
      session_id: "s-1",
      query: "what matters",
      limit: 1
    });
    expect(recall.details.count).toBe(1);
    expect(backend.recalls[0]?.session_id).toBe("s-1");

    const store = await api.tools.get("memory_store")?.execute({
      session_id: "s-1",
      text: "remember this"
    });
    expect(store.details.memory_id).toBe("mem-1");
    expect(backend.stores[0]?.source).toBe("tool.memory_store");

    const forget = await api.tools.get("memory_forget")?.execute({
      session_id: "s-1",
      query: "fallback"
    });
    expect(forget.details.memory_ids).toEqual(["mem-9"]);

    const update = await api.tools.get("memory_update")?.execute({
      memory_id: "mem-1",
      text: "new text"
    });
    expect(update.details.updated_fields).toEqual(["text"]);
  });

  it("validates empty memory_update payloads", async () => {
    const api = new FakeApi();
    createAgentToolsExtension({ backend: new FakeBackend() })(api, {});

    const response = await api.tools.get("memory_update")?.execute({ memory_id: "mem-1" });

    expect(response.details.error).toBe("memory_update_failed");
    expect(String(response.details.message)).toContain("Provide at least one field to update");
  });
});
