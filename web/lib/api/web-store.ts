import type { StreamEventState } from "@/lib/api/stream-event-handler";
import { appendRunFailure, resumeStreamProjection } from "@/lib/api/stream-event-helpers";
import { AstraApiError, isArtifactPublicationV1 } from "@astra/sdk";
import { appendExplainFact, markExplainGap, beginExplainRepair, finishExplainRepair } from "@/lib/explain-analyze-observation";
import type { RuntimeConfig } from "@/lib/runtime-config";
import {
  artifactsFromValues,
  mergeChatArtifacts,
} from "@/lib/api/stream-artifacts";
import {
  type RunListResponse,
  type RunStatus,
  type RuntimeSessionListCursor,
  type RuntimeSessionListResponse,
  type RuntimeSessionResponse,
  type RuntimeTranscriptItemResponse,
} from "@astra/sdk";
import {
  activeRunPriority,
  isTerminalChatRunStatus,
  runBlocksChatTurn,
} from "@/lib/chat-run-state";
import {
  RuntimeClientError,
  runtimeErrorDetail,
} from "@/lib/runtime-client/errors";
import {
  WebRuntimeClient,
  getRuntimeClient,
  requireRuntimeClient,
} from "@/lib/runtime-client/server";
import {
  normalizeWorkspaceSelection,
  sameWorkspaceSelection,
} from "@/lib/workspace-authority";
import type {
  ChatDetail,
  StopRunRequest,
  ChatListResponse,
  ChatMessage,
  ChatSummary,
  ComposerOptions,
  CreateProjectRequest,
  KnowledgeFile,
  ProjectDetail,
  ProjectListResponse,
  ProjectSummary,
  SearchResponse,
  SidebarData,
  UserSummary,
  WorkspaceSelection,
} from "@/lib/api/types";
import {
  makeActiveRunRecord,
  sameRunBinding,
  type ChatRunBinding,
  mergeRunStreamBinding,
  normalizeNextEventIndex,
  type ChatActiveRunRecord,
} from "@/lib/api/active-run-merge";
import { settleRuntimeCancel } from "@/lib/api/runtime-cancel-settlement";

type ChatRecord = ChatSummary & {
  createdAt: string;
  archivedAt?: string | null;
  backendSessionId?: string | null;
  messages: ChatMessage[];
  streamProjection?: ChatStreamProjection;
  workspaceSelection?: WorkspaceSelection;
  activeRun?: ChatActiveRunRecord;
  locallyStoppedRuns?: Record<string, string>;
  pendingTurn?: {
    messageId: string;
    content: string;
    options: ComposerOptions;
  };
};

type ChatStreamProjection = {
  assistantMessageId: string;
  sessionId: string;
  state: StreamEventState;
  writer?: AbortController;
};

export type ChatStreamProjectionLease = {
  ownerUserId: string;
  chatId: string;
  projection: ChatStreamProjection;
  writer: AbortController;
};

export class ChatStreamProjectionError extends Error {
  constructor(readonly code: "stream_projection_busy" | "stream_continuation_unavailable") {
    super(code === "stream_projection_busy"
      ? "This message is already being observed by another connection."
      : "The complete stream continuation is unavailable. Reopen the session to inspect its recorded state.");
  }
}

/** One current raw projection, owned by the existing chat rather than a physical reader. */
export function claimChatStreamProjection(ownerUserId: string, chatId: string, assistantMessageId: string,
  writer: AbortController, binding: { sessionId: string; runId?: string; nextEventIndex?: number }) {
  const chat = getStore(ownerUserId).chats.find(item => item.id === chatId);
  if (!chat?.messages.some(message => message.id === assistantMessageId && message.role === "assistant")) {
    throw new ChatStreamProjectionError("stream_continuation_unavailable");
  }
  const projection = chat.streamProjection;
  if (projection?.writer && !projection.writer.signal.aborted) {
    throw new ChatStreamProjectionError("stream_projection_busy");
  }
  if (projection && (projection.assistantMessageId !== assistantMessageId ||
    projection.sessionId && binding.sessionId && projection.sessionId !== binding.sessionId ||
    binding.runId && projection.state.runId !== binding.runId)) {
    throw new ChatStreamProjectionError("stream_continuation_unavailable");
  }
  if (!projection) throw new ChatStreamProjectionError("stream_continuation_unavailable");
  if (projection.state.protocolError || (binding.nextEventIndex ?? 0) > (projection.state.nextEventIndex ?? 0)) {
    throw new ChatStreamProjectionError("stream_continuation_unavailable");
  }
  projection.writer = writer;
  return { ownerUserId, chatId, projection, writer } satisfies ChatStreamProjectionLease;
}

export function withChatStreamProjection(lease: ChatStreamProjectionLease, apply: () => void): boolean {
  const chat = getStore(lease.ownerUserId).chats.find(item => item.id === lease.chatId);
  if (chat?.streamProjection !== lease.projection || lease.projection.writer !== lease.writer ||
    lease.writer.signal.aborted) return false;
  try { apply(); } catch (error) { lease.projection.state.protocolError = true; throw error; }
  return true;
}

export function releaseChatStreamProjection(lease: ChatStreamProjectionLease) {
  if (lease.projection.writer === lease.writer) lease.projection.writer = undefined;
}

function discardChatStreamProjection(chat: ChatRecord) {
  chat.streamProjection?.writer?.abort();
  chat.streamProjection = undefined;
}

export class StaleDeferredRunError extends Error {
  constructor(message = "No active run is available for deferred input.") {
    super(message);
    this.name = "StaleDeferredRunError";
  }
}

type ProjectRecord = ProjectDetail["project"];

type Store = {
  projects: ProjectRecord[];
  chats: ChatRecord[];
  files: Record<string, KnowledgeFile[]>;
};

const LOCAL_ACTIVE_RUN_GRACE_MS = 30_000;
const LOCAL_CANCELLING_STATUS_GRACE_MS = 15_000;
const LOCAL_STOPPED_RUN_GRACE_MS = 30 * 60_000;
const SESSION_SYNC_PAGE_SIZE = 200;
const RUN_SYNC_PAGE_SIZE = 200;
const CHAT_HISTORY_LIMIT = 200;
const MAX_DEFERRED_INPUT_CHARS = 20_000;
const LEGACY_LOCAL_CHAT_IDS = new Set(["chat-web-agent-notes"]);
const WORKSPACE_SELECTION_METADATA_KEY = "workspace_selection";

export type ModelOfferingSelectionErrorCode =
  | "invalid_selection"
  | "authentication_required";

export class ModelOfferingSelectionError extends Error {
  constructor(
    readonly code: ModelOfferingSelectionErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "ModelOfferingSelectionError";
  }
}

function runtimeOperationError(operation: string, error: unknown) {
  return new Error(`${operation}: ${runtimeErrorDetail(error)}`);
}

export function requireSelectedOfferingId(model?: string | null): string {
  const normalized = model?.trim();
  if (!normalized) {
    throw new ModelOfferingSelectionError(
      "invalid_selection",
      "Select an available Model Offering before starting a turn.",
    );
  }
  return normalized;
}

declare global {
  var __astraWebStores: Record<string, Store> | undefined;
}

const DEFAULT_STORE_SCOPE = "offline";

function nowIso() {
  return new Date().toISOString();
}

function isStoppableAssistantMessage(message: ChatMessage) {
  return (
    message.role === "assistant" &&
    message.status !== "failed" &&
    (message.status === "streaming" ||
      message.reasoningStatus === "streaming" ||
      message.status === undefined)
  );
}

function stoppedAssistantMessageController(assistantMessageId: string | null) {
  let snapshot: { message: ChatMessage; applied: ChatMessage; lastMessageAt: string;
    lastMessagePreview: ChatRecord["lastMessagePreview"]; appliedAt: string } | undefined;
  return {
    complete(chat: ChatRecord) {
      if (snapshot || !assistantMessageId) return false;
      const message = chat.messages.find(item => item.id === assistantMessageId && isStoppableAssistantMessage(item));
      if (!message) return false;
      const previous = { ...message };
      const timestamp = nowIso();
      Object.assign(message, {
        content: message.content.trim()
          ? `${message.content}${message.content.endsWith("\n") ? "" : "\n"}\nStopped.` : "Stopped.",
        status: "complete", completedAt: timestamp, reasoningStatus: "complete",
      });
      snapshot = { message: previous, applied: { ...message }, lastMessageAt: chat.lastMessageAt,
        lastMessagePreview: chat.lastMessagePreview, appliedAt: timestamp };
      chat.lastMessageAt = timestamp;
      chat.lastMessagePreview = message.content;
      return true;
    },
    restore(chat: ChatRecord) {
      if (!snapshot) return;
      const { message: previous, applied } = snapshot;
      const message = chat.messages.find(item => item.id === previous.id);
      if (!message) return;
      Object.assign(message, Object.fromEntries((["content", "status", "completedAt", "reasoningStatus"] as const)
        .filter(key => message[key] === applied[key]).map(key => [key, previous[key]])));
      if (chat.lastMessageAt === snapshot.appliedAt) chat.lastMessageAt = snapshot.lastMessageAt;
      if (chat.lastMessagePreview === applied.content) chat.lastMessagePreview = snapshot.lastMessagePreview;
    },
  };
}

function titleFromMessage(message: string) {
  const text = message.trim().replace(/\s+/g, " ");
  if (!text) {
    return null;
  }
  return text.length > 56 ? `${text.slice(0, 53)}...` : text;
}

export function normalizedActiveSkills(skills?: string[]) {
  if (!Array.isArray(skills)) {
    return [];
  }
  return [...new Set(skills.map((skill) => skill.trim()).filter(Boolean))].sort(
    (left, right) => left.localeCompare(right),
  );
}

