import type { RuntimeConfig } from "@/lib/runtime-config";
import {
  buildQueryString,
  chatRunStreamPath,
  parseSseDataEvents,
  type RunListResponse,
  type RunStatus,
  type RuntimeSessionListResponse,
  type RuntimeSessionResponse,
  type RuntimeTranscriptItemResponse,
  type RuntimeTranscriptResponse,
  type StreamEvent,
} from "@astra/sdk";
import {
  activeRunPriority,
  isTerminalChatRunStatus,
  runBlocksChatTurn,
} from "@/lib/chat-run-state";
import {
  RuntimeClientError,
  WebRuntimeClient,
  getRuntimeClient,
  readRuntimeErrorDetail,
  requireRuntimeClient,
  runtimeErrorDetail,
} from "@/lib/runtime-client";
import type {
  ChatDetail,
  ChatListResponse,
  ChatMessage,
  ChatSummary,
  ComposerOptions,
  CreateProjectRequest,
  KnowledgeFile,
  ModelSummary,
  ProjectDetail,
  ProjectListResponse,
  ProjectSummary,
  SearchResponse,
  SidebarData,
  UserSummary,
} from "@/lib/api/types";
import { modelCache } from "@/lib/api/model-cache";

type ChatActiveRunSource = "backend_poll" | "stream" | "local_mutation";

type ChatActiveRunRecord = NonNullable<ChatDetail["activeRun"]> & {
  source: ChatActiveRunSource;
  observedAt: string;
};

