import type { ExplainAnalyzeEventV1 } from "@astra/sdk";
import type { ChatMessage } from "@/lib/api/types";
import { appendExplainFact, beginExplainRepair, finishExplainRepair, markExplainGap, EXPLAIN_FACT_LIMIT } from "@/lib/explain-analyze-observation";
const message: ChatMessage = { id: "a", role: "assistant", content: "answer", createdAt: "2026-01-01" };
const fact: ExplainAnalyzeEventV1 = { type: "explain_analyze", schema_version: 1,
  event_id: "e1", run_id: "r", turn_id: "t", node_id: "t", producer_id: "p",
  clock_domain_id: "c", kind: "turn", label: "Answer", transition: "started", elapsed_ms: 0 };
describe("Explain observation repair", () => {
  it("clears a repaired delivery gap retaining transcript and recovered facts", () => {
    let current = beginExplainRepair(markExplainGap(message), "repair-1");
    current = appendExplainFact(current, fact);
    current = finishExplainRepair(current, "repair-1");
    expect(current).toMatchObject({ content: "answer", explainAnalyzeEvents: [fact], explainAnalyzeDegraded: false });
    expect(current.explainAnalyzeRepairToken).toBeUndefined();
  });
  it("does not clear a new gap during replay or an older repair's successor", () => {
    const repairing = beginExplainRepair(markExplainGap(message), "old");
    const newerGap = markExplainGap(repairing);
    expect(finishExplainRepair(newerGap, "old").explainAnalyzeDegraded).toBe(true);
    const newerRepair = beginExplainRepair(newerGap, "new");
    expect(finishExplainRepair(newerRepair, "old")).toBe(newerRepair);
    expect(finishExplainRepair(newerRepair, "new").explainAnalyzeDegraded).toBe(false);
  });
  it("does not treat EOF as proof malformed or truncated facts were repaired", () => {
    const badBefore = beginExplainRepair(markExplainGap(message, true), "repair");
    expect(finishExplainRepair(badBefore, "repair").explainAnalyzeDegraded).toBe(true);
    const badDuring = markExplainGap(beginExplainRepair(message, "repair"), true);
    expect(finishExplainRepair(badDuring, "repair").explainAnalyzeDegraded).toBe(true);
  });
  it("ignores envelope differences and duplicates at capacity, but retains conflicts", () => {
    const full = { ...message, explainAnalyzeEvents: Array.from({ length: EXPLAIN_FACT_LIMIT }, (_, i) => ({ ...fact, event_id: `e${i}` })) };
    expect(appendExplainFact(full, { ...fact, index: 90 } as ExplainAnalyzeEventV1)).toBe(full);
    const truncated = appendExplainFact(full, { ...fact, event_id: "new" });
    expect(truncated.explainAnalyzeUnrecoverable).toBe(true);
    expect(truncated.explainAnalyzeEvents).toHaveLength(EXPLAIN_FACT_LIMIT);
    const conflict = appendExplainFact(appendExplainFact(message, fact), { ...fact, label: "Conflict" });
    expect(conflict.explainAnalyzeEvents).toHaveLength(2);
  });
});