export function normalizedActiveTools(tools?: string[], webSearch = false) {
  const normalized = Array.isArray(tools)
    ? tools.map((tool) => tool.trim()).filter(Boolean)
    : [];
  if (webSearch) {
    normalized.push("web_search", "web_fetch");
  }
  return [...new Set(normalized)].sort((left, right) =>
    left.localeCompare(right),
  );
}

function isFreshLocalActiveRun(
  activeRun: ChatActiveRunRecord,
  now = Date.now(),
) {
  if (activeRun.source === "backend_poll") {
    return false;
  }
  const observedAt = Date.parse(activeRun.observedAt);
  const maxAge =
    activeRun.status.trim().toLowerCase() === "cancelling"
      ? LOCAL_CANCELLING_STATUS_GRACE_MS
      : LOCAL_ACTIVE_RUN_GRACE_MS;
  return Number.isFinite(observedAt) && now - observedAt <= maxAge;
}

function pruneLocallyStoppedRuns(chat: ChatRecord, now = Date.now()) {
  const stoppedRuns = chat.locallyStoppedRuns;
  if (!stoppedRuns) {
    return;
  }
  for (const [runId, observedAt] of Object.entries(stoppedRuns)) {
    const stoppedAt = Date.parse(observedAt);
    if (
      !Number.isFinite(stoppedAt) ||
      now - stoppedAt > LOCAL_STOPPED_RUN_GRACE_MS
    ) {
      delete stoppedRuns[runId];
    }
  }
  if (Object.keys(stoppedRuns).length === 0) {
    chat.locallyStoppedRuns = undefined;
  }
}

function rememberLocallyStoppedRun(chat: ChatRecord, runId: string) {
  pruneLocallyStoppedRuns(chat);
  chat.locallyStoppedRuns = {
    ...(chat.locallyStoppedRuns ?? {}),
    [runId]: nowIso(),
  };
}

function forgetLocallyStoppedRun(chat: ChatRecord, runId: string) {
  if (!chat.locallyStoppedRuns) {
    return;
  }
  delete chat.locallyStoppedRuns[runId];
  if (Object.keys(chat.locallyStoppedRuns).length === 0) {
    chat.locallyStoppedRuns = undefined;
  }
}

function isLocallyStoppedRun(
  chat: ChatRecord | undefined,
  runId: string,
  now = Date.now(),
) {
  const observedAt = chat?.locallyStoppedRuns?.[runId];
  if (!observedAt) {
    return false;
  }
  const stoppedAt = Date.parse(observedAt);
  return (
    Number.isFinite(stoppedAt) && now - stoppedAt <= LOCAL_STOPPED_RUN_GRACE_MS
  );
}

async function reconcileStoppedRun(
  runId: string,
  ownedChat: () => ChatRecord | undefined,
  admittedClient?: WebRuntimeClient,
): Promise<void> {
  if (!ownedChat()) return;
  const client = admittedClient ?? await getRuntimeClient({
    auth: "required", operation: `reconcile stopped run ${runId}`,
  }).catch(() => null);
  if (!client || !ownedChat()) return;
  let status: RunStatus;
  try {
    status = await client.sdk.getRunStatus(runId);
  } catch (error) {
    const chat = ownedChat();
    if (!chat) return;
    if (error instanceof RuntimeClientError && error.status && [404, 409, 410].includes(error.status)) {
      forgetLocallyStoppedRun(chat, runId);
      chat.activeRun = undefined;
    } else {
      console.warn("Failed to reconcile stopped run:", runtimeErrorDetail(error));
    }
    return;
  }
  const chat = ownedChat();
  if (!chat || status.runId !== runId || status.sessionId !== backendSessionIdForChat(chat)) return;
  if (isTerminalChatRunStatus(status.status)) {
    forgetLocallyStoppedRun(chat, runId);
    chat.activeRun = undefined;
  }
  // Nonterminal polling cannot reverse an admitted local cancellation. The
  // existing stopped-run marker and hydration owner retain that intent.
}

function compareActiveRunDeterministically(
  left: ChatActiveRunRecord,
  right: ChatActiveRunRecord,
) {
  const priorityDelta = activeRunPriority(left) - activeRunPriority(right);
  if (priorityDelta !== 0) {
    return priorityDelta;
  }
  return left.runId.localeCompare(right.runId);
}

function mergeActiveRunRecord(params: {
  existing?: ChatActiveRunRecord;
  backend?: ChatActiveRunRecord;
  backendRunStatuses: Map<string, string>;
  now?: number;
}): ChatActiveRunRecord | undefined {
  const { existing, backend, backendRunStatuses, now = Date.now() } = params;
  if (!existing) return backend;
  if (backend && existing.runId !== backend.runId) return backend;
  if (isTerminalChatRunStatus(backendRunStatuses.get(existing.runId))) return undefined;
  if (!backend) {
    return runBlocksChatTurn(existing.status) && isFreshLocalActiveRun(existing, now)
      ? existing : undefined;
  }
  if (
    existing.source !== "backend_poll" &&
    compareActiveRunDeterministically(existing, backend) >= 0
  ) {
    return existing;
  }
  return mergeRunStreamBinding(backend, existing);
}

function backendSessionIdForChat(chat: ChatRecord) {
  return chat.backendSessionId ?? chat.id;
}

function publicActiveRun(
  chat: ChatRecord,
): ChatDetail["activeRun"] {
  const activeRun = chat.activeRun;
  if (!activeRun) {
    return undefined;
  }
  const assistant = chat.messages.find(message =>
    message.id === activeRun.assistantMessageId && message.role === "assistant");
  const projection = chat.streamProjection;
  const nextEventIndex = assistant && projection && !projection.state.protocolError &&
    projection.assistantMessageId === assistant.id && projection.state.runId === activeRun.runId &&
    projection.sessionId === backendSessionIdForChat(chat)
    ? normalizeNextEventIndex(projection.state.nextEventIndex) : null;
  return {
    runId: activeRun.runId,
    status: activeRun.status,
    waitingFor: activeRun.waitingFor ?? null,
    ...(assistant ? { assistantMessageId: assistant.id } : {}),
    ...(nextEventIndex !== null ? { nextEventIndex } : {}),
  };
}

function hasContinuingStream(chat: ChatRecord): boolean {
  const run = chat.activeRun;
  const state = chat.streamProjection?.state;
  return publicActiveRun(chat)?.nextEventIndex !== undefined &&
    run?.status.trim().toLowerCase() !== "cancelling" && run?.waitingFor !== "cancel_requested" &&
    chat.messages.some(message => message.id === run?.assistantMessageId && message.status === "streaming") &&
    state?.lastStatus === "streaming" && state.runLifecycle !== "finished" &&
    state.runLifecycle !== "paused" && !state.terminalStatus;
}

function workspaceSelectionMetadata(selection: WorkspaceSelection) {
  if (selection.kind === "server_sandbox") {
    return { kind: "server_sandbox" };
  }
  return {
    kind: "edge_workspace",
    edgeAgentId: selection.edgeAgentId,
    displayName: selection.displayName ?? null,
    cwd: selection.cwd,
  };
}

function hasWorkspaceSelectionMetadata(session: RuntimeSessionResponse) {
  return Object.prototype.hasOwnProperty.call(
    session.metadata ?? {},
    WORKSPACE_SELECTION_METADATA_KEY,
  );
}

function seedStore(): Store {
  const now = nowIso();
  const projectId = "project-web-agent";
  return {
    projects: [
      {
        id: projectId,
        name: "Web agent workspace",
        description: "Session durability, context, and remote agent UI notes.",
        instructions:
          "Prefer concise implementation notes. Keep session state durable and auditable.",
        memory: "The user is validating the Astra web agent v1 workflow.",
        visibility: "private",
        starred: true,
        createdAt: now,
        updatedAt: now,
      },
    ],
    chats: [],
    files: {
      [projectId]: [],
    },
  };
}

export function getStore(ownerUserId = DEFAULT_STORE_SCOPE) {
  globalThis.__astraWebStores ??= {};
  globalThis.__astraWebStores[ownerUserId] ??= seedStore();
  const store = globalThis.__astraWebStores[ownerUserId];
  normalizeCanonicalChatIds(store);
  return store;
}

export function getCurrentUser(): UserSummary {
  return {
    id: "local-user",
    name: "Astra user",
    plan: "free",
  };
}

export async function listChats(
  ownerUserId: string,
  params: {
    projectId?: string | null;
    q?: string | null;
    cursor?: string | null;
    limit?: number;
    archived?: boolean;
  },
  client?: WebRuntimeClient,
): Promise<ChatListResponse> {
  await syncBackendSessions(ownerUserId, client);
  const store = getStore(ownerUserId);
  const limit = params.limit ?? 50;
  const offset = Number(params.cursor ?? 0);
  const hasProjectFilter = params.projectId !== undefined;
  const query = params.q?.trim().toLowerCase();
  const archived = params.archived ?? false;
  let items = store.chats.filter((chat) => {
    if (Boolean(chat.archivedAt) !== archived) {
      return false;
    }
    if (hasProjectFilter) {
      const expected =
        params.projectId === "null" ? null : (params.projectId ?? null);
      if (chat.projectId !== expected) {
        return false;
      }
    }
    if (query) {
      const haystack =
        `${chat.title ?? ""} ${chat.lastMessagePreview ?? ""}`.toLowerCase();
      return haystack.includes(query);
    }
    return true;
  });
  items = items.sort((a, b) => b.lastMessageAt.localeCompare(a.lastMessageAt));
  const page = items.slice(offset, offset + limit).map(chatSummary);
  const nextOffset = offset + limit;
  return {
    items: page,
    nextCursor: nextOffset < items.length ? String(nextOffset) : null,
  };
}

