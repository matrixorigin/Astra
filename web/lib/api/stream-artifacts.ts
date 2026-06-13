import type { RuntimeArtifactResponse } from "@astra/sdk";
import type { ChatArtifactRef } from "@/lib/api/types";
import type { WebRuntimeClient } from "@/lib/runtime-client/server";

function stringField(value: unknown) {
  return typeof value === "string" && value.trim() ? value : null;
}

function numberField(value: unknown) {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

const INTERNAL_ARTIFACT_KINDS = new Set(["composite_snapshot_index"]);
const INTERNAL_ARTIFACT_SOURCES = new Set(["composite_snapshot_index"]);
const CHAT_VISIBLE_ARTIFACT_SOURCES = new Set(["publish_artifact"]);
const CHAT_VISIBLE_ARTIFACT_NORMALIZE_VERSIONS = new Set(["artifact_file_v1"]);

function isChatVisibleRuntimeArtifact(
  source: string | null,
  kind: string,
  metadata: Record<string, unknown> | null,
) {
  if (
    INTERNAL_ARTIFACT_KINDS.has(kind) ||
    (source && INTERNAL_ARTIFACT_SOURCES.has(source))
  ) {
    return false;
  }

  const normalizeVersion = stringField(metadata?.normalize_version);
  return Boolean(
    source &&
    CHAT_VISIBLE_ARTIFACT_SOURCES.has(source) &&
    normalizeVersion &&
    CHAT_VISIBLE_ARTIFACT_NORMALIZE_VERSIONS.has(normalizeVersion),
  );
}

export function artifactFromRuntime(
  artifact: RuntimeArtifactResponse,
): ChatArtifactRef | null {
  const content =
    artifact.content && typeof artifact.content === "object"
      ? (artifact.content as Record<string, unknown>)
      : null;
  const metadata =
    artifact.metadata && typeof artifact.metadata === "object"
      ? artifact.metadata
      : null;
  const id = stringField(artifact.artifact_id);
  const kind =
    stringField(artifact.artifact_kind) ?? stringField(content?.kind);
  const source = stringField(artifact.source);
  if (!id || !kind || !content) {
    return null;
  }
  if (!isChatVisibleRuntimeArtifact(source, kind, metadata)) {
    return null;
  }
  return {
    id,
    kind,
    source,
    title:
      stringField(content.title) ??
      stringField(metadata?.title) ??
      stringField(content.filename),
    filename:
      stringField(content.filename) ?? stringField(metadata?.download_filename),
    sizeBytes:
      numberField(content.byte_size) ?? numberField(metadata?.byte_size),
    contentType:
      stringField(content.content_type) ?? stringField(metadata?.content_type),
    renderer: stringField(content.renderer) ?? stringField(metadata?.renderer),
    downloadFilename: stringField(metadata?.download_filename),
    content,
    createdAt: artifact.created_at ?? null,
  };
}

export async function fetchSessionArtifacts(
  client: WebRuntimeClient,
  sessionId: string,
) {
  const body = await client.sdk.listSessionArtifacts(sessionId, { limit: 50 });
  return (body.artifacts ?? [])
    .map(artifactFromRuntime)
    .filter((artifact): artifact is ChatArtifactRef => Boolean(artifact));
}
