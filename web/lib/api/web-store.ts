import type { RuntimeConfig } from "@/lib/runtime-config";
import {
  artifactsFromValues,
  mergeChatArtifacts,
} from "@/lib/api/stream-artifacts";
import {
  buildQueryString,
  chatRunStreamPath,
  parseSseDataEvents,
  type RunListResponse,
  type RunStatus,
  type RuntimeSessionListCursor,
  type RuntimeSessionListResponse,
  type RuntimeSessionResponse,
  type RuntimeTranscriptItemResponse,
  type RuntimeTranscriptResponse,
} from "@astra/sdk";
import {
  activeRunPriority,
  isTerminalChatRunStatus,
  runBlocksChatTurn,
} from "@/lib/chat-run-state";
import {
  RuntimeClientError,
  readRuntimeErrorDetail,
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
  maxNextEventIndex,
  mergeRunStreamBinding,
  normalizeNextEventIndex,
  type ChatActiveRunRecord,
} from "@/lib/api/active-run-merge";
import { modelCache } from "@/lib/api/model-cache";
import { settleRuntimeCancel } from "@/lib/api/runtime-cancel-settlement";

type ChatRecord = ChatSummary & {
  createdAt: string;
  archivedAt?: string | null;
  backendSessionId?: string | null;
  messages: ChatMessage[];
  workspaceSelection?: WorkspaceSelection;
  activeRun?: ChatActiveRunRecord;
  locallyStoppedRuns?: Record<string, string>;
  pendingTurn?: {
    messageId: string;
    content: string;
    options: ComposerOptions;
  };
};

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

const AGENT_RESPONSE_TIMEOUT_MS = 30_000;
const AGENT_STREAM_TIMEOUT_MS = 180_000;
const LOCAL_ACTIVE_RUN_GRACE_MS = 30_000;
const LOCAL_CANCELLING_STATUS_GRACE_MS = 15_000;
const LOCAL_STOPPED_RUN_GRACE_MS = 30 * 60_000;
const SESSION_SYNC_PAGE_SIZE = 200;
const RUN_SYNC_PAGE_SIZE = 200;
const MAX_DEFERRED_INPUT_CHARS = 20_000;
const LEGACY_LOCAL_CHAT_IDS = new Set(["chat-web-agent-notes"]);
const WORKSPACE_SELECTION_METADATA_KEY = "workspace_selection";

type StreamResult = {
  assistantText: string;
  error?: string;
  finished?: boolean;
  nextOffset?: number;
};

export type ModelOfferingSelectionErrorCode =
  | "invalid_selection"
  | "authentication_required"
  | "catalog_unavailable"
  | "offering_unavailable";

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

function findStoppableAssistantMessage(
  chat: ChatRecord,
  assistantMessageId?: string | null,
) {
  if (assistantMessageId) {
    const matched = chat.messages.find(
      (message) =>
        message.id === assistantMessageId &&
        isStoppableAssistantMessage(message),
    );
    if (matched) {
      return matched;
    }
  }

  for (let index = chat.messages.length - 1; index >= 0; index -= 1) {
    const message = chat.messages[index];
    if (message && isStoppableAssistantMessage(message)) {
      return message;
    }
  }
  return null;
}

function cloneChatMessage(message: ChatMessage): ChatMessage {
  return {
    ...message,
    attachments: message.attachments
      ? message.attachments.map((attachment) => ({ ...attachment }))
      : undefined,
    artifacts: message.artifacts
      ? message.artifacts.map((artifact) => ({ ...artifact }))
      : undefined,
  };
}