export function listProjects(
  ownerUserId: string,
  params: {
    q?: string | null;
    sort?: "activity" | "created" | "name";
    cursor?: string | null;
    limit?: number;
  },
): ProjectListResponse {
  const store = getStore(ownerUserId);
  const limit = params.limit ?? 24;
  const offset = Number(params.cursor ?? 0);
  const query = params.q?.trim().toLowerCase();
  const sort = params.sort ?? "activity";
  let projects = store.projects.filter((project) => {
    if (!query) {
      return true;
    }
    return `${project.name} ${project.description ?? ""}`
      .toLowerCase()
      .includes(query);
  });
  projects = projects.sort((a, b) => {
    if (sort === "name") {
      return a.name.localeCompare(b.name);
    }
    if (sort === "created") {
      return b.createdAt.localeCompare(a.createdAt);
    }
    return b.updatedAt.localeCompare(a.updatedAt);
  });
  const nextOffset = offset + limit;
  return {
    items: projects.slice(offset, nextOffset).map(projectSummary),
    nextCursor: nextOffset < projects.length ? String(nextOffset) : null,
  };
}

export function createProject(
  ownerUserId: string,
  payload: CreateProjectRequest,
) {
  const store = getStore(ownerUserId);
  const timestamp = nowIso();
  const project: ProjectRecord = {
    id: crypto.randomUUID(),
    name: payload.name.trim(),
    description: payload.description?.trim() || null,
    instructions: payload.instructions?.trim() || null,
    memory: null,
    visibility: "private",
    starred: false,
    createdAt: timestamp,
    updatedAt: timestamp,
  };
  store.projects.unshift(project);
  store.files[project.id] = [];
  return projectSummary(project);
}

export function getProject(
  ownerUserId: string,
  projectId: string,
): ProjectDetail | null {
  const store = getStore(ownerUserId);
  const project = store.projects.find((item) => item.id === projectId);
  if (!project) {
    return null;
  }
  return {
    project,
    chats: store.chats
      .filter((chat) => chat.projectId === projectId && !chat.archivedAt)
      .sort((a, b) => b.lastMessageAt.localeCompare(a.lastMessageAt))
      .map(chatSummary),
    files: store.files[projectId] ?? [],
  };
}

export async function getProjectHydrated(
  ownerUserId: string,
  projectId: string,
  client?: WebRuntimeClient,
): Promise<ProjectDetail | null> {
  await syncBackendSessions(ownerUserId, client);
  return getProject(ownerUserId, projectId);
}

export function updateProject(
  ownerUserId: string,
  projectId: string,
  payload: Partial<CreateProjectRequest>,
) {
  const store = getStore(ownerUserId);
  const project = store.projects.find((item) => item.id === projectId);
  if (!project) {
    return null;
  }
  if (payload.name !== undefined) {
    project.name = payload.name.trim();
  }
  if (payload.description !== undefined) {
    project.description = payload.description?.trim() || null;
  }
  if (payload.instructions !== undefined) {
    project.instructions = payload.instructions?.trim() || null;
  }
  project.updatedAt = nowIso();
  return getProject(ownerUserId, projectId);
}

export function setProjectStar(
  ownerUserId: string,
  projectId: string,
  starred: boolean,
) {
  const store = getStore(ownerUserId);
  const project = store.projects.find((item) => item.id === projectId);
  if (!project) {
    return null;
  }
  project.starred = starred;
  project.updatedAt = nowIso();
  return { starred };
}

export function addProjectFile(
  ownerUserId: string,
  projectId: string,
  file: File,
): KnowledgeFile | null {
  const store = getStore(ownerUserId);
  if (!store.projects.some((project) => project.id === projectId)) {
    return null;
  }
  const timestamp = nowIso();
  const record: KnowledgeFile = {
    id: crypto.randomUUID(),
    filename: file.name,
    mimeType: file.type || "application/octet-stream",
    sizeBytes: file.size,
    sourceType: "upload",
    indexStatus: "indexed",
    indexedAt: timestamp,
    createdAt: timestamp,
  };
  store.files[projectId] ??= [];
  store.files[projectId].unshift(record);
  touchProjectInStore(store, projectId);
  return record;
}

export function removeProjectFile(
  ownerUserId: string,
  projectId: string,
  fileId: string,
) {
  const store = getStore(ownerUserId);
  const files = store.files[projectId];
  if (!files) {
    return false;
  }
  const before = files.length;
  store.files[projectId] = files.filter((file) => file.id !== fileId);
  if (store.files[projectId].length !== before) {
    touchProjectInStore(store, projectId);
    return true;
  }
  return false;
}

/**
 * Synchronous in-memory snapshot. Route/UI detail reads should prefer
 * getChatHydrated so backend sessions, runs, and transcripts are reconciled.
 */
export function getChat(
  ownerUserId: string,
  chatId: string,
): ChatDetail | null {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  const project = chat.projectId
    ? store.projects.find((item) => item.id === chat.projectId)
    : undefined;
  return {
    chat: {
      id: chat.id,
      title: chat.title,
      projectId: chat.projectId,
      createdAt: chat.createdAt,
      updatedAt: chat.lastMessageAt,
      archivedAt: chat.archivedAt ?? null,
      model: chat.model ?? null,
    },
    messages: chat.messages,
    session: {
      chatId: chat.id,
      backendSessionId: chat.backendSessionId ?? null,
      persisted: Boolean(chat.backendSessionId),
      messageCount: chat.messages.length,
    },
    project: project ? { id: project.id, name: project.name } : undefined,
    activeRun: publicActiveRun(chat),
    pendingTurn: chat.pendingTurn,
    workspaceSelection: chat.workspaceSelection,
    workspaceSelectionExplicit: Boolean(chat.workspaceSelection),
  };
}

export async function getChatHydrated(
  ownerUserId: string,
  chatId: string,
  client?: WebRuntimeClient,
): Promise<ChatDetail | null> {
  const canonicalId = await syncBackendTranscript(ownerUserId, chatId, client);
  return getChat(ownerUserId, canonicalId ?? chatId);
}

export async function createChatWithMessage(
  ownerUserId: string,
  payload: {
    message: string;
    model: string;
    options: Omit<ComposerOptions, "model">;
    projectId?: string | null;
    workspaceSelection?: WorkspaceSelection | null;
  },
) {
  const store = getStore(ownerUserId);
  const timestamp = nowIso();
  const projectId = payload.projectId ?? null;
  const workspaceSelection = payload.workspaceSelection
    ? normalizeWorkspaceSelection(payload.workspaceSelection)
    : undefined;
  if (
    projectId &&
    !store.projects.some((project) => project.id === projectId)
  ) {
    throw new Error(`Cannot create chat: project ${projectId} was not found.`);
  }
  const title = titleFromMessage(payload.message);
  const userMessage: ChatMessage = {
    id: crypto.randomUUID(),
    role: "user",
    content: payload.message,
    activeSkills: payload.options.activeSkills,
    activeTools: normalizedActiveTools(
      payload.options.activeTools,
      payload.options.webSearch,
    ),
    createdAt: timestamp,
    status: "complete",
  };
  const chat: ChatRecord = {
    id: `web-${crypto.randomUUID()}`,
    title,
    projectId,
    createdAt: timestamp,
    lastMessageAt: timestamp,
    lastMessagePreview: payload.message,
    model: payload.model,
    backendSessionId: null,
    ...(workspaceSelection ? { workspaceSelection } : {}),
    messages: [userMessage],
    activeRun: undefined,
    pendingTurn: {
      messageId: userMessage.id,
      content: payload.message,
      options: {
        ...payload.options,
        model: payload.model,
      },
    },
  };

  store.chats.unshift(chat);
  if (chat.projectId) {
    touchProjectInStore(store, chat.projectId);
  }
  return {
    chatId: chat.id,
    messageId: userMessage.id,
  };
}

export function beginStreamingMessage(
  ownerUserId: string,
  chatId: string,
  payload: {
    content: string;
    options?: ComposerOptions;
    pendingMessageId?: string;
    workspaceSelection?: WorkspaceSelection;
  },
) {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  if (chat.streamProjection?.writer && !chat.streamProjection.writer.signal.aborted &&
    chat.streamProjection.state.runLifecycle !== "finished") {
    throw new ChatStreamProjectionError("stream_projection_busy");
  }

  const timestamp = nowIso();
  const pendingUserMessage =
    payload.pendingMessageId &&
    chat.pendingTurn?.messageId === payload.pendingMessageId
      ? chat.messages.find(
          (item) =>
            item.id === payload.pendingMessageId && item.role === "user",
        )
      : undefined;
  if (payload.pendingMessageId && !pendingUserMessage) {
    return null;
  }
  const userMessage: ChatMessage = pendingUserMessage ?? {
    id: crypto.randomUUID(),
    role: "user",
    content: payload.content,
    activeSkills: payload.options?.activeSkills,
    activeTools: normalizedActiveTools(payload.options?.activeTools, payload.options?.webSearch),
    createdAt: timestamp,
    status: "complete",
  };
  if (pendingUserMessage && payload.options?.activeSkills?.length) {
    pendingUserMessage.activeSkills = payload.options.activeSkills;
  }
  const assistantMessage = streamingAssistantMessage(chat, crypto.randomUUID(), timestamp,
    { sessionId: chat.backendSessionId ?? "", nextEventIndex: 0 });

  if (pendingUserMessage) {
    chat.messages.push(assistantMessage);
    chat.pendingTurn = undefined;
  } else {
    chat.messages.push(userMessage, assistantMessage);
  }
  chat.lastMessageAt = timestamp;
  chat.lastMessagePreview = payload.content;
  commitChatMetadata(ownerUserId, chat, {
    title: chat.title ?? titleFromMessage(payload.content),
    ...(payload.options?.model ? { model: payload.options.model } : {}),
    ...(payload.workspaceSelection ? { workspaceSelection: payload.workspaceSelection } : {}),
  });
  if (chat.projectId) {
    touchProjectInStore(store, chat.projectId);
  }

  return {
    userMessage,
    assistantMessage,
    sessionId: chat.id,
  };
}

