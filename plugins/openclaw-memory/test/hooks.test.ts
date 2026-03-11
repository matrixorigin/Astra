import { describe, expect, it } from "vitest";

import { createHooksExtension } from "../src/hooks";
import type { MemoryBackend, RecallParams, StoreParams, UpdateParams } from "../src/runtime";

class FakeBackend implements MemoryBackend {
  public recalls: RecallParams[] = [];
  public stores: StoreParams[] = [];

  async recall(params: RecallParams) {
    this.recalls.push(params);
    return [
      {
        event_id: "evt-1",
        event_type: "user_message",
        content: "Remember the DB DSN override.",
        score: 0.95
      }
    ];
  }

  async store(params: StoreParams) {
    this.stores.push(params);
    return `mem-${this.stores.length}`;
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
  public hooks = new Map<string, (...args: any[]) => any>();

  on(name: string, callback: (...args: any[]) => any) {
    this.hooks.set(name, callback);
  }
}

describe("hooks extension", () => {
  it("registers modern and legacy prompt hooks plus capture", async () => {
    const api = new FakeApi();
    const backend = new FakeBackend();

    createHooksExtension({ backend })(api, {
      autoRecall: true,
      autoCapture: true,
      captureMaxItems: 2
    });

    expect([...api.hooks.keys()]).toEqual([
      "before_prompt_build",
      "before_agent_start",
      "agent_end"
    ]);

    const promptResult = await api.hooks.get("before_prompt_build")?.({
      session_id: "s-1",
      prompt: "How should I configure cache?"
    });
    expect(promptResult.prependContext).toContain("<relevant-memories>");
    expect(promptResult.prependContext).toContain("Remember the DB DSN override.");

    const legacyResult = await api.hooks.get("before_agent_start")?.({
      session_id: "s-1",
      prompt: "How should I configure cache?"
    });
    expect(legacyResult.prependContext).toContain("<relevant-memories>");

    const captureResult = await api.hooks.get("agent_end")?.({
      session_id: "s-1",
      user_id: "u-1",
      success: true,
      messages: [
        { role: "user", content: "use poetry run ruff" },
        { role: "assistant", content: "ack" },
        { role: "user", content: "use poetry run ruff" }
      ]
    });
    expect(captureResult).toEqual({ captured: 1, memoryIds: ["mem-1"] });
    expect(backend.stores[0]?.source).toBe("hook.agent_end");
  });

  it("returns null when recall is disabled", async () => {
    const api = new FakeApi();
    createHooksExtension({ backend: new FakeBackend() })(api, { autoRecall: false });

    const result = await api.hooks.get("before_prompt_build")?.({
      session_id: "s-1",
      prompt: "ignored"
    });

    expect(result).toBeNull();
  });
});
