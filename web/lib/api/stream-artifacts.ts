import type { RuntimeArtifactResponse } from "@astra/sdk";
import type { ChatArtifactRef } from "@/lib/api/types";
import type { WebRuntimeClient } from "@/lib/runtime-client/server";

type JsonRecord = Record<string, unknown>;

const BASE64_PATTERN =
  /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}(?:==)?|[A-Za-z0-9+/]{3}=?)?$/;

function record(value: unknown): JsonRecord | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonRecord)
    : null;
}

function stringField(...values: unknown[]) {
  for (const value of values) {
    if (typeof value === "string" && value.trim()) {
      return value;
    }
  }
  return null;
}

function numberField(...values: unknown[]) {
  for (const value of values) {
    if (typeof value === "number" && Number.isFinite(value)) {
      return value;
    }
  }
  return null;
}

export function safeArtifactDownloadUrl(...values: unknown[]) {
  const value = stringField(...values);
  if (!value) {
    return null;
  }
  return (value.startsWith("/") && !value.startsWith("//")) ||
    /^https?:\/\//i.test(value)
    ? value
    : null;
}

function firstFilePart(artifact: JsonRecord) {
  if (!Array.isArray(artifact.parts)) {
    return null;
  }
  for (const rawPart of artifact.parts) {
    const file = record(record(rawPart)?.file);
    if (file) {
      return file;
    }
  }
  return null;
}

function inlineTextContent(artifact: JsonRecord) {
  if (!Array.isArray(artifact.parts)) {
    return null;
  }
  const text = artifact.parts
    .map((rawPart) => stringField(record(rawPart)?.text))
    .filter((value): value is string => Boolean(value))
    .join("\n");
  return text
    ? { encoding: "utf-8", data: text, content_type: "text/plain" }
    : null;
}

function inlineArtifactContent(data: JsonRecord) {
  if (data.encoding !== "base64") {
    return data;
  }
  const encoded = stringField(data.data);
  return encoded && BASE64_PATTERN.test(encoded) ? data : null;
}

function artifactFromToolResult(value: unknown): ChatArtifactRef | null {
  const artifact = record(value);
  if (!artifact) {
    return null;
  }
  const data = record(artifact.data);
  const metadata = record(artifact.metadata);
  const file = firstFilePart(artifact);
  const id = stringField(artifact.artifact_id, artifact.id);
  const kind = stringField(artifact.type, artifact.kind);
  if (!id || !kind) {
    return null;
  }
  const filename = stringField(
    data?.filename,
    metadata?.filename,
    file?.name,
    artifact.name,
  );
  const contentType = stringField(
    data?.content_type,
    data?.mime_type,
    metadata?.content_type,
    metadata?.mime_type,
    file?.mimeType,
  );
  return {
    id,
    kind,
    source: stringField(metadata?.source),
    title: stringField(artifact.name, artifact.description, filename),
    filename,
    sizeBytes: numberField(data?.byte_size, metadata?.byte_size),
    contentType,
    renderer: stringField(data?.renderer, metadata?.renderer),
    downloadFilename: stringField(
      data?.download_filename,
      metadata?.download_filename,
      filename,
    ),
    downloadUrl: safeArtifactDownloadUrl(data?.download_url, file?.uri),
    content: data
      ? inlineArtifactContent(data)
      : inlineTextContent(artifact),
    createdAt: stringField(artifact.created_at),
  };
}

export function artifactsFromValues(values: unknown): ChatArtifactRef[] {
  if (!Array.isArray(values)) {
    return [];
  }
  return values
    .map(artifactFromToolResult)
    .filter((artifact): artifact is ChatArtifactRef => Boolean(artifact));
}

export function artifactsFromToolCallEnd(
  event: Record<string, unknown>,
): ChatArtifactRef[] {
  if (event.type !== "tool_call_end") {
    return [];
  }
  return artifactsFromValues(event.artifacts);
}

function definedArtifactFields(artifact: ChatArtifactRef) {
  return Object.fromEntries(
    Object.entries(artifact).filter(
      ([, value]) => value !== null && value !== undefined,
    ),
  ) as Partial<ChatArtifactRef>;
}

export function mergeChatArtifacts(
  current: ChatArtifactRef[],
  incoming: ChatArtifactRef[],
) {
  if (incoming.length === 0) {
    return current;
  }
  const merged = [...current];
  const indexes = new Map(merged.map((artifact, index) => [artifact.id, index]));
  for (const artifact of incoming) {
    const index = indexes.get(artifact.id);
    if (index === undefined) {
      indexes.set(artifact.id, merged.length);
      merged.push(artifact);
    } else {
      merged[index] = { ...merged[index], ...definedArtifactFields(artifact) };
    }
  }
  return merged;
}

const INTERNAL_ARTIFACT_KINDS = new Set(["composite_snapshot_index"]);
const INTERNAL_ARTIFACT_SOURCES = new Set(["composite_snapshot_index"]);
const CHAT_VISIBLE_ARTIFACT_SOURCES = new Set(["publish_artifact"]);
const CHAT_VISIBLE_ARTIFACT_NORMALIZE_VERSIONS = new Set([
  "artifact_file_v1",
]);

function isChatVisibleRuntimeArtifact(
  source: string | null,
  kind: string,
  metadata: JsonRecord | null,
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
  const content = record(artifact.content);
  const metadata = record(artifact.metadata);
  const id = stringField(artifact.artifact_id);
  const kind = stringField(artifact.artifact_kind, content?.kind);
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
    title: stringField(content.title, metadata?.title, content.filename),
    filename: stringField(content.filename, metadata?.download_filename),
    sizeBytes: numberField(content.byte_size, metadata?.byte_size),
    contentType: stringField(content.content_type, metadata?.content_type),
    renderer: stringField(content.renderer, metadata?.renderer),
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