export function setChatActiveRun(
  ownerUserId: string,
  chatId: string,
  activeRun?: ChatDetail["activeRun"] | ChatActiveRunRecord,
) {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  chat.activeRun = activeRun
    ? mergeRunStreamBinding(
        makeActiveRunRecord(
          activeRun,
          "source" in activeRun ? activeRun.source : "stream",
          "observedAt" in activeRun ? activeRun.observedAt : nowIso(),
        ),
        chat.activeRun,
      )
    : undefined;
  if (activeRun?.runId) {
    forgetLocallyStoppedRun(chat, activeRun.runId);
  }
  return chat.activeRun;
}

/**
 * Merge a stream run update into an existing activeRun record, preserving
 * nextEventIndex for event replay continuity. Used by both store and React
 * hooks to ensure consistent activeRun state across layers.
 */
export function mergeStreamRunUpdate(
  run: {
    runId: string;
    status: string;
    waitingFor?: string | null;
    assistantMessageId?: string | null;
    nextEventIndex?: number | null;
  },
  existingActiveRun: ChatDetail["activeRun"] | undefined,
): NonNullable<ChatDetail["activeRun"]> {
  const base = makeActiveRunRecord(
    {
      runId: run.runId,
      status: run.status,
      assistantMessageId: run.assistantMessageId,
      nextEventIndex: run.nextEventIndex,
      waitingFor: run.waitingFor,
    },
    "stream",
  );

  return mergeRunStreamBinding(base, existingActiveRun);
}

function deferredRunInputIdempotencyKey(runId: string, userMessageId: string) {
  return `web-deferred:${runId}:${userMessageId}`;
}

function streamingAssistantMessage(chat: ChatRecord, id: string, createdAt: string,
  binding: { sessionId: string; runId?: string; nextEventIndex: number }): ChatMessage {
  discardChatStreamProjection(chat);
  chat.streamProjection = { assistantMessageId: id, sessionId: binding.sessionId, state: {
    assistantText: "", assistantRawText: "", reasoningText: "", lastStatus: "streaming",
    protocolError: false, runLifecycle: "running", runId: binding.runId, nextEventIndex: binding.nextEventIndex,
  } };
  return {
    id,
    role: "assistant",
    content: "",
    createdAt,
    status: "streaming",
    reasoning: "",
    reasoningStatus: "streaming",
  };
}

function findAssistantMessageAfter(
  messages: ChatMessage[],
  userMessageId: string,
) {
  const userIndex = messages.findIndex(
    (message) => message.id === userMessageId,
  );
  if (userIndex === -1) {
    return undefined;
  }
  for (const message of messages.slice(userIndex + 1)) {
    if (message.role === "user") break;
    if (message.role === "assistant") return message;
  }
  return undefined;
}

export async function queueDeferredRunInput(
  ownerUserId: string,
  chatId: string,
  payload: {
    content: string;
    options?: ComposerOptions;
    pendingMessageId?: string;
  },
  admittedClient?: WebRuntimeClient,
) {
  if ([...payload.content].length > MAX_DEFERRED_INPUT_CHARS) {
    throw new Error("Deferred input is too large.");
  }
  await syncBackendSessions(ownerUserId, admittedClient);

  // Re-read chat after every async boundary to avoid TOCTOU races.
  function findChat(): ChatRecord | undefined {
    return getStore(ownerUserId).chats.find((item) => item.id === chatId);
  }

  let chat = findChat();
  if (!chat) {
    return null;
  }
  if (!chat.activeRun?.runId) {
    throw new StaleDeferredRunError();
  }

  const userMessageId =
    typeof payload.pendingMessageId === "string" &&
    payload.pendingMessageId.trim()
      ? payload.pendingMessageId.trim()
      : crypto.randomUUID();
  const existingResponse = (current: ChatRecord) => {
    const userMessage = current.messages.find(message => message.id === userMessageId && message.role === "user");
    const assistantMessage = userMessage && findAssistantMessageAfter(current.messages, userMessageId);
    // Idempotent acknowledgement must not reset a live projection or reselect an older message.
    return userMessage && assistantMessage ? { userMessage, assistantMessage, activeRun: publicActiveRun(current) } : undefined;
  };
  const previous = existingResponse(chat);
  if (previous) return previous;

  const activeRunId = chat.activeRun.runId;
  const client = admittedClient ?? await requireRuntimeClient({
    auth: "required",
    operation: `submit deferred run input for ${activeRunId}`,
  });

  // Verify chat hasn't changed after obtaining the client.
  chat = findChat();
  if (!chat?.activeRun || chat.activeRun.runId !== activeRunId) {
    throw new StaleDeferredRunError(
      "The run changed before input could be submitted.",
    );
  }

  let runStatus: RunStatus;
  try {
    runStatus = await client.sdk.getRunStatus(activeRunId);
  } catch (error) {
    if (
      error instanceof RuntimeClientError &&
      error.status &&
      [404, 409, 410].includes(error.status)
    ) {
      chat.activeRun = undefined;
      throw new StaleDeferredRunError("The previous run is no longer active.");
    }
    throw error;
  }

  // Verify chat hasn't changed after getRunStatus.
  chat = findChat();
  if (!chat?.activeRun || chat.activeRun.runId !== activeRunId) {
    throw new StaleDeferredRunError("The run changed during status check.");
  }

  if (
    runStatus.runId !== activeRunId || runStatus.sessionId !== backendSessionIdForChat(chat) ||
    !runBlocksChatTurn(runStatus.status)
  ) {
    chat.activeRun = undefined;
    throw new StaleDeferredRunError("The previous run is no longer active.");
  }
  const nextEventIndex = normalizeNextEventIndex(runStatus.eventsCount);
  if (nextEventIndex === null) throw new Error("The run did not supply a valid continuation cursor.");
  const accepted = existingResponse(chat);
  if (accepted) return accepted;
  chat.activeRun = makeActiveRunRecord(
    {
      runId: runStatus.runId,
      status: runStatus.status,
      waitingFor: runStatus.waitingFor ?? null,
      assistantMessageId: chat.activeRun.assistantMessageId ?? null,
      nextEventIndex: chat.activeRun.nextEventIndex ?? null,
    },
    "backend_poll",
  );

  const activeSkills = normalizedActiveSkills(payload.options?.activeSkills);
  const activeTools = normalizedActiveTools(
    payload.options?.activeTools,
    payload.options?.webSearch,
  );
  if (!chat.messages.some(message => message.id === userMessageId && message.role === "user")) {
    try {
      await client.sdk.submitRunInput(activeRunId, {
        idempotencyKey: deferredRunInputIdempotencyKey(
          activeRunId,
          userMessageId,
        ),
        input: {
          content: payload.content,
          active_skills: activeSkills,
          active_tools: activeTools,
        },
      });
    } catch (error) {
      if (
        error instanceof RuntimeClientError &&
        error.status &&
        [404, 409, 410].includes(error.status)
      ) {
        chat.activeRun = undefined;
        throw new StaleDeferredRunError("The previous run is no longer active.");
      }
      throw error;
    }
  }

  // Final verification after submitRunInput.
  chat = findChat();
  if (!chat?.activeRun || chat.activeRun.runId !== activeRunId ||
    backendSessionIdForChat(chat) !== runStatus.sessionId) {
    throw new StaleDeferredRunError("The run changed after input submission.");
  }

  const concurrent = existingResponse(chat);
  if (concurrent) return concurrent;
  const existingUser = chat.messages.find(message => message.id === userMessageId && message.role === "user");
  if (existingUser && chat.messages.slice(chat.messages.indexOf(existingUser) + 1).some(message => message.role === "user")) {
    throw new Error("The queued input was superseded by a newer message.");
  }
  const timestamp = nowIso();
  const userMessage: ChatMessage = existingUser ?? {
    id: userMessageId, role: "user", content: payload.content,
    activeSkills: activeSkills.length ? activeSkills : undefined,
    activeTools: activeTools.length ? activeTools : undefined,
    createdAt: timestamp, status: "complete",
  };
  const assistantMessage = streamingAssistantMessage(chat, crypto.randomUUID(), timestamp,
    { sessionId: runStatus.sessionId, runId: activeRunId, nextEventIndex });
  if (!existingUser) chat.messages.push(userMessage);
  chat.messages.push(assistantMessage);
  chat.lastMessageAt = timestamp;
  chat.lastMessagePreview = payload.content;
  if (chat.projectId) {
    touchProjectInStore(getStore(ownerUserId), chat.projectId);
  }
  chat.activeRun = makeActiveRunRecord(
    {
      runId: chat.activeRun.runId,
      status: "input-queued",
      waitingFor: "user_input",
      assistantMessageId: assistantMessage.id,
      nextEventIndex,
    },
    "local_mutation",
  );

  return {
    userMessage,
    assistantMessage,
    activeRun: publicActiveRun(chat),
  };
}

function storedRunBinding(chat: ChatRecord): ChatRunBinding | null {
  return chat.activeRun?.runId ? {
    chatId: chat.id, sessionId: backendSessionIdForChat(chat), runId: chat.activeRun.runId,
    assistantMessageId: chat.activeRun.assistantMessageId ?? null,
  } : null;
}

export class StaleRunBindingError extends Error {
  constructor() { super("The selected run changed while Stop was in progress."); }
}