type ChatRecord = ChatSummary & {
  createdAt: string;
  archivedAt?: string | null;
  backendSessionId?: string | null;
  messages: ChatMessage[];
  activeRun?: ChatActiveRunRecord;
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
const SESSION_SYNC_PAGE_SIZE = 200;
const RUN_SYNC_PAGE_SIZE = 200;
const MAX_DEFERRED_INPUT_CHARS = 20_000;
const LEGACY_LOCAL_CHAT_IDS = new Set(["chat-web-agent-notes"]);

type StreamResult = {
  assistantText: string;
  error?: string;
  finished?: boolean;
  nextOffset?: number;
};

function runtimeOperationError(operation: string, error: unknown) {
  return new Error(`${operation}: ${runtimeErrorDetail(error)}`);
}

declare global {
  // eslint-disable-next-line no-var
  var __astraWebStores: Record<string, Store> | undefined;
}

const DEFAULT_STORE_SCOPE = "offline";

function nowIso() {
  return new Date().toISOString();
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

function makeActiveRunRecord(
  activeRun: NonNullable<ChatDetail["activeRun"]>,
  source: ChatActiveRunSource,
  observedAt = nowIso(),
): ChatActiveRunRecord {
  return {
    runId: activeRun.runId,
    status: activeRun.status,
    waitingFor: activeRun.waitingFor ?? null,
    source,
    observedAt,
  };
}

function isFreshLocalActiveRun(
  activeRun: ChatActiveRunRecord,
  now = Date.now(),
) {
  if (activeRun.source === "backend_poll") {
    return false;
  }
  const observedAt = Date.parse(activeRun.observedAt);
  return (
    Number.isFinite(observedAt) && now - observedAt <= LOCAL_ACTIVE_RUN_GRACE_MS
  );
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
  return backend;
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
  };
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

export function listModelSummaries(): ModelSummary[] {
  return [
    {
      id: "sonnet-4.6-adaptive",
      name: "Sonnet 4.6",
      subtitle: "Responsive everyday work",
      tier: "included",
    },
    {
      id: "opus-4.7",
      name: "Opus 4.7",
      subtitle: "Most capable for ambitious work",
      tier: "upgrade",
    },
    {
      id: "haiku-4.5",
      name: "Haiku 4.5",
      subtitle: "Fastest and most efficient",
      tier: "included",
    },
  ];
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
  },
) {
  const store = getStore(ownerUserId);
  const timestamp = nowIso();
  const projectId = payload.projectId ?? null;
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
  const timestamp = nowIso();
  const userMessage: ChatMessage = {
    id: crypto.randomUUID(),
    role: "user",
    content: payload.content,
    activeSkills: payload.options?.activeSkills,
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

  const agentResult = await callBackendAgent({
    sessionId: chat.id,
    text: payload.content,
    model: payload.options?.model,
    activeSkills: payload.options?.activeSkills,
  });
  assertBackendSessionMatchesChat(chat.id, agentResult.sessionId);
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
    ? makeActiveRunRecord(
        activeRun,
        "source" in activeRun ? activeRun.source : "stream",
        "observedAt" in activeRun ? activeRun.observedAt : nowIso(),
      )
    : undefined;
  return chat.activeRun;
}

export async function queueDeferredRunInput(
  ownerUserId: string,
  chatId: string,
  payload: {
    content: string;
    options?: ComposerOptions;
  },
) {
  if ([...payload.content].length > MAX_DEFERRED_INPUT_CHARS) {
    throw new Error("Deferred input is too large.");
  }
  await syncBackendSessions(ownerUserId);
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  if (!chat.activeRun?.runId) {
    throw new StaleDeferredRunError();
  }

  const client = await requireRuntimeClient({
    auth: "required",
    operation: `submit deferred run input for ${chat.activeRun.runId}`,
  });
  const activeRunId = chat.activeRun.runId;
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
  if (
    runStatus.sessionId !== backendSessionIdForChat(chat) ||
    !runBlocksChatTurn(runStatus.status)
  ) {
    chat.activeRun = undefined;
    throw new StaleDeferredRunError("The previous run is no longer active.");
  }
  chat.activeRun = makeActiveRunRecord(
    {
      runId: runStatus.runId,
      status: runStatus.status,
      waitingFor: runStatus.waitingFor ?? null,
    },
    "backend_poll",
  );

  const activeSkills = normalizedActiveSkills(payload.options?.activeSkills);
  try {
    await client.sdk.submitRunInput(activeRunId, {
      idempotencyKey: crypto.randomUUID(),
      input: {
        content: payload.content,
        active_skills: activeSkills,
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

  const timestamp = nowIso();
  const userMessage: ChatMessage = {
    id: crypto.randomUUID(),
    role: "user",
    content: payload.content,
    activeSkills: activeSkills.length ? activeSkills : undefined,
    createdAt: timestamp,
    status: "complete",
  };

  chat.messages.push(userMessage);
  chat.lastMessageAt = timestamp;
  chat.lastMessagePreview = payload.content;
  if (chat.projectId) {
    touchProjectInStore(store, chat.projectId);
  }
  chat.activeRun = makeActiveRunRecord(
    {
      runId: chat.activeRun.runId,
      status: "input-queued",
      waitingFor: "user_input",
    },
    "local_mutation",
  );

  return {
    userMessage,
    activeRun: publicActiveRun(chat.activeRun),
  };
}

export async function stopActiveRun(ownerUserId: string, chatId: string) {
  await syncBackendSessions(ownerUserId);
  const store = getStore(ownerUserId);
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  if (!chat.activeRun?.runId) {
    throw new Error("No active run is available to stop.");
  }

  const client = await requireRuntimeClient({
    auth: "required",
    operation: `cancel active run ${chat.activeRun.runId}`,
  });

  await client.sdk.cancelRun(chat.activeRun.runId);
  chat.activeRun = makeActiveRunRecord(
    {
      runId: chat.activeRun.runId,
      status: "cancelling",
      waitingFor: null,
    },
    "local_mutation",
  );

  return {
    activeRun: publicActiveRun(chat.activeRun),
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
    message.artifacts = patch.artifacts;
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

  const model = params.model ?? chat.model ?? "sonnet-4.6-adaptive";
  const session = await createBackendSession({
    title: chat.title,
    projectId: chat.projectId,
    model,
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

  return {
    id: existing?.id ?? session.session_id,
    title,
    projectId,
    createdAt,
    lastMessageAt: updatedAt,
    lastMessagePreview: existing?.lastMessagePreview ?? title ?? undefined,
    archivedAt,
    model,
    backendSessionId: session.session_id,
    messages: existing?.messages ?? [],
    activeRun: existing?.activeRun,
    pendingTurn: existing?.pendingTurn,
  };
}

async function listAllBackendRuns(
  client: WebRuntimeClient,
  ownerUserId: string,
): Promise<RunStatus[]> {
  const runs: RunStatus[] = [];
  let offset = 0;

  for (;;) {
    let parsed: RunListResponse;
    try {
      parsed = await client.sdk.listRuns({
        limit: RUN_SYNC_PAGE_SIZE,
        offset,
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
    const total =
      typeof parsed.total === "number" && Number.isFinite(parsed.total)
        ? parsed.total
        : null;

    if (
      page.length === 0 ||
      page.length < responseLimit ||
      (total !== null && offset + page.length >= total)
    ) {
      break;
    }

    offset += page.length;
  }

  return runs;
}

async function listAllBackendSessions(
  client: WebRuntimeClient,
  ownerUserId: string,
): Promise<RuntimeSessionResponse[]> {
  const sessions: RuntimeSessionResponse[] = [];
  let offset = 0;

  for (;;) {
    let parsed: RuntimeSessionListResponse;
    try {
      parsed = await client.sdk.listRuntimeSessions({
        limit: SESSION_SYNC_PAGE_SIZE,
        offset,
      });
    } catch (error) {
      throw runtimeOperationError(
        `Cannot sync persisted sessions for user ${ownerUserId}`,
        error,
      );
    }

    const page = Array.isArray(parsed.sessions) ? parsed.sessions : [];
    sessions.push(...page);

    const responseLimit =
      typeof parsed.limit === "number" && parsed.limit > 0
        ? parsed.limit
        : SESSION_SYNC_PAGE_SIZE;
    const total =
      typeof parsed.total === "number" && Number.isFinite(parsed.total)
        ? parsed.total
        : null;

    if (
      page.length === 0 ||
      page.length < responseLimit ||
      (total !== null && offset + page.length >= total)
    ) {
      break;
    }

    offset += page.length;
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

  for (const run of runs) {
    backendRunStatuses.set(run.runId, run.status);
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
  chat.messages = messages;
  const latest = messages[messages.length - 1];
  chat.lastMessageAt = latest?.createdAt ?? chat.lastMessageAt;
  chat.lastMessagePreview = latest?.content ?? chat.lastMessagePreview;
}

async function createBackendSession(params: {
  title: string | null;
  projectId: string | null;
  model: string;
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
        project_id: params.projectId,
        initial_model: params.model,
        current_model: params.model,
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
  let parsed: RuntimeSessionResponse;
  try {
    parsed = await client.sdk.getRuntimeSession(sessionId);
  } catch (error) {
    throw runtimeOperationError(
      `Cannot read persisted session ${sessionId} before model update`,
      error,
    );
  }

  const metadata = {
    ...(parsed.metadata ?? {}),
    current_model: model,
  };

  try {
    await client.sdk.updateRuntimeSession(sessionId, { metadata });
  } catch (error) {
    throw runtimeOperationError(
      `Cannot update persisted session ${sessionId} model`,
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
  model?: string;
  activeSkills?: string[];
}): Promise<{ ok: boolean; sessionId?: string; assistantText: string }> {
  const controller = new AbortController();
  const timeout = setTimeout(
    () => controller.abort(),
    AGENT_RESPONSE_TIMEOUT_MS,
  );

  try {
    const client = await requireRuntimeClient({
      auth: "optional",
      operation: "start web chat turn",
    });
    const model = await resolveBackendModelName(client, params.model);
    const activeSkills = normalizedActiveSkills(params.activeSkills);

    const run = await client.sdk.createRun(
      {
        message: params.text,
        sessionId: params.sessionId,
        model,
        allowSkills: activeSkills.length ? activeSkills : undefined,
        context: {
          source: "web_v1",
          edge_profile: activeSkills.length
            ? { active_skills: activeSkills }
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
    const record = event as StreamEvent & Record<string, unknown>;
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
    } else if (type === "error" && typeof record.message === "string") {
      error = record.message;
    } else if (
      type === "run_finished" &&
      record.status === "failed" &&
      typeof record.error === "string"
    ) {
      error = record.error;
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

export async function resolveBackendModelName(
  runtime: RuntimeConfig | WebRuntimeClient,
  model?: string,
) {
  if (!model) {
    return model;
  }

  try {
    const client =
      runtime instanceof WebRuntimeClient
        ? runtime
        : new WebRuntimeClient(runtime);
    const accessToken = client.config.accessToken;
    if (!accessToken) {
      return model;
    }

    const cached = modelCache.get(accessToken);
    let modelsPromise: Promise<Array<{ model_id?: string; name?: string }>>;
    if (cached) {
      modelsPromise = cached;
    } else {
      modelsPromise = client.sdk.listModels();
      modelCache.set(accessToken, modelsPromise);
    }

    const models = await modelsPromise.catch((err) => {
      modelCache.invalidate(accessToken);
      throw err;
    });
    const matched = models.find(
      (item) => item.model_id === model || item.name === model,
    );
    return matched?.name ?? model;
  } catch {
    return model;
  }
}
