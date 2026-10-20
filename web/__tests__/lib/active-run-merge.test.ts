import { mergeStreamRunUpdate } from "@/lib/api/active-run-merge";

it("keeps progress monotonic within one message while status-only updates preserve its binding", () => {
  const existing = { runId: "root", status: "running", assistantMessageId: "first", nextEventIndex: 12 };
  expect(mergeStreamRunUpdate({ runId: "root", status: "waiting", nextEventIndex: 9 }, existing))
    .toMatchObject({ status: "waiting", assistantMessageId: "first", nextEventIndex: 12 });
});

it.each(["another message", "previously unbound", "another root"])("does not carry a cursor into %s", change => {
  const existing = { runId: "root", status: "running", nextEventIndex: 12,
    ...(change === "previously unbound" ? {} : { assistantMessageId: "first" }) };
  expect(mergeStreamRunUpdate({ runId: change === "another root" ? "other" : "root",
    status: "input-queued", assistantMessageId: "second", nextEventIndex: 8 }, existing))
    .toMatchObject({ assistantMessageId: "second", nextEventIndex: 8 });
});