export async function stopActiveRun(
  ownerUserId: string,
  chatId: string,
  requested: StopRunRequest,
  options?: { skipSync?: boolean; cancelTimeoutMs?: number; client?: WebRuntimeClient },
) {
  if (!options?.skipSync) await syncBackendSessions(ownerUserId, options?.client);
  const findChat = () => getStore(ownerUserId).chats.find(item => item.id === chatId);
  if (!findChat()) return null;
  const binding: ChatRunBinding = { ...requested, chatId };
  const boundChat = () => {
    const chat = findChat();
    return chat && sameRunBinding(storedRunBinding(chat), binding) ? chat : undefined;
  };
  if (!boundChat()) throw new StaleRunBindingError();
  const client = options?.client ?? await requireRuntimeClient({
    auth: "required", operation: `cancel active run ${binding.runId}`,
  });
  const chat = boundChat();
  if (!chat) throw new StaleRunBindingError();
  const previousRun = chat.activeRun!;
  const cancellingRun = makeActiveRunRecord({ ...previousRun, status: "cancelling" }, "local_mutation");
  chat.activeRun = cancellingRun;
  rememberLocallyStoppedRun(chat, binding.runId);
  const ownedChat = () => {
    const current = boundChat();
    return current?.activeRun === cancellingRun ? current : undefined;
  };
  const message = stoppedAssistantMessageController(binding.assistantMessageId);
  const completeMessage = () => {
    const current = ownedChat();
    if (!current) return;
    if (message.complete(current)) {
      current.streamProjection?.writer?.abort();
      if (current.projectId) touchProjectInStore(getStore(ownerUserId), current.projectId);
    }
  };
  const restore = () => {
    const current = ownedChat();
    if (!current) return;
    forgetLocallyStoppedRun(current, binding.runId);
    message.restore(current);
    current.activeRun = previousRun;
  };
  let cancelPending: boolean;
  try {
    cancelPending = await settleRuntimeCancel(client.sdk.cancelRun(binding.runId), options?.cancelTimeoutMs,
      settled => {
        if (settled.status === "completed") {
          completeMessage();
          void reconcileStoppedRun(binding.runId, ownedChat);
        } else restore();
      });
  } catch (error) {
    restore();
    throw error;
  }
  const current = ownedChat();
  if (cancelPending && current) cancellingRun.waitingFor = "cancel_requested";
  completeMessage();
  if (!cancelPending) await reconcileStoppedRun(binding.runId, ownedChat, client);
  const selected = findChat();
  return selected ? { activeRun: publicActiveRun(selected), cancelPending } : null;
}

export async function resumeActiveRun(ownerUserId: string, chatId: string, admittedClient?: WebRuntimeClient) {
  await syncBackendSessions(ownerUserId, admittedClient);
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  if (!chat.activeRun?.runId) {
    throw new Error("No paused run is available to resume.");
  }
  if (chat.activeRun.status.trim().toLowerCase() !== "paused") {
    throw new Error("Only paused runs can be resumed.");
  }

  const runId = chat.activeRun.runId;
  const sessionId = backendSessionIdForChat(chat);
  const assistantMessageId = chat.activeRun.assistantMessageId;
  const binding = storedRunBinding(chat)!;
  const currentChat = () => {
    const current = getStore(ownerUserId).chats.find(item => item.id === chatId);
    if (!current || !sameRunBinding(storedRunBinding(current), binding)) {
      throw new Error("The run changed while resume was in progress.");
    }
    return current;
  };
  const client = admittedClient ?? await requireRuntimeClient({
    auth: "required",
    operation: `resume active run ${runId}`,
  });
  const beforeResume = currentChat();
  const previousRun = beforeResume.activeRun;
  const projection = beforeResume.streamProjection;
  const lifecycle = projection?.state.runLifecycle;
  const lastStatus = projection?.state.lastStatus;
  const terminalStatus = projection?.state.terminalStatus;
  const writer = projection?.writer;
  const resumed = await client.sdk.resumeRun(runId);
  const current = currentChat();
  // Child observations can advance the shared cursor. Only newer root facts
  // supersede this acknowledgement; metadata polling cannot retire parser state.
  if (current.streamProjection !== projection ||
    current.activeRun !== previousRun && current.activeRun?.source !== "backend_poll" ||
    projection && (projection.state.runLifecycle !== lifecycle ||
      projection.state.lastStatus !== lastStatus || projection.state.terminalStatus !== terminalStatus)) {
    return { activeRun: publicActiveRun(current) };
  }
  if (isArtifactPublicationV1(resumed.artifact_publication)) {
    const message = current.messages.find(item => item.id === assistantMessageId && item.role === "assistant");
    if (message) message.artifactPublication = resumed.artifact_publication;
  }
  current.activeRun = makeActiveRunRecord(
    {
      runId,
      status: resumed.status,
      waitingFor: null,
      assistantMessageId: assistantMessageId ?? null,
      nextEventIndex: current.activeRun!.nextEventIndex ?? null,
    },
    "local_mutation",
  );
  if (resumed.status === "running" && projection && !projection.state.protocolError &&
    projection.assistantMessageId === assistantMessageId && projection.sessionId === sessionId &&
    projection.state.runId === runId && projection.state.runLifecycle === "paused") {
    writer?.abort();
    resumeStreamProjection(projection.state);
    updateStreamingAssistantMessage(ownerUserId, chatId, projection.assistantMessageId, {
      status: "streaming", reasoningStatus: projection.state.reasoningText ? "streaming" : undefined,
    });
  }

  return { activeRun: publicActiveRun(current) };
}

export function updateStreamingAssistantMessage(
  ownerUserId: string,
  chatId: string,
  messageId: string,
  patch: {
    content?: string;
    reasoning?: string;
    reasoningStatus?: ChatMessage["reasoningStatus"];
    status?: ChatMessage["status"];
    artifacts?: ChatMessage["artifacts"];
    artifactPublication?: ChatMessage["artifactPublication"];
    explainAnalyzeEvent?: NonNullable<ChatMessage["explainAnalyzeEvents"]>[number];
    explainAnalyzeDegraded?: boolean;
    explainAnalyzeUnrecoverable?: boolean;
    explainAnalyzeRepair?: { token: string; complete: boolean };
  },
) {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }

  const message = chat.messages.find((item) => item.id === messageId);
  if (!message) {
    return null;
  }

  if (patch.content !== undefined) {
    message.content = patch.content;
    chat.lastMessagePreview = patch.content || chat.lastMessagePreview;
  }
  if ("reasoning" in patch) {
    message.reasoning = patch.reasoning;
  }
  if ("reasoningStatus" in patch) {
    message.reasoningStatus = patch.reasoningStatus;
  }
  if (patch.status !== undefined) {
    message.status = patch.status;
  }
  if (patch.artifacts !== undefined) {
    message.artifacts = mergeChatArtifacts(
      message.artifacts ?? [],
      patch.artifacts,
    );
  }
  if (patch.artifactPublication !== undefined) message.artifactPublication = patch.artifactPublication;
  if (patch.explainAnalyzeEvent !== undefined) {
    Object.assign(message, appendExplainFact(message, patch.explainAnalyzeEvent));
  }
  if (patch.explainAnalyzeDegraded === true) {
    Object.assign(message, markExplainGap(message, patch.explainAnalyzeUnrecoverable));
  }
  if (patch.explainAnalyzeRepair) {
    const { token, complete } = patch.explainAnalyzeRepair;
    Object.assign(message, complete ? finishExplainRepair(message, token) : beginExplainRepair(message, token));
  }
  // Observation repair must not reorder chats/projects or resemble new output.
  if (patch.content !== undefined || patch.reasoning !== undefined ||
    patch.reasoningStatus !== undefined || patch.status !== undefined ||
    patch.artifacts !== undefined) {
    chat.lastMessageAt = nowIso();
    if (chat.projectId) touchProjectInStore(store, chat.projectId);
  }
  return message;
}

// Metadata commits replace only the shell. In-flight readers can then reject
// their old snapshot without another revision or cache; live Run/parser owners
// retain their existing references. Async writers rebind after acknowledgement.
function commitChatMetadata(
  ownerUserId: string,
  original: Pick<ChatRecord, "id" | "backendSessionId">,
  patch: Partial<Pick<ChatRecord, "model" | "workspaceSelection" | "title" | "projectId" | "archivedAt" | "lastMessageAt" | "backendSessionId">>,
): boolean {
  const store = getStore(ownerUserId);
  const index = store.chats.findIndex(chat => chat.id === original.id &&
    chat.backendSessionId === original.backendSessionId);
  if (index < 0) return false;
  store.chats[index] = { ...store.chats[index], ...patch };
  return true;
}

export async function updateChatModel(
  ownerUserId: string,
  chatId: string,
  model: string,
  client?: WebRuntimeClient,
) {
  const normalized = model.trim();
  if (!normalized) {
    return null;
  }
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  const binding = { id: chat.id, backendSessionId: chat.backendSessionId };
  await updateBackendSessionModel(chat, normalized, client);
  if (!commitChatMetadata(ownerUserId, binding, { model: normalized })) return null;
  return getChat(ownerUserId, chatId);
}

export async function updateChatWorkspaceSelection(
  ownerUserId: string,
  chatId: string,
  selection: WorkspaceSelection | null,
  client?: WebRuntimeClient,
) {
  const normalized: WorkspaceSelection | null =
    selection === null
      ? null
      : (normalizeWorkspaceSelection(selection) ?? null);
  if (selection !== null && normalized === null) return null;
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }

  if (sameWorkspaceSelection(chat.workspaceSelection, normalized)) {
    return getChat(ownerUserId, chatId);
  }
  const binding = { id: chat.id, backendSessionId: chat.backendSessionId };
  if (chat.backendSessionId) {
    await updateBackendSessionWorkspaceSelection(chat, normalized, client);
  }
  if (!commitChatMetadata(ownerUserId, binding, { workspaceSelection: normalized ?? undefined })) return null;
  return getChat(ownerUserId, chatId);
}

export function moveChat(
  ownerUserId: string,
  chatId: string,
  projectId: string | null,
) {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  if (
    projectId &&
    !store.projects.some((project) => project.id === projectId)
  ) {
    return null;
  }
  commitChatMetadata(ownerUserId, chat, { projectId, lastMessageAt: nowIso() });
  return getChat(ownerUserId, chatId);
}

export async function archiveChat(
  ownerUserId: string,
  chatId: string,
  archived: boolean,
  client?: WebRuntimeClient,
) {
  const chat = getStore(ownerUserId).chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  const binding = { id: chat.id, backendSessionId: chat.backendSessionId };
  await updateBackendSessionArchive(chat, archived, client);
  if (!commitChatMetadata(ownerUserId, binding, { archivedAt: archived ? nowIso() : null })) return null;
  return getChat(ownerUserId, chatId);
}