function stoppedAssistantMessageController(
  chat: ChatRecord,
  assistantMessageId?: string | null,
): {
  complete: (currentChat: ChatRecord) => boolean;
  restore: (currentChat: ChatRecord) => void;
} {
  const message = findStoppableAssistantMessage(chat, assistantMessageId);
  const snapshot = message
    ? {
        message: cloneChatMessage(message),
        lastMessageAt: chat.lastMessageAt,
        lastMessagePreview: chat.lastMessagePreview,
      }
    : null;
  return {
    complete(currentChat) {
      const currentMessage = findStoppableAssistantMessage(
        currentChat,
        assistantMessageId,
      );
      if (!currentMessage) {
        return false;
      }

      currentMessage.content = currentMessage.content.trim()
        ? `${currentMessage.content}${currentMessage.content.endsWith("\n") ? "" : "\n"}\nStopped.`
        : "Stopped.";
      currentMessage.status = "complete";
      currentMessage.completedAt = nowIso();
      currentMessage.reasoningStatus = "complete";
      currentChat.lastMessageAt = nowIso();
      currentChat.lastMessagePreview = currentMessage.content;
      return true;
    },
    restore(currentChat) {
      if (!snapshot) {
        return;
      }
      const messageIndex = currentChat.messages.findIndex(
        (message) => message.id === snapshot.message.id,
      );
      if (messageIndex === -1) {
        return;
      }
      currentChat.messages[messageIndex] = cloneChatMessage(snapshot.message);
      currentChat.lastMessageAt = snapshot.lastMessageAt;
      currentChat.lastMessagePreview = snapshot.lastMessagePreview;
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

function normalizedActiveSkills(skills?: string[]) {
  if (!Array.isArray(skills)) {
    return [];
  }
  return [...new Set(skills.map((skill) => skill.trim()).filter(Boolean))].sort(
    (left, right) => left.localeCompare(right),
  );
}

function normalizedActiveTools(tools?: string[], webSearch = false) {
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
  ownerUserId: string,
  chatId: string,
  runId: string,
): Promise<ChatDetail["activeRun"]> {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return undefined;
  }
  const client = await getRuntimeClient({
    auth: "required",
    operation: `reconcile stopped run ${runId}`,
  }).catch((error) => {
    console.warn(
      "Failed to initialize runtime client for stop reconciliation:",
      runtimeErrorDetail(error),
    );
    return null;
  });
  if (!client) {
    return publicActiveRun(chat.activeRun);
  }

  let runStatus: RunStatus;
  try {
    runStatus = await client.sdk.getRunStatus(runId);
  } catch (error) {
    if (
      error instanceof RuntimeClientError &&
      error.status &&
      [404, 409, 410].includes(error.status)
    ) {
      forgetLocallyStoppedRun(chat, runId);
      if (chat.activeRun?.runId === runId) {
        chat.activeRun = undefined;
      }
      return publicActiveRun(chat.activeRun);
    }
    console.warn("Failed to reconcile stopped run:", runtimeErrorDetail(error));
    return publicActiveRun(chat.activeRun);
  }

  if (runStatus.sessionId !== backendSessionIdForChat(chat)) {
    forgetLocallyStoppedRun(chat, runId);
    if (chat.activeRun?.runId === runId) {
      chat.activeRun = undefined;
    }
    return publicActiveRun(chat.activeRun);
  }

  if (isTerminalChatRunStatus(runStatus.status)) {
    forgetLocallyStoppedRun(chat, runId);
    if (chat.activeRun?.runId === runId) {
      chat.activeRun = undefined;
    }
    return publicActiveRun(chat.activeRun);
  }

  if (runBlocksChatTurn(runStatus.status) && chat.activeRun?.runId === runId) {
    chat.activeRun = makeActiveRunRecord(
      isLocallyStoppedRun(chat, runId)
        ? {
            runId,
            status: "cancelling",
            waitingFor: "cancel_requested",
            assistantMessageId: chat.activeRun.assistantMessageId ?? null,
            nextEventIndex: chat.activeRun.nextEventIndex ?? null,
          }
        : {
            runId: runStatus.runId,
            status: runStatus.status,
            waitingFor: runStatus.waitingFor ?? null,
            assistantMessageId: chat.activeRun.assistantMessageId ?? null,
            nextEventIndex: maxNextEventIndex(
              chat.activeRun.nextEventIndex,
              runStatus.eventsCount,
            ),
          },
      isLocallyStoppedRun(chat, runId) ? "local_mutation" : "backend_poll",
    );
  }

  return publicActiveRun(chat.activeRun);
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
  if (existing) {
    const backendStatusForExisting = backendRunStatuses.get(existing.runId);
    if (isTerminalChatRunStatus(backendStatusForExisting)) {
      return undefined;
    }
  }
  if (!backend) {
    return existing &&
      runBlocksChatTurn(existing.status) &&
      isFreshLocalActiveRun(existing, now)
      ? existing
      : undefined;
  }
  if (!existing) {
    return backend;
  }
  if (existing.runId !== backend.runId) {
    return backend;
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
  activeRun?: ChatActiveRunRecord,
): ChatDetail["activeRun"] {
  if (!activeRun) {
    return undefined;
  }
  return {
    runId: activeRun.runId,
    status: activeRun.status,
    waitingFor: activeRun.waitingFor ?? null,
    ...(activeRun.assistantMessageId
      ? { assistantMessageId: activeRun.assistantMessageId }
      : {}),
    ...(normalizeNextEventIndex(activeRun.nextEventIndex) !== null
      ? { nextEventIndex: normalizeNextEventIndex(activeRun.nextEventIndex) }
      : {}),
  };
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
): Promise<ChatListResponse> {
  await syncBackendSessions(ownerUserId);
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
): Promise<ProjectDetail | null> {
  await syncBackendSessions(ownerUserId);
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
    activeRun: publicActiveRun(chat.activeRun),
    pendingTurn: chat.pendingTurn,
    workspaceSelection: chat.workspaceSelection,
    workspaceSelectionExplicit: Boolean(chat.workspaceSelection),
  };
}

export async function getChatHydrated(
  ownerUserId: string,
  chatId: string,
): Promise<ChatDetail | null> {
  await syncBackendSessions(ownerUserId);
  await syncBackendTranscript(ownerUserId, chatId);
  return getChat(ownerUserId, chatId);
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

export async function sendMessage(
  ownerUserId: string,
  chatId: string,
  payload: {
    content: string;
    options?: ComposerOptions;
  },
) {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  // Validate model selection before mutating chat state
  const selectedModel = requireSelectedOfferingId(
    payload.options?.model ?? chat.model,
  );
  const timestamp = nowIso();
  const userMessage: ChatMessage = {
    id: crypto.randomUUID(),
    role: "user",
    content: payload.content,
    activeSkills: payload.options?.activeSkills,
    activeTools: normalizedActiveTools(
      payload.options?.activeTools,
      payload.options?.webSearch,
    ),
    createdAt: timestamp,
    status: "complete",
  };
  chat.messages.push(userMessage);
  chat.lastMessageAt = timestamp;
  chat.lastMessagePreview = payload.content;
  chat.title ??= titleFromMessage(payload.content);
  if (payload.options?.model) {
    chat.model = payload.options.model;
  }

  const backendSessionId = await ensureChatBackendSession(
    ownerUserId,
    chat.id,
    {
      model: selectedModel,
    },
  );
  const agentResult = await callBackendAgent({
    sessionId: backendSessionId,
    text: payload.content,
    model: selectedModel,
    activeSkills: payload.options?.activeSkills,
    activeTools: payload.options?.activeTools,
    webSearch: payload.options?.webSearch,
  });
  assertBackendSessionMatchesChat(backendSessionId, agentResult.sessionId);
  const assistantMessage = appendAssistantMessage(
    chat,
    agentResult.assistantText,
    agentResult.ok,
  );
  if (chat.projectId) {
    touchProjectInStore(store, chat.projectId);
  }
  return { userMessage, assistantMessage };
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
    createdAt: timestamp,
    status: "complete",
  };
  if (pendingUserMessage && payload.options?.activeSkills?.length) {
    pendingUserMessage.activeSkills = payload.options.activeSkills;
  }
  const assistantMessage: ChatMessage = {
    id: crypto.randomUUID(),
    role: "assistant",
    content: "",
    createdAt: timestamp,
    reasoning: "",
    reasoningStatus: "streaming",
    status: "streaming",
  };

  if (pendingUserMessage) {
    chat.messages.push(assistantMessage);
    chat.pendingTurn = undefined;
  } else {
    chat.messages.push(userMessage, assistantMessage);
  }
  chat.lastMessageAt = timestamp;
  chat.lastMessagePreview = payload.content;
  chat.title ??= titleFromMessage(payload.content);
  if (payload.options?.model) {
    chat.model = payload.options.model;
  }
  if (payload.workspaceSelection) {
    chat.workspaceSelection = payload.workspaceSelection;
  }
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

function streamingAssistantMessage(id: string, createdAt: string): ChatMessage {
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
  return messages
    .slice(userIndex + 1)
    .find((message) => message.role === "assistant");
}

export async function queueDeferredRunInput(
  ownerUserId: string,
  chatId: string,
  payload: {
    content: string;
    options?: ComposerOptions;
    pendingMessageId?: string;
  },
) {
  if ([...payload.content].length > MAX_DEFERRED_INPUT_CHARS) {
    throw new Error("Deferred input is too large.");
  }
  await syncBackendSessions(ownerUserId);

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
  const existingUserMessage = chat.messages.find(
    (message) => message.id === userMessageId && message.role === "user",
  );
  if (existingUserMessage && chat.activeRun) {
    let assistantMessage = findAssistantMessageAfter(
      chat.messages,
      existingUserMessage.id,
    );
    if (!assistantMessage) {
      assistantMessage = streamingAssistantMessage(
        crypto.randomUUID(),
        nowIso(),
      );
      chat.messages.push(assistantMessage);
    }
    chat.activeRun = mergeRunStreamBinding(
      makeActiveRunRecord(
        {
          ...chat.activeRun,
          assistantMessageId: assistantMessage.id,
        },
        "local_mutation",
      ),
      chat.activeRun,
    );
    return {
      userMessage: existingUserMessage,
      assistantMessage,
      activeRun: publicActiveRun(chat.activeRun),
    };
  }

  const client = await requireRuntimeClient({
    auth: "required",
    operation: `submit deferred run input for ${chat.activeRun.runId}`,
  });
  const activeRunId = chat.activeRun.runId;

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
    runStatus.sessionId !== backendSessionIdForChat(chat) ||
    !runBlocksChatTurn(runStatus.status)
  ) {
    chat.activeRun = undefined;
    throw new StaleDeferredRunError("The previous run is no longer active.");
  }
  // Final guard: another concurrent operation (e.g. stop) may have cleared
  // or replaced chat.activeRun between findChat() and this mutation point.
  if (chat.activeRun?.runId !== activeRunId) {
    throw new StaleDeferredRunError(
      "The run was superseded before input could be submitted.",
    );
  }
  chat.activeRun = makeActiveRunRecord(
    {
      runId: runStatus.runId,
      status: runStatus.status,
      waitingFor: runStatus.waitingFor ?? null,
      assistantMessageId: chat.activeRun.assistantMessageId ?? null,
      nextEventIndex: normalizeNextEventIndex(runStatus.eventsCount),
    },
    "backend_poll",
  );

  const activeSkills = normalizedActiveSkills(payload.options?.activeSkills);
  const activeTools = normalizedActiveTools(
    payload.options?.activeTools,
    payload.options?.webSearch,
  );
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

  // Final verification after submitRunInput.
  chat = findChat();
  if (!chat?.activeRun || chat.activeRun.runId !== activeRunId) {
    throw new StaleDeferredRunError("The run changed after input submission.");
  }

  const timestamp = nowIso();
  const userMessage: ChatMessage = {
    id: userMessageId,
    role: "user",
    content: payload.content,
    activeSkills: activeSkills.length ? activeSkills : undefined,
    activeTools: activeTools.length ? activeTools : undefined,
    createdAt: timestamp,
    status: "complete",
  };
  const assistantMessage = streamingAssistantMessage(
    crypto.randomUUID(),
    timestamp,
  );

  chat.messages.push(userMessage);
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
      nextEventIndex: normalizeNextEventIndex(runStatus.eventsCount),
    },
    "local_mutation",
  );

  return {
    userMessage,
    assistantMessage,
    activeRun: publicActiveRun(chat.activeRun),
  };
}

