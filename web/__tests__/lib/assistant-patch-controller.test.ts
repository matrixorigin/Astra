import { createAssistantPatchController } from "@/lib/api/assistant-patch-controller";
import type { ChatDetail } from "@/lib/api/types";

function makeDetail(): ChatDetail {
  return {
    chat: {
      id: "chat-1",
      title: null,
      projectId: null,
      createdAt: "2026-01-01T00:00:00.000Z",
      updatedAt: "2026-01-01T00:00:00.000Z",
    },
    messages: [
      {
        id: "assistant-1",
        role: "assistant",
        content: "",
        createdAt: "2026-01-01T00:00:00.000Z",
        status: "streaming",
      },
    ],
  };
}

function makeStateHarness() {
  let detail = makeDetail();
  return {
    get detail() {
      return detail;
    },
    setDetail(updater: ChatDetail | ((current: ChatDetail) => ChatDetail)) {
      detail = typeof updater === "function" ? updater(detail) : updater;
    },
  };
}

describe("createAssistantPatchController", () => {
  let rafCallbacks: Map<number, FrameRequestCallback>;
  let nextRafId: number;
  let originalRequestAnimationFrame: typeof globalThis.requestAnimationFrame;
  let originalCancelAnimationFrame: typeof globalThis.cancelAnimationFrame;

  beforeEach(() => {
    rafCallbacks = new Map();
    nextRafId = 1;
    originalRequestAnimationFrame = globalThis.requestAnimationFrame;
    originalCancelAnimationFrame = globalThis.cancelAnimationFrame;
    globalThis.requestAnimationFrame = vi.fn((callback) => {
      const id = nextRafId;
      nextRafId += 1;
      rafCallbacks.set(id, callback);
      return id;
    });
    globalThis.cancelAnimationFrame = vi.fn((id) => {
      rafCallbacks.delete(id);
    });
  });

  afterEach(() => {
    globalThis.requestAnimationFrame = originalRequestAnimationFrame;
    globalThis.cancelAnimationFrame = originalCancelAnimationFrame;
  });

  it("applies immediate patches to the current assistant message", () => {
    const harness = makeStateHarness();
    const controller = createAssistantPatchController({
      setDetail: harness.setDetail,
      getAssistantId: () => "assistant-1",
    });

    controller.patchNow({ content: "hello", status: "complete" });

    expect(harness.detail.messages[0]).toMatchObject({
      content: "hello",
      status: "complete",
    });
  });

  it("coalesces batched patches into one animation-frame update", () => {
    const harness = makeStateHarness();
    const controller = createAssistantPatchController({
      setDetail: harness.setDetail,
      getAssistantId: () => "assistant-1",
    });

    controller.patchBatched({ content: "hel" });
    controller.patchBatched({ content: "hello", reasoning: "thinking" });

    expect(harness.detail.messages[0]?.content).toBe("");
    expect(rafCallbacks.size).toBe(1);

    const callback = [...rafCallbacks.values()][0];
    callback(0);

    expect(harness.detail.messages[0]).toMatchObject({
      content: "hello",
      reasoning: "thinking",
    });
  });

  it("preserves artifacts that arrived before a resumed stream cursor", () => {
    const harness = makeStateHarness();
    harness.detail.messages[0]!.artifacts = [
      { id: "artifact-a", kind: "file", filename: "a.pdf" },
    ];
    const controller = createAssistantPatchController({
      setDetail: harness.setDetail,
      getAssistantId: () => "assistant-1",
    });

    controller.patchBatched({
      artifacts: [
        { id: "artifact-b", kind: "file", filename: "b.pdf" },
      ],
    });
    const callback = [...rafCallbacks.values()][0];
    callback(0);

    expect(harness.detail.messages[0]?.artifacts).toEqual([
      expect.objectContaining({ id: "artifact-a" }),
      expect.objectContaining({ id: "artifact-b" }),
    ]);
  });

  it("drops pending and future patches after cancel", () => {
    const harness = makeStateHarness();
    const controller = createAssistantPatchController({
      setDetail: harness.setDetail,
      getAssistantId: () => "assistant-1",
    });

    controller.patchBatched({ content: "pending" });
    controller.cancel();
    controller.patchNow({ content: "late" });

    expect(harness.detail.messages[0]?.content).toBe("");
    expect(globalThis.cancelAnimationFrame).toHaveBeenCalledTimes(1);
  });
});