export async function deleteChat(
  ownerUserId: string,
  chatId: string,
  client?: WebRuntimeClient,
): Promise<boolean> {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return false;
  }
  await deleteBackendSession(chat, client);
  discardChatStreamProjection(chat);
  store.chats = store.chats.filter((item) => item.id !== chatId);
  if (chat.projectId) {
    touchProjectInStore(store, chat.projectId);
  }
  return true;
}

export async function deleteArchivedChats(
  ownerUserId: string,
  client?: WebRuntimeClient,
): Promise<number> {
  await syncBackendSessions(ownerUserId, client);
  const store = getStore(ownerUserId);
  const archivedChats = store.chats.filter((chat) => chat.archivedAt);
  for (const chat of archivedChats) {
    await deleteBackendSession(chat, client);
  }
  for (const chat of archivedChats) discardChatStreamProjection(chat);
  const archivedIds = new Set(archivedChats.map((chat) => chat.id));
  store.chats = store.chats.filter((chat) => !archivedIds.has(chat.id));
  const touchedProjectIds = new Set(
    archivedChats
      .map((chat) => chat.projectId)
      .filter((projectId): projectId is string => Boolean(projectId)),
  );
  for (const projectId of touchedProjectIds) {
    touchProjectInStore(store, projectId);
  }
  return archivedChats.length;
}

export async function getSidebar(
  ownerUserId: string,
  client?: WebRuntimeClient,
): Promise<SidebarData> {
  await syncBackendSessions(ownerUserId, client);
  const store = getStore(ownerUserId);
  const recentChats: Array<{
    kind: "chat";
    id: string;
    title: string;
    href: string;
    updatedAt: string;
  }> = store.chats
    .filter((chat) => !chat.archivedAt)
    .map((chat) => ({
      kind: "chat",
      id: chat.id,
      title: chat.title ?? chat.lastMessagePreview?.slice(0, 48) ?? "Untitled",
      href: chat.projectId
        ? `/projects/${chat.projectId}/chats/${chat.id}`
        : `/chats/${chat.id}`,
      updatedAt: chat.lastMessageAt,
    }));
  const projectChats = store.chats
    .filter((chat) => !chat.archivedAt && chat.projectId)
    .sort((a, b) => b.lastMessageAt.localeCompare(a.lastMessageAt));
  const recentProjectGroups = store.projects
    .map((project) => {
      const chats = projectChats
        .filter((chat) => chat.projectId === project.id)
        .slice(0, 8)
        .map((chat) => ({
          kind: "chat" as const,
          id: chat.id,
          title:
            chat.title ?? chat.lastMessagePreview?.slice(0, 48) ?? "Untitled",
          href: `/projects/${project.id}/chats/${chat.id}`,
          updatedAt: chat.lastMessageAt,
        }));
      if (!chats.length) {
        return null;
      }
      const updatedAt = chats[0]?.updatedAt ?? project.updatedAt;
      return {
        project: {
          kind: "project" as const,
          id: project.id,
          title: project.name,
          href: `/projects/${project.id}`,
          updatedAt,
        },
        chats,
        updatedAt,
      };
    })
    .filter((group): group is NonNullable<typeof group> => Boolean(group))
    .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt))
    .slice(0, 10);
  const recentOtherChats = store.chats
    .filter((chat) => !chat.archivedAt && !chat.projectId)
    .sort((a, b) => b.lastMessageAt.localeCompare(a.lastMessageAt))
    .slice(0, 20)
    .map((chat) => ({
      kind: "chat" as const,
      id: chat.id,
      title: chat.title ?? chat.lastMessagePreview?.slice(0, 48) ?? "Untitled",
      href: `/chats/${chat.id}`,
      updatedAt: chat.lastMessageAt,
    }));
  const archivedChats: Array<{
    kind: "chat";
    id: string;
    title: string;
    href: string;
    updatedAt: string;
  }> = store.chats
    .filter((chat) => chat.archivedAt)
    .map((chat) => ({
      kind: "chat",
      id: chat.id,
      title: chat.title ?? chat.lastMessagePreview?.slice(0, 48) ?? "Untitled",
      href: chat.projectId
        ? `/projects/${chat.projectId}/chats/${chat.id}`
        : `/chats/${chat.id}`,
      updatedAt: chat.archivedAt ?? chat.lastMessageAt,
    }));
  const recentProjects: Array<{
    kind: "project";
    id: string;
    title: string;
    href: string;
    updatedAt: string;
  }> = store.projects.map((project) => ({
    kind: "project",
    id: project.id,
    title: project.name,
    href: `/projects/${project.id}`,
    updatedAt: project.updatedAt,
  }));
  const recents = [...recentChats, ...recentProjects]
    .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt))
    .slice(0, 20);
  const untitled = recentChats.filter((chat) => chat.title === "Untitled");
  return {
    recents,
    recentProjectGroups,
    recentOtherChats,
    untitled,
    archivedChats: archivedChats
      .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt))
      .slice(0, 50),
    user: getCurrentUser(),
  };
}

export async function ensureChatBackendSession(
  ownerUserId: string,
  chatId: string,
  params: {
    model?: string | null;
    runtime?: WebRuntimeClient;
  } = {},
): Promise<string> {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    throw new Error(
      `Cannot create persisted session: chat ${chatId} was not found.`,
    );
  }
  if (chat.backendSessionId) {
    return chat.backendSessionId;
  }

  const binding = { id: chat.id, backendSessionId: chat.backendSessionId };
  const model = requireSelectedOfferingId(params.model ?? chat.model);
  const session = await createBackendSession({
    chatId: chat.id,
    title: chat.title,
    projectId: chat.projectId,
    model,
    workspaceSelection: chat.workspaceSelection,
    runtime: params.runtime,
  });
  if (!commitChatMetadata(ownerUserId, binding, { backendSessionId: session.sessionId })) {
    throw new Error("The chat was deleted or rebound while its Session was being created.");
  }
  return session.sessionId;
}

export function searchData(ownerUserId: string, query: string): SearchResponse {
  const q = query.trim().toLowerCase();
  const store = getStore(ownerUserId);
  const projects = store.projects
    .filter(
      (project) =>
        !q ||
        `${project.name} ${project.description ?? ""}`
          .toLowerCase()
          .includes(q),
    )
    .slice(0, 8)
    .map((project) => ({
      id: project.id,
      name: project.name,
      updatedAt: project.updatedAt,
    }));
  const chats = store.chats
    .filter((chat) => !chat.archivedAt)
    .filter(
      (chat) =>
        !q ||
        `${chat.title ?? ""} ${chat.lastMessagePreview ?? ""}`
          .toLowerCase()
          .includes(q),
    )
    .slice(0, 12)
    .map((chat) => ({
      id: chat.id,
      title: chat.title,
      projectId: chat.projectId,
      updatedAt: chat.lastMessageAt,
    }));
  return { projects, chats };
}

function chatSummary(chat: ChatRecord): ChatSummary {
  return {
    id: chat.id,
    title: chat.title,
    lastMessageAt: chat.lastMessageAt,
    lastMessagePreview: chat.lastMessagePreview,
    projectId: chat.projectId,
    archivedAt: chat.archivedAt ?? null,
    model: chat.model ?? null,
  };
}

function metadataString(
  metadata: Record<string, unknown> | undefined,
  key: string,
): string | null {
  const value = metadata?.[key];
  return typeof value === "string" && value.trim() ? value : null;
}

function isWebSession(session: RuntimeSessionResponse): boolean {
  return metadataString(session.metadata, "source") === "web_v1";
}

function isUnpersistedLocalChat(chat: ChatRecord): boolean {
  return (
    !chat.backendSessionId &&
    (Boolean(chat.pendingTurn) ||
      Boolean(chat.activeRun) ||
      chat.messages.some((message) => message.status === "streaming"))
  );
}

function chatRecordFromBackendSession(
  session: RuntimeSessionResponse,
  existing?: ChatRecord,
): ChatRecord | null {
  if (!session.session_id || !isWebSession(session)) {
    return null;
  }

  const createdAt = session.created_at ?? existing?.createdAt ?? nowIso();
  const updatedAt = session.updated_at ?? existing?.lastMessageAt ?? createdAt;
  const projectId = metadataString(session.metadata, "project_id");
  const model =
    metadataString(session.metadata, "current_model") ??
    metadataString(session.metadata, "initial_model") ??
    existing?.model ??
    null;
  const title = session.title ?? existing?.title ?? null;
  const archivedAt = session.status === "archived" ? updatedAt : null;
  const workspaceSelection = hasWorkspaceSelectionMetadata(session)
    ? normalizeWorkspaceSelection(
        session.metadata?.[WORKSPACE_SELECTION_METADATA_KEY],
      )
    : existing?.workspaceSelection;

  if (existing?.streamProjection?.sessionId && existing.streamProjection.sessionId !== session.session_id) {
    discardChatStreamProjection(existing);
  }
  return {
    id:
      existing?.id ??
      metadataString(session.metadata, "web_chat_id") ??
      session.session_id,
    title,
    projectId,
    createdAt,
    lastMessageAt: updatedAt,
    lastMessagePreview: existing?.lastMessagePreview ?? title ?? undefined,
    archivedAt,
    model,
    backendSessionId: session.session_id,
    messages: existing?.messages ?? [],
    streamProjection: existing?.streamProjection,
    workspaceSelection,
    activeRun: existing?.activeRun,
    locallyStoppedRuns: existing?.locallyStoppedRuns,
    pendingTurn: existing?.pendingTurn,
  };
}