export async function stopActiveRun(
  ownerUserId: string,
  chatId: string,
  options?: { skipSync?: boolean; cancelTimeoutMs?: number },
) {
  if (!options?.skipSync) {
    await syncBackendSessions(ownerUserId);
  }
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  if (!chat.activeRun?.runId) {
    throw new Error("No active run is available to stop.");
  }

  const previousActiveRun = chat.activeRun;
  const runId = previousActiveRun.runId;
  const stoppedMessage = stoppedAssistantMessageController(
    chat,
    previousActiveRun.assistantMessageId ?? null,
  );
  const client = await requireRuntimeClient({
    auth: "required",
    operation: `cancel active run ${runId}`,
  });

  let stoppedMessageCompleted = false;
  const persistStoppedAssistantMessage = () => {
    if (stoppedMessageCompleted) {
      return;
    }
    const currentStore = getStore(ownerUserId);
    const currentChat = currentStore.chats.find((item) => item.id === chatId);
    if (!currentChat) {
      return;
    }
    stoppedMessageCompleted = stoppedMessage.complete(currentChat);
    if (stoppedMessageCompleted && currentChat.projectId) {
      touchProjectInStore(currentStore, currentChat.projectId);
    }
  };

  rememberLocallyStoppedRun(chat, runId);
  chat.activeRun = makeActiveRunRecord(
    {
      runId,
      status: "cancelling",
      waitingFor: previousActiveRun.waitingFor ?? null,
      assistantMessageId: previousActiveRun.assistantMessageId ?? null,
      nextEventIndex: previousActiveRun.nextEventIndex ?? null,
    },
    "local_mutation",
  );

  const restorePreviousRun = () => {
    const currentChat = getStore(ownerUserId).chats.find(
      (item) => item.id === chatId,
    );
    if (currentChat?.activeRun?.runId !== runId) {
      return;
    }
    forgetLocallyStoppedRun(currentChat, runId);
    if (stoppedMessageCompleted) {
      stoppedMessage.restore(currentChat);
    }
    currentChat.activeRun = makeActiveRunRecord(
      {
        runId,
        status: previousActiveRun.status,
        waitingFor: previousActiveRun.waitingFor ?? null,
        assistantMessageId: previousActiveRun.assistantMessageId ?? null,
        nextEventIndex: previousActiveRun.nextEventIndex ?? null,
      },
      "local_mutation",
    );
  };

  let cancelPending: boolean;
  try {
    cancelPending = await settleRuntimeCancel(
      client.sdk.cancelRun(runId),
      options?.cancelTimeoutMs,
      (settled) => {
        if (settled.status === "completed") {
          persistStoppedAssistantMessage();
          void reconcileStoppedRun(ownerUserId, chatId, runId);
          return;
        }
        restorePreviousRun();
      },
    );
  } catch (error) {
    restorePreviousRun();
    throw error;
  }

  if (cancelPending) {
    const currentChat = getStore(ownerUserId).chats.find(
      (item) => item.id === chatId,
    );
    if (currentChat?.activeRun?.runId === runId) {
      currentChat.activeRun = makeActiveRunRecord(
        {
          runId,
          status: "cancelling",
          waitingFor: "cancel_requested",
          assistantMessageId: previousActiveRun.assistantMessageId ?? null,
          nextEventIndex: previousActiveRun.nextEventIndex ?? null,
        },
        "local_mutation",
      );
    }
    persistStoppedAssistantMessage();
  } else {
    persistStoppedAssistantMessage();
    await reconcileStoppedRun(ownerUserId, chatId, runId);
  }

  return {
    activeRun: publicActiveRun(chat.activeRun),
    cancelPending,
  };
}

