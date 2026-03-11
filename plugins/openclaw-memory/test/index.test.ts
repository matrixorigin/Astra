import { describe, expect, it } from "vitest";

import registerPlugin, { id } from "../src/index";
import type { MemoryBackend, RecallParams, StoreParams, UpdateParams } from "../src/runtime";

class FakeBackend implements MemoryBackend {
  async recall(_params: RecallParams) {
    return [];
  }

  async store(_params: StoreParams) {
    return "mem-1";
  }

  async forget(_memoryId: string) {
    return true;
  }

  async update(_params: UpdateParams) {
    return true;
  }

  async searchIds() {
    return [];
  }
}

class FakeApi {
  public tools = new Map<string, Record<string, unknown>>();
  public hooks = new Map<string, (...args: unknown[]) => unknown>();

  registerTool(spec: Record<string, unknown>) {
    this.tools.set(String(spec.name), spec);
  }

  on(name: string, callback: (...args: unknown[]) => unknown) {
    this.hooks.set(name, callback);
  }
}

describe("single entry extension", () => {
  it("registers tools and hooks through the package entrypoint", () => {
    const api = new FakeApi();
    const backend = new FakeBackend();

    const result = registerPlugin(api, {}, { backend });

    expect(id).toBe("mo-agent-memory");
    expect(result.id).toBe("mo-agent-memory");
    expect([...api.tools.keys()]).toEqual([
      "memory_recall",
      "memory_store",
      "memory_forget",
      "memory_update",
    ]);
    expect([...api.hooks.keys()]).toEqual([
      "before_prompt_build",
      "before_agent_start",
      "agent_end",
    ]);
  });
});
