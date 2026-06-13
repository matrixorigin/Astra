import {
  blockedRunMessage,
  projectRunWaitingState,
  runWaitingStatusMessage,
} from "@/lib/run-status-messages";

describe("run status messages", () => {
  it("maps execution-boundary waiting reasons to readable assistant feedback", () => {
    expect(runWaitingStatusMessage("executor_offline", true)).toBe(
      "Run paused because the selected executor is offline. Reconnect it or choose another workspace.",
    );
    expect(runWaitingStatusMessage("fallback_disabled", true)).toBe(
      "Run paused because server fallback is disabled for this workspace.",
    );
    expect(
      runWaitingStatusMessage("workspace_executor_unavailable", true),
    ).toBe(
      "Run paused because the selected workspace is not connected to an available executor. Choose Server sandbox or a connected edge workspace.",
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
      "Tool transport disconnected. Reconnect the selected executor or retry after transport recovers.",
    );
    expect(blockedRunMessage("workspace_path_mismatch")).toBe(
      "The selected workspace does not own the referenced local path. Choose a matching edge workspace or use paths inside the bound workspace.",
    );
    expect(blockedRunMessage("workspace_executor_unavailable")).toBe(
      "The selected workspace is not connected to an available executor transport. Choose Server sandbox or a connected edge workspace.",
    );
  });
});