export async function resumeActiveRun(ownerUserId: string, chatId: string) {
  await syncBackendSessions(ownerUserId);
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

  const client = await requireRuntimeClient({
    auth: "required",
    operation: `resume active run ${chat.activeRun.runId}`,
  });

  await client.sdk.resumeRun(chat.activeRun.runId);
  chat.activeRun = makeActiveRunRecord(
    {
      runId: chat.activeRun.runId,
      status: "running",
      waitingFor: null,
      assistantMessageId: chat.activeRun.assistantMessageId ?? null,
      nextEventIndex: chat.activeRun.nextEventIndex ?? null,
    },
    "local_mutation",
  );

  return {
    activeRun: publicActiveRun(chat.activeRun),
  };
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
  if (patch.reasoning !== undefined) {
    message.reasoning = patch.reasoning;
  }
  if (patch.reasoningStatus !== undefined) {
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
  chat.lastMessageAt = nowIso();
  if (chat.projectId) {
    touchProjectInStore(store, chat.projectId);
  }
  return message;
}

export async function updateChatModel(
  ownerUserId: string,
  chatId: string,
  model: string,
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
  await updateBackendSessionModel(chat, normalized);
  chat.model = normalized;
  return getChat(ownerUserId, chatId);
}

export async function updateChatWorkspaceSelection(
  ownerUserId: string,
  chatId: string,
  selection: WorkspaceSelection | null,
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

  const previous = chat.workspaceSelection;
  if (sameWorkspaceSelection(previous, normalized)) {
    return getChat(ownerUserId, chatId);
  }
  if (normalized) {
    chat.workspaceSelection = normalized;
  } else {
    delete chat.workspaceSelection;
  }
  try {
    if (chat.backendSessionId) {
      await updateBackendSessionWorkspaceSelection(chat, normalized);
    }
  } catch (error) {
    if (previous) {
      chat.workspaceSelection = previous;
    } else {
      delete chat.workspaceSelection;
    }
    throw error;
  }
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
  chat.projectId = projectId;
  chat.lastMessageAt = nowIso();
  return getChat(ownerUserId, chatId);
}

export async function archiveChat(
  ownerUserId: string,
  chatId: string,
  archived: boolean,
) {
  const chat = getStore(ownerUserId).chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  await updateBackendSessionArchive(chat, archived);
  chat.archivedAt = archived ? nowIso() : null;
  return getChat(ownerUserId, chatId);
}

export async function deleteChat(
  ownerUserId: string,
  chatId: string,
): Promise<boolean> {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return false;
  }
  await deleteBackendSession(chat);
  store.chats = store.chats.filter((item) => item.id !== chatId);
  if (chat.projectId) {
    touchProjectInStore(store, chat.projectId);
  }
  return true;
}