async function listAllBackendRuns(
  client: WebRuntimeClient,
  ownerUserId: string,
): Promise<RunStatus[]> {
  const runs: RunStatus[] = [];
  let cursor: NonNullable<RunListResponse["nextCursor"]> | undefined;

  for (;;) {
    let parsed: RunListResponse;
    try {
      parsed = await client.sdk.listRuns({
        limit: RUN_SYNC_PAGE_SIZE,
        ...(cursor ? { cursor } : {}),
      });
    } catch (error) {
      throw runtimeOperationError(
        `Cannot sync active runs for user ${ownerUserId}`,
        error,
      );
    }

    const page = Array.isArray(parsed.runs) ? parsed.runs : [];
    runs.push(...page);

    const responseLimit =
      typeof parsed.limit === "number" && parsed.limit > 0
        ? parsed.limit
        : RUN_SYNC_PAGE_SIZE;
    cursor = parsed.nextCursor ?? undefined;

    if (page.length === 0 || page.length < responseLimit || !cursor) {
      break;
    }
  }

  return runs;
}

async function listAllBackendSessions(
  client: WebRuntimeClient,
  ownerUserId: string,
): Promise<RuntimeSessionResponse[]> {
  const sessions: RuntimeSessionResponse[] = [];
  let cursor: RuntimeSessionListCursor | undefined;

  for (;;) {
    let parsed: RuntimeSessionListResponse;
    try {
      parsed = await client.sdk.listRuntimeSessions({
        limit: SESSION_SYNC_PAGE_SIZE,
        cursor,
      });
    } catch (error) {
      throw runtimeOperationError(
        `Cannot sync persisted sessions for user ${ownerUserId}`,
        error,
      );
    }

    const page = Array.isArray(parsed.sessions) ? parsed.sessions : [];
    sessions.push(...page);

    cursor = parsed.next_cursor ?? undefined;
    if (page.length === 0 || !cursor) {
      break;
    }
  }

  return sessions;
}

async function syncBackendSessions(
  ownerUserId: string,
  admittedClient?: WebRuntimeClient,
): Promise<void> {
  const beforeRead = new Map(getStore(ownerUserId).chats.map(chat => [chat.id, chat]));
  const beforeBySession = new Map([...beforeRead.values()].filter(chat => chat.backendSessionId)
    .map(chat => [chat.backendSessionId!, chat]));
  const client = admittedClient ?? await getRuntimeClient({
    auth: "required",
    operation: `sync persisted sessions for user ${ownerUserId}`,
  });
  if (!client) {
    return;
  }

  const sessions = await listAllBackendSessions(client, ownerUserId);
  const runs = await listAllBackendRuns(client, ownerUserId);
  const syncStartedAt = nowIso();
  const store = getStore(ownerUserId);
  const byId = new Map(store.chats.map((chat) => [chat.id, chat]));
  const byBackendSessionId = new Map(
    store.chats
      .filter((chat) => chat.backendSessionId)
      .map((chat) => [chat.backendSessionId as string, chat]),
  );
  const backendChatIds = new Set<string>();
  const activeRunBySession = new Map<string, ChatActiveRunRecord>();
  const backendRunStatuses = new Map<string, string>();
  const syncNow = Date.now();

  for (const run of runs) {
    backendRunStatuses.set(run.runId, run.status);
    const existingForRun =
      byBackendSessionId.get(run.sessionId) ?? byId.get(run.sessionId);
    if (existingForRun && isTerminalChatRunStatus(run.status)) {
      forgetLocallyStoppedRun(existingForRun, run.runId);
    }
    if (isLocallyStoppedRun(existingForRun, run.runId, syncNow)) {
      continue;
    }
    // One chat owns one root run at a time. Child runs share its session but
    // are managed through the agent workbench; selecting one here makes Stop,
    // input routing, and refresh target the wrong execution.
    if (run.parentRunId !== null || run.depth !== 0) {
      continue;
    }
    if (!runBlocksChatTurn(run.status)) {
      continue;
    }
    const candidate = makeActiveRunRecord(
      {
        runId: run.runId,
        status: run.status,
        waitingFor: run.waitingFor ?? null,
      },
      "backend_poll",
      syncStartedAt,
    );
    const current = activeRunBySession.get(run.sessionId);
    if (!current || activeRunPriority(candidate) > activeRunPriority(current)) {
      activeRunBySession.set(run.sessionId, candidate);
    }
  }

  for (const session of sessions) {
    const existing = session.session_id
      ? (byBackendSessionId.get(session.session_id) ??
        byId.get(session.session_id))
      : undefined;
    const original = beforeBySession.get(session.session_id) ?? beforeRead.get(existing?.id ?? session.session_id);
    if (existing !== original) {
      if (existing) backendChatIds.add(existing.id);
      continue;
    }
    const record = chatRecordFromBackendSession(session, existing);
    if (!record) {
      continue;
    }
    const backendRun = session.session_id ? activeRunBySession.get(session.session_id) : undefined;
    // Polling may observe terminal before the live parser receives it. Keep
    // that exact continuation until its real terminal, unless a successor won.
    const preserveParser = existing && hasContinuingStream(existing) &&
      !backendRun && isTerminalChatRunStatus(backendRunStatuses.get(existing.activeRun!.runId));
    record.activeRun = preserveParser ? existing.activeRun : mergeActiveRunRecord({
      existing: existing?.activeRun,
      backend: backendRun,
      backendRunStatuses,
    });
    if (record.streamProjection?.state.runId && record.activeRun &&
      record.streamProjection.state.runId !== record.activeRun.runId) {
      discardChatStreamProjection(record);
    }
    backendChatIds.add(record.id);
    const index = store.chats.findIndex((chat) => chat.id === record.id);
    if (index >= 0) {
      store.chats[index] = record;
    } else {
      store.chats.push(record);
    }
  }

  // The runtime session table is the source of truth. Once a full paginated
  // sync succeeds, remove local web-chat shells whose persisted runtime session
  // disappeared (for example after a developer resets the MatrixOne database).
  // Keeping those shells lets the UI send messages to non-existent sessions and
  // turns a clean 404 into a fake assistant error message.
  store.chats = store.chats.filter((chat) => {
    const retained = backendChatIds.has(chat.id) || isUnpersistedLocalChat(chat) ||
      beforeRead.get(chat.id) !== chat;
    if (!retained) discardChatStreamProjection(chat);
    return retained;
  });
  normalizeCanonicalChatIds(store);
  store.chats.sort((a, b) => b.lastMessageAt.localeCompare(a.lastMessageAt));
}

function transcriptItemToMessage(
  chatId: string,
  item: RuntimeTranscriptItemResponse,
): ChatMessage | null {
  if (
    typeof item.item_seq !== "number" ||
    typeof item.role !== "string" ||
    typeof item.content !== "string"
  ) {
    return null;
  }
  if (
    item.role !== "user" &&
    item.role !== "assistant" &&
    item.role !== "system"
  ) {
    return null;
  }
  const reasoning =
    typeof item.reasoning === "string" ? item.reasoning.trim() : "";
  const artifacts = mergeChatArtifacts(
    [],
    artifactsFromValues(item.artifacts),
  );
  // Canonical transcripts retain assistant tool-call scaffolding even when it
  // has no user-visible text. It is valid replay state, but rendering it as a
  // blank chat message makes a cancelled/refreshed turn look like data loss.
  if (
    item.role === "assistant" &&
    !item.content.trim() &&
    !reasoning &&
    artifacts.length === 0
  ) {
    return null;
  }
  const reasoningStatus =
    item.reasoning_status === "streaming" ||
    item.reasoning_status === "complete"
      ? item.reasoning_status
      : reasoning
        ? "complete"
        : undefined;

  return {
    id: `${chatId}:${item.item_seq}`,
    role: item.role,
    content: item.content,
    reasoning: reasoning || undefined,
    reasoningStatus,
    artifacts: artifacts.length > 0 ? artifacts : undefined,
    createdAt: item.created_at ?? nowIso(),
    status: "complete",
  };
}

