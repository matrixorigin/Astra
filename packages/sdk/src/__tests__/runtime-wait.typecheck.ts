import type { RuntimeWaitEvent, StreamEvent, WorkTurnStreamEvent } from "../index";

// A consumer of either public stream can narrow to the exported typed event.
export function runtimeWaitObservation(event: StreamEvent | WorkTurnStreamEvent): RuntimeWaitEvent | undefined {
  if (event.type === "runtime_wait") {
    const observation: RuntimeWaitEvent = event;
    const reason: "agent_input" = observation.reason;
    const waiting: boolean = observation.waiting;
    const runId: string = observation.run_id;
    return { type: "runtime_wait", reason, waiting, run_id: runId };
  }
  return undefined;
}