export async function deleteArchivedChats(
  ownerUserId: string,
): Promise<number> {
  await syncBackendSessions(ownerUserId);
  const store = getStore(ownerUserId);
  const archivedChats = store.chats.filter((chat) => chat.archivedAt);
  for (const chat of archivedChats) {
    await deleteBackendSession(chat);
  }
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

export async function getSidebar(ownerUserId: string): Promise<SidebarData> {
  await syncBackendSessions(ownerUserId);
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

  const model = requireSelectedOfferingId(params.model ?? chat.model);
  const session = await createBackendSession({
    chatId: chat.id,
    title: chat.title,
    projectId: chat.projectId,
    model,
    workspaceSelection: chat.workspaceSelection,
    runtime: params.runtime,
  });
  chat.backendSessionId = session.sessionId;
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

async function syncBackendSessions(ownerUserId: string): Promise<void> {
  const client = await getRuntimeClient({
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
        nextEventIndex: normalizeNextEventIndex(
          (run as { eventsCount?: unknown }).eventsCount,
        ),
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
    const record = chatRecordFromBackendSession(session, existing);
    if (!record) {
      continue;
    }
    record.activeRun = mergeActiveRunRecord({
      existing: existing?.activeRun,
      backend: session.session_id
        ? activeRunBySession.get(session.session_id)
        : undefined,
      backendRunStatuses,
    });
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
  store.chats = store.chats.filter(
    (chat) => backendChatIds.has(chat.id) || isUnpersistedLocalChat(chat),
  );
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

async function syncBackendTranscript(
  ownerUserId: string,
  chatId: string,
): Promise<void> {
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  const hasIncompleteAssistant =
    chat?.messages.some(
      (message) =>
        message.role === "assistant" && message.status === "streaming",
    ) ?? false;
  if (!chat || (chat.messages.length > 0 && !hasIncompleteAssistant)) {
    return;
  }
  const backendSessionId = chat.backendSessionId ?? chat.id;

  const client = await getRuntimeClient({
    auth: "required",
    operation: `sync persisted transcript for session ${backendSessionId}`,
  });
  if (!client) {
    return;
  }

  let parsed: RuntimeTranscriptResponse;
  try {
    parsed = await client.sdk.getSessionTranscript(backendSessionId, {
      limit: 200,
    });
  } catch (error) {
    throw runtimeOperationError(
      `Cannot sync persisted transcript for session ${backendSessionId}`,
      error,
    );
  }
  const messages = (parsed.items ?? [])
    .map((item) => transcriptItemToMessage(chatId, item))
    .filter((message): message is ChatMessage => Boolean(message));
  if (!messages.length) {
    return;
  }

  // The transcript is authoritative for committed history, while the active
  // run is authoritative for the in-flight turn. A turn's assistant row is
  // not committed until output arrives, so replacing local state with the
  // transcript during refresh leaves no message for SSE resume to target.
  // Preserve that overlay, and synthesize it after a Web process restart.
  const activeRun = chat.activeRun;
  if (activeRun && runBlocksChatTurn(activeRun.status)) {
    const localAssistant =
      (activeRun.assistantMessageId
        ? chat.messages.find(
            (message) =>
              message.id === activeRun.assistantMessageId &&
              message.role === "assistant",
          )
        : undefined) ??
      [...chat.messages]
        .reverse()
        .find(
          (message) =>
            message.role === "assistant" && message.status === "streaming",
        );
    const localAssistantIndex = localAssistant
      ? chat.messages.findIndex((message) => message.id === localAssistant.id)
      : -1;
    const localUser =
      localAssistantIndex > 0 &&
      chat.messages[localAssistantIndex - 1]?.role === "user"
        ? chat.messages[localAssistantIndex - 1]
        : undefined;
    const canonicalTail = messages[messages.length - 1];
    const canonicalHasCurrentUser =
      canonicalTail?.role === "user" &&
      (!localUser || canonicalTail.content === localUser.content);

    if (!canonicalHasCurrentUser && localUser) {
      messages.push(localUser);
    }
    const assistant =
      localAssistant ??
      ({
        id: `inflight:${activeRun.runId}`,
        role: "assistant",
        content: "",
        createdAt: nowIso(),
        reasoning: "",
        reasoningStatus: "streaming",
        status: "streaming",
      } satisfies ChatMessage);
    messages.push(assistant);
    activeRun.assistantMessageId = assistant.id;
  }

  chat.messages = messages;
  const latest = messages[messages.length - 1];
  chat.lastMessageAt = latest?.createdAt ?? chat.lastMessageAt;
  chat.lastMessagePreview = latest?.content || chat.lastMessagePreview;
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

async function deleteBackendSession(chat: ChatRecord): Promise<void> {
  const sessionId = backendSessionIdForChat(chat);
  const client = await requireRuntimeClient({
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
): Promise<void> {
  const sessionId = backendSessionIdForChat(chat);
  const client = await requireRuntimeClient({
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
): Promise<void> {
  const sessionId = backendSessionIdForChat(chat);
  const client = await requireRuntimeClient({
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
): Promise<void> {
  const sessionId = backendSessionIdForChat(chat);
  const client = await requireRuntimeClient({
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

function assertBackendSessionMatchesChat(chatId: string, sessionId?: string) {
  if (sessionId && sessionId !== chatId) {
    throw new Error(
      `Runtime returned session_id ${sessionId}, but Web chat is bound to ${chatId}.`,
    );
  }
}

function normalizeCanonicalChatIds(store: Store) {
  const seen = new Set<string>();
  const seenBackendSessionIds = new Set<string>();
  const normalized: ChatRecord[] = [];
  for (const chat of store.chats) {
    if (LEGACY_LOCAL_CHAT_IDS.has(chat.id)) {
      continue;
    }
    const backendSessionId =
      chat.backendSessionId ??
      (chat.id.startsWith("web-") ? undefined : chat.id);
    if (backendSessionId && seenBackendSessionIds.has(backendSessionId)) {
      continue;
    }
    if (seen.has(chat.id)) {
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

function appendAssistantMessage(
  chat: ChatRecord,
  text: string,
  ok: boolean,
): ChatMessage {
  const timestamp = nowIso();
  const message: ChatMessage = {
    id: crypto.randomUUID(),
    role: "assistant",
    content: text,
    createdAt: timestamp,
    status: ok ? "complete" : "failed",
  };
  chat.messages.push(message);
  chat.lastMessageAt = timestamp;
  chat.lastMessagePreview = text;
  return message;
}

async function callBackendAgent(params: {
  sessionId?: string;
  text: string;
  model: string;
  activeSkills?: string[];
  activeTools?: string[];
  webSearch?: boolean;
}): Promise<{ ok: boolean; sessionId?: string; assistantText: string }> {
  const controller = new AbortController();
  const timeout = setTimeout(
    () => controller.abort(),
    AGENT_RESPONSE_TIMEOUT_MS,
  );

  try {
    const client = await requireRuntimeClient({
      auth: "required",
      operation: "start web chat turn",
    });
    const modelSelection = await resolveModelOfferingSelection(
      client,
      params.model,
    );
    const activeSkills = normalizedActiveSkills(params.activeSkills);
    const activeTools = normalizedActiveTools(
      params.activeTools,
      params.webSearch,
    );

    const run = await client.sdk.createRun(
      {
        message: params.text,
        sessionId: params.sessionId,
        modelSelection,
        allowSkills: activeSkills.length ? activeSkills : undefined,
        enabledTools: activeTools,
        context: {
          source: "web_v1",
          edge_profile:
            activeSkills.length || activeTools.length
              ? {
                  ...(activeSkills.length
                    ? { active_skills: activeSkills }
                    : {}),
                  ...(activeTools.length ? { active_tools: activeTools } : {}),
                }
              : undefined,
        },
      },
      {
        signal: controller.signal,
      },
    );
    if (run.runId) {
      const streamed = await readRunStream(client, run.runId);
      if (streamed.error) {
        throw new Error(streamed.error);
      }
      if (streamed.assistantText.trim()) {
        return {
          ok: true,
          sessionId: run.sessionId,
          assistantText: streamed.assistantText.trim(),
        };
      }
    }

    return {
      ok: true,
      sessionId: run.sessionId,
      assistantText: run.runId
        ? "Astra completed the run without returning visible text."
        : "The run was accepted by Astra.",
    };
  } catch (error) {
    const message =
      error instanceof Error && error.name === "AbortError"
        ? `timed out after ${AGENT_RESPONSE_TIMEOUT_MS / 1000}s`
        : runtimeErrorDetail(error, "unknown error");
    const prefix =
      error instanceof RuntimeClientError
        ? "Astra runtime rejected the request"
        : "I could not reach the Astra runtime from the web UI";
    return {
      ok: false,
      assistantText: `${prefix}. (${message})`,
    };
  } finally {
    clearTimeout(timeout);
  }
}

async function readRunStream(
  client: WebRuntimeClient,
  runId: string,
): Promise<StreamResult> {
  const startedAt = Date.now();
  let nextOffset = 0;
  let assistantText = "";

  while (Date.now() - startedAt < AGENT_STREAM_TIMEOUT_MS) {
    const controller = new AbortController();
    const remainingMs = Math.max(
      1,
      AGENT_STREAM_TIMEOUT_MS - (Date.now() - startedAt),
    );
    const timeout = setTimeout(() => controller.abort(), remainingMs);

    try {
      const response = await client.fetchResponse(
        `${chatRunStreamPath(runId)}${buildQueryString({ last_index: nextOffset })}`,
        {
          auth: "required",
          operation: `stream run ${runId}`,
          signal: controller.signal,
        },
      );

      if (!response.ok) {
        return { assistantText, error: await readRuntimeErrorDetail(response) };
      }

      const parsed = parseRunSseText(await response.text());
      if (parsed.assistantText) {
        assistantText = parsed.assistantText;
      }
      if (
        typeof parsed.nextOffset === "number" &&
        parsed.nextOffset > nextOffset
      ) {
        nextOffset = parsed.nextOffset;
      }
      if (parsed.error || parsed.finished) {
        return {
          assistantText,
          error: parsed.error,
          finished: parsed.finished,
          nextOffset,
        };
      }
    } catch (error) {
      const detail =
        error instanceof Error && error.name === "AbortError"
          ? `timed out after ${AGENT_STREAM_TIMEOUT_MS / 1000}s while waiting for Astra`
          : error instanceof Error
            ? error.message
            : "unknown stream error";
      return { assistantText, error: detail };
    } finally {
      clearTimeout(timeout);
    }

    await new Promise((resolve) => setTimeout(resolve, 500));
  }

  return {
    assistantText,
    error: `timed out after ${AGENT_STREAM_TIMEOUT_MS / 1000}s while waiting for Astra`,
  };
}

function parseRunSseText(text: string): StreamResult {
  let assistantText = "";
  let finalText = "";
  let error: string | undefined;
  let finished = false;
  let nextOffset = 0;

  for (const event of parseSseDataEvents(text.replace(/\r\n/g, "\n"))) {
    const record = event as Record<string, unknown>;
    const type = typeof record.type === "string" ? record.type : "";
    if (typeof record.index === "number" && Number.isFinite(record.index)) {
      nextOffset = Math.max(nextOffset, Math.trunc(record.index) + 1);
    }

    if (type === "text_delta" && typeof record.content === "string") {
      assistantText += record.content;
    } else if (type === "text_done" && typeof record.full_text === "string") {
      finalText = record.full_text;
    } else if (
      type === "turn_complete" &&
      typeof record.assistant_text === "string"
    ) {
      finalText = record.assistant_text;
    } else if (type === "run_error") {
      const runError =
        typeof record.message === "string"
          ? record.message
          : typeof record.error === "string"
            ? record.error
            : undefined;
      if (runError) {
        error = runError;
      }
    } else if (type === "error" && typeof record.message === "string") {
      error = record.message;
    } else if (
      type === "run_finished" &&
      record.status === "failed" &&
      typeof record.error === "string"
    ) {
      error = record.error;
      finished = true;
    } else if (
      type === "run_finished" &&
      (record.status === "paused" || record.status === "interrupted")
    ) {
      finished = true;
    } else if (type === "run_finished") {
      finished = true;
    }
  }

  return {
    assistantText: finalText || assistantText,
    error,
    finished,
    nextOffset,
  };
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
  const accessToken = client.config.accessToken;
  if (!accessToken) {
    throw new ModelOfferingSelectionError(
      "authentication_required",
      "authenticated model access is required",
    );
  }

  let modelsPromise = modelCache.get(accessToken);
  if (!modelsPromise) {
    modelsPromise = client.sdk.listModels();
    modelCache.set(accessToken, modelsPromise);
  }

  const models = await modelsPromise.catch((error: unknown) => {
    modelCache.invalidate(accessToken);
    throw new ModelOfferingSelectionError(
      "catalog_unavailable",
      error instanceof Error ? error.message : "Model catalog is unavailable",
    );
  });
  const matched = models.find(
    (item) => item.offering_id === offeringId && item.is_active,
  );
  if (!matched) {
    throw new ModelOfferingSelectionError(
      "offering_unavailable",
      `Model Offering '${offeringId}' is not available`,
    );
  }
  return { offeringId };
}
