import { describe, expect, test } from "vitest";
import {
  normalizeToolStatus,
  planStepResultStatus,
  toolTerminalStatus,
} from "../lifecycle-utils";

describe("lifecycle utils", () => {
  test("normalizes only known tool statuses", () => {
    expect(normalizeToolStatus("done")).toBe("done");
    expect(normalizeToolStatus("completed")).toBe("done");
    expect(normalizeToolStatus("failed")).toBe("error");
    expect(normalizeToolStatus("timed_out")).toBe("error");
    expect(normalizeToolStatus("cancelled")).toBe("cancelled");
    expect(normalizeToolStatus("skipped")).toBe("skipped");
    expect(normalizeToolStatus("")).toBeUndefined();
    expect(normalizeToolStatus("ok")).toBeUndefined();
  });

  test("projects cancelled tool events before generic failure", () => {
    expect(
      toolTerminalStatus({
        type: "tool_transport_failed",
        success: false,
        error_kind: "cancelled",
      }),
    ).toBe("cancelled");
    expect(toolTerminalStatus({ status: "cancelled", success: false })).toBe(
      "cancelled",
    );
  });

  test("does not treat blank error_kind as failure", () => {
    expect(toolTerminalStatus({ type: "tool_call_end", error_kind: "" })).toBe(
      "done",
    );
  });

  test("does not let skipped flag override explicit done status", () => {
    expect(toolTerminalStatus({ status: "done", skipped: true })).toBe("done");
    expect(toolTerminalStatus({ skipped: true })).toBe("skipped");
    expect(toolTerminalStatus({ status: "skipped", success: false })).toBe(
      "skipped",
    );
  });

  test("projects failed plan step terminal aliases as errors", () => {
    expect(planStepResultStatus("error")).toBe("error");
    expect(planStepResultStatus("failed")).toBe("error");
    expect(planStepResultStatus("timed_out")).toBe("error");
    expect(planStepResultStatus("success")).toBe("done");
  });
});
