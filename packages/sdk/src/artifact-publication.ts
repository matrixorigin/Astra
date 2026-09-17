import type { ArtifactPublicationV1 } from "./types";
export function isArtifactPublicationV1(value: unknown): value is ArtifactPublicationV1 {
  if (!value || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  const id = (x: unknown) => typeof x === "string" && x.length > 0 && x.length <= 256;
  if (v.type !== "artifact_publication" || v.schema_version !== 1 ||
      !id(v.run_id) || !id(v.turn_id) || typeof v.recorded !== "boolean" ||
      !Number.isSafeInteger(v.execution_owner_generation) || Number(v.execution_owner_generation) < 0 ||
      v.artifact_type !== "explain_analyze_snapshot") return false;
  return v.status === "published"
    ? typeof v.handle === "string" && /^artifact:\/\/session\/explain-analyze\/[a-f0-9]{64}$/i.test(v.handle)
    : v.status === "unavailable" && typeof v.reason_code === "string" &&
      v.reason_code.length > 0 && v.reason_code.length <= 64 &&
      typeof v.message === "string" && v.message.length > 0 && v.message.length <= 512 &&
      !/[\x00-\x1f\x7f-\x9f]/.test(v.message);
}
