import {
  blockedRunMessage,
  projectRunWaitingState,
  runWaitingStatusMessage,
} from "@/lib/run-status-messages";

describe("run status messages", () => {
  it("maps execution-boundary waiting reasons to readable assistant feedback", () => {
    expect(runWaitingStatusMessage("executor_offline", true)).toBe(
      "Run paused because the execution environment is offline. Reconnect it or choose another environment.",
    );
    expect(runWaitingStatusMessage("fallback_disabled", true)).toBe(
      "Run paused because this request needs a file or command environment. Connect one or choose a sandbox, then retry.",
    );
    expect(
      runWaitingStatusMessage("workspace_executor_unavailable", true),
    ).toBe(
      "Run paused because this request needs a file or command environment. Connect one or choose a sandbox, then retry.",
    );
  });

  it("maps generic waiting reasons without leaking protocol prefixes", () => {
    expect(runWaitingStatusMessage("waiting: tool_approval", false)).toBe(
      "Waiting for tool approval.",
    );
    expect(runWaitingStatusMessage("custom_runtime_signal", false)).toBe(
      "Waiting for custom runtime signal.",
    );
  });

  it("projects ordinary waiting events without promoting them to blocked", () => {
    expect(
      projectRunWaitingState({ reason: "waiting: tool_approval" }),
    ).toEqual({
      status: "waiting",
      waitingFor: "tool_approval",
      blocked: false,
    });
  });

  it("projects execution-boundary waiting events as blocked", () => {
    expect(projectRunWaitingState({ error_kind: "executor_offline" })).toEqual({
      status: "blocked",
      waitingFor: "executor_offline",
      blocked: true,
    });
  });

  it("maps blocked reasons to actionable work-surface messages", () => {
    expect(blockedRunMessage("transport_disconnected")).toBe(
      "Execution connection disconnected. Reconnect it or retry after it recovers.",
    );
    expect(blockedRunMessage("workspace_path_mismatch")).toBe(
      "The referenced path is outside the selected file environment. Choose the environment that contains it or use a path inside the current one.",
    );
    expect(blockedRunMessage("workspace_executor_unavailable")).toBe(
      "This request needs a file or command environment. Connect one or choose a sandbox, then retry.",
    );
  });
});
