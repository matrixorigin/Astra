import { describe, expect, it } from "vitest";
import { isArtifactPublicationV1 } from "../artifact-publication";
const identity = { type: "artifact_publication", schema_version: 1, run_id: "run-1", turn_id: "turn-1",
  execution_owner_generation: 1, artifact_type: "explain_analyze_snapshot", recorded: true };
describe("artifact publication", () => {
  it("distinguishes readable reports and failures that could not be retained", () => {
    expect(isArtifactPublicationV1({ ...identity, status: "published", handle: `artifact://session/explain-analyze/${"a".repeat(64)}` })).toBe(true);
    expect(isArtifactPublicationV1({ ...identity, recorded: false, status: "unavailable", reason_code: "storage_failed", message: "Report storage failed." })).toBe(true);
    expect(isArtifactPublicationV1({ ...identity, status: "published", handle: "/tmp/report.md" })).toBe(false);
    expect(isArtifactPublicationV1({ ...identity, status: "published" })).toBe(false);
  });
});
