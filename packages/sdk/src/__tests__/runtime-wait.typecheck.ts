import { isWorkTurnEventType } from "../types";
import type { RuntimeWaitEvent, StreamEvent, WorkTurnStreamEvent } from "../index";

// A consumer of either public stream can narrow to the exported typed event.
export function runtimeWaitObservation(event: StreamEvent): RuntimeWaitEvent | undefined {
  if (event.type === "runtime_wait") {
    const observation: RuntimeWaitEvent = event;
    const reason: "agent_input" = observation.reason;
    const waiting: boolean = observation.waiting;
    const runId: string = observation.run_id;
    return { type: "runtime_wait", reason, waiting, run_id: runId };
  }
  return undefined;
}

export function workRuntimeWaitObservation(event: WorkTurnStreamEvent): RuntimeWaitEvent | undefined {
  return isWorkTurnEventType(event, "runtime_wait") ? event : undefined;
}

export function unknownWorkPayloadRemainsUnknown(event: WorkTurnStreamEvent) {
  if ("new_payload" in event) {
    const payload: unknown = event.new_payload;
    // @ts-expect-error Unmodeled runtime facts do not become unchecked payloads.
    const count: number = event.new_payload;
    return { payload, count };
  }
  return undefined;
}