async function syncBackendTranscript(ownerUserId: string, chatId: string, admittedClient?: WebRuntimeClient): Promise<string | undefined> {
  const currentChat = () => getStore(ownerUserId).chats.find(item =>
    item.id === chatId || item.backendSessionId === chatId);
  const original = currentChat();
  const sessionId = original?.backendSessionId;
  const runId = original?.activeRun?.runId;
  const assistantId = original?.activeRun?.assistantMessageId;
  const projection = original?.streamProjection;
  const messages = original?.messages;
  const pendingTurn = original?.pendingTurn;
  // A detail read may overlap POST admission, a terminal, or another reader.
  // Compare the entire existing continuation before publishing success/errors.
  const stillCurrent = () => {
    const current = currentChat();
    return current === original && current?.backendSessionId === sessionId && current?.activeRun?.runId === runId &&
      current?.activeRun?.assistantMessageId === assistantId && current?.streamProjection === projection &&
      current?.messages === messages && current?.pendingTurn === pendingTurn;
  };
  const client = admittedClient ?? await getRuntimeClient({ auth: "required", operation: "read selected conversation" });
  if (!client || !stillCurrent()) return currentChat()?.id;

  let backendSessionId = sessionId;
  if (!backendSessionId) {
    // Resolve legacy Web IDs through the existing directory; known Sessions
    // read directly, without enumerating the owner's Runs or other Sessions.
    const sessions = await listAllBackendSessions(client, ownerUserId);
    if (!stillCurrent()) return currentChat()?.id;
    backendSessionId = sessions.find(session => session.session_id === chatId ||
      metadataString(session.metadata, "web_chat_id") === chatId)?.session_id;
    if (!backendSessionId) return;
  }
  let parsed;
  try {
    const reusable = runId && publicActiveRun(original)?.nextEventIndex !== undefined;
    parsed = await client.sdk.getSessionConversation(backendSessionId, CHAT_HISTORY_LIMIT,
      reusable ? { runId, preserveTerminal: !isLocallyStoppedRun(original, runId, Date.now()) &&
        !!original && hasContinuingStream(original) } : undefined);
  } catch (error) {
    if (!stillCurrent()) return currentChat()?.id;
    if ((error instanceof AstraApiError || error instanceof RuntimeClientError) && error.status === 404) {
      const missing = currentChat();
      if (missing?.backendSessionId === backendSessionId) {
        discardChatStreamProjection(missing);
        const store = getStore(ownerUserId);
        store.chats = store.chats.filter(item => item !== missing);
      }
      return;
    }
    throw runtimeOperationError(`Cannot sync persisted transcript for session ${backendSessionId}`, error);
  }
  if (!stillCurrent()) return currentChat()?.id;
  if (parsed.session.session_id !== backendSessionId) {
    throw new Error("Conversation response does not match the selected Session.");
  }
  const existing = currentChat();
  // A newly started POST has a parser before its Run identity arrives.
  const livePost = existing?.streamProjection;
  if (livePost?.writer && !livePost.writer.signal.aborted &&
    !livePost.state.runId && !livePost.state.protocolError) return existing!.id;
  const root = parsed.selected_root;
  const stopped = root && isLocallyStoppedRun(existing, root.run_id, Date.now());
  const continuing = existing && hasContinuingStream(existing);
  const warm = existing && root && root.run_id === runId && publicActiveRun(existing)?.nextEventIndex !== undefined &&
    (!isTerminalChatRunStatus(root.status) || !stopped && continuing);
  // An omitted history is only valid for the exact reusable continuation.
  // Never turn it into an empty cold transcript after a local Stop/expiry.
  if (!parsed.history_loaded && !warm) return existing?.id;
  if (existing && !warm) discardChatStreamProjection(existing);
  const record = chatRecordFromBackendSession(parsed.session, existing);
  if (!record) return;
  if (root && isTerminalChatRunStatus(root.status)) forgetLocallyStoppedRun(record, root.run_id);
  const active = root && runBlocksChatTurn(root.status) && !stopped
    ? makeActiveRunRecord({ runId: root.run_id, status: root.status, waitingFor: root.waiting_for }, "backend_poll")
    : undefined;
  const reconciled = mergeActiveRunRecord({
    existing: root?.run_id === existing?.activeRun?.runId ? existing?.activeRun : undefined,
    backend: active, backendRunStatuses: new Map(root ? [[root.run_id, root.status]] : []),
  });
  if (warm) {
    // Stream terminal owns an unstopped continuation; cancellation keeps its
    // existing expiry rules even while the parser remains reusable.
    record.activeRun = isTerminalChatRunStatus(root!.status) && !stopped && continuing
      ? existing.activeRun : reconciled;
    if (!record.activeRun) discardChatStreamProjection(record);
  } else {
    record.activeRun = reconciled;
    record.messages = parsed.items.flatMap(item => {
      if (item.kind === "opening") return [{ id: item.id, role: "user" as const,
        content: item.content, createdAt: item.created_at, status: "complete" as const }];
      if (item.kind === "failure") return [{ id: item.id, role: "assistant" as const,
        content: item.failure.message, createdAt: item.created_at, status: "failed" as const }];
      const message = transcriptItemToMessage(record.id, item);
      if (message && item.failure) {
        message.status = "failed";
        message.content = appendRunFailure(message.content, item.failure.message);
        message.reasoningStatus = undefined;
      }
      return message ? [message] : [];
    });
    if (record.activeRun) {
      const active = record.activeRun;
      const assistant = streamingAssistantMessage(record, `inflight:${active.runId}`, nowIso(), {
        sessionId: backendSessionId, runId: active.runId, nextEventIndex: 0,
      });
      record.messages.push(assistant);
      active.assistantMessageId = assistant.id;
    }
    const latest = record.messages.at(-1);
    record.lastMessageAt = latest?.createdAt ?? record.lastMessageAt;
    record.lastMessagePreview = latest?.content || record.lastMessagePreview;
  }
  const store = getStore(ownerUserId);
  const index = store.chats.findIndex(item => item.id === chatId || item.backendSessionId === backendSessionId);
  if (index < 0) store.chats.push(record);
  else store.chats[index] = record;
  return record.id;
}

async function createBackendSession(params: {
  chatId: string;
  title: string | null;
  projectId: string | null;
  model: string;
  workspaceSelection?: WorkspaceSelection;
  runtime?: WebRuntimeClient;
}): Promise<{ sessionId: string }> {
  const client =
    params.runtime ??
    (await requireRuntimeClient({
      auth: "required",
      operation: "create persisted session",
    }));
  let parsed: RuntimeSessionResponse;
  try {
    parsed = await client.sdk.createRuntimeSession({
      agent_id: null,
      title: params.title,
      metadata: {
        source: "web_v1",
        web_chat_id: params.chatId,
        project_id: params.projectId,
        initial_model: params.model,
        current_model: params.model,
        ...(params.workspaceSelection
          ? {
              [WORKSPACE_SELECTION_METADATA_KEY]: workspaceSelectionMetadata(
                params.workspaceSelection,
              ),
            }
          : {}),
      },
    });
  } catch (error) {
    throw runtimeOperationError("Cannot create persisted session", error);
  }
  if (!parsed.session_id) {
    throw new Error(
      "Cannot create persisted session: runtime response did not include session_id.",
    );
  }

  return { sessionId: parsed.session_id };
}

async function deleteBackendSession(chat: ChatRecord, admittedClient?: WebRuntimeClient): Promise<void> {
  const sessionId = backendSessionIdForChat(chat);
  const client = admittedClient ?? await requireRuntimeClient({
    auth: "required",
    operation: `delete persisted session ${sessionId}`,
  });
  try {
    await client.sdk.deleteSession(sessionId);
  } catch (error) {
    throw runtimeOperationError(
      `Cannot delete persisted session ${sessionId}`,
      error,
    );
  }
}

async function updateBackendSessionArchive(
  chat: ChatRecord,
  archived: boolean,
  admittedClient?: WebRuntimeClient,
): Promise<void> {
  const sessionId = backendSessionIdForChat(chat);
  const client = admittedClient ?? await requireRuntimeClient({
    auth: "required",
    operation: `update persisted session ${sessionId} archive state`,
  });
  try {
    await client.sdk.updateRuntimeSession(sessionId, {
      status: archived ? "archived" : "active",
    });
  } catch (error) {
    throw runtimeOperationError(
      `Cannot update persisted session ${sessionId}`,
      error,
    );
  }
}

async function updateBackendSessionModel(
  chat: ChatRecord,
  model: string,
  admittedClient?: WebRuntimeClient,
): Promise<void> {
  const sessionId = backendSessionIdForChat(chat);
  const client = admittedClient ?? await requireRuntimeClient({
    auth: "required",
    operation: `update persisted session ${sessionId} model`,
  });
  try {
    await client.sdk.updateRuntimeSession(sessionId, {
      metadata_patch: { current_model: model },
    });
  } catch (error) {
    throw runtimeOperationError(
      `Cannot update persisted session ${sessionId} model`,
      error,
    );
  }
}

async function updateBackendSessionWorkspaceSelection(
  chat: ChatRecord,
  selection: WorkspaceSelection | null,
  admittedClient?: WebRuntimeClient,
): Promise<void> {
  const sessionId = backendSessionIdForChat(chat);
  const client = admittedClient ?? await requireRuntimeClient({
    auth: "required",
    operation: `update persisted session ${sessionId} workspace selection`,
  });
  try {
    await client.sdk.updateRuntimeSession(sessionId, {
      metadata_patch: {
        [WORKSPACE_SELECTION_METADATA_KEY]: selection
          ? workspaceSelectionMetadata(selection)
          : null,
      },
    });
  } catch (error) {
    throw runtimeOperationError(
      `Cannot update persisted session ${sessionId} workspace selection`,
      error,
    );
  }
}

function normalizeCanonicalChatIds(store: Store) {
  const seen = new Set<string>();
  const seenBackendSessionIds = new Set<string>();
  const normalized: ChatRecord[] = [];
  for (const chat of store.chats) {
    if (LEGACY_LOCAL_CHAT_IDS.has(chat.id)) {
      discardChatStreamProjection(chat);
      continue;
    }
    const backendSessionId =
      chat.backendSessionId ??
      (chat.id.startsWith("web-") ? undefined : chat.id);
    if (backendSessionId && seenBackendSessionIds.has(backendSessionId)) {
      discardChatStreamProjection(chat);
      continue;
    }
    if (seen.has(chat.id)) {
      discardChatStreamProjection(chat);
      continue;
    }
    seen.add(chat.id);
    if (backendSessionId) {
      seenBackendSessionIds.add(backendSessionId);
    }
    normalized.push(chat);
  }
  store.chats = normalized;
}

function projectSummary(project: ProjectRecord): ProjectSummary {
  return {
    id: project.id,
    name: project.name,
    description: project.description,
    updatedAt: project.updatedAt,
    starred: project.starred,
    visibility: project.visibility,
  };
}

function touchProjectInStore(store: Store, projectId: string) {
  const project = store.projects.find((item) => item.id === projectId);
  if (project) {
    project.updatedAt = nowIso();
  }
}

export async function resolveModelOfferingSelection(
  runtime: RuntimeConfig | WebRuntimeClient,
  offeringId: string,
): Promise<{ offeringId: string }> {
  if (!offeringId || offeringId.trim() !== offeringId) {
    throw new ModelOfferingSelectionError(
      "invalid_selection",
      "offeringId must be an exact non-empty identifier",
    );
  }

  const client =
    runtime instanceof WebRuntimeClient
      ? runtime
      : new WebRuntimeClient(runtime);
  const { accessToken } = client.sdk.getTokens();
  if (!accessToken) {
    throw new ModelOfferingSelectionError(
      "authentication_required",
      "authenticated model access is required",
    );
  }

  // The server is the authority for Offering existence, activity, scope, and
  // route revalidation at run admission. The SDK drains the complete
  // paginated catalog for browsing, but this helper still never invents an
  // admission allow-list: the exact Offering is revalidated at run admission.
  return { offeringId };
}
