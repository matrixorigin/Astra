import { getRuntimeConfig, type RuntimeConfig } from '@/lib/runtime-config';
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
} from '@/lib/api/types';

type ChatRecord = ChatSummary & {
  createdAt: string;
  backendSessionId?: string | null;
  archivedAt?: string | null;
  messages: ChatMessage[];
  pendingTurn?: {
    messageId: string;
    content: string;
    options: ComposerOptions;
  };
};

type ProjectRecord = ProjectDetail['project'];

type Store = {
  projects: ProjectRecord[];
  chats: ChatRecord[];
  files: Record<string, KnowledgeFile[]>;
};

const AGENT_RESPONSE_TIMEOUT_MS = 30_000;
const AGENT_STREAM_TIMEOUT_MS = 180_000;

type BackendChatResponse = {
  session_id?: string;
  run_id?: string;
  status?: string;
};

type BackendModelListItem = {
  model_id?: string;
  name?: string;
};

type StreamResult = {
  assistantText: string;
  error?: string;
  finished?: boolean;
  nextOffset?: number;
};

declare global {
  // eslint-disable-next-line no-var
  var __astraWebStore: Store | undefined;
}

function nowIso() {
  return new Date().toISOString();
}

function titleFromMessage(message: string) {
  const text = message.trim().replace(/\s+/g, ' ');
  if (!text) {
    return null;
  }
  return text.length > 56 ? `${text.slice(0, 53)}...` : text;
}

function normalizedActiveSkills(skills?: string[]) {
  if (!Array.isArray(skills)) {
    return [];
  }
  return [...new Set(skills.map((skill) => skill.trim()).filter(Boolean))].sort((left, right) => (
    left.localeCompare(right)
  ));
}

function seedStore(): Store {
  const now = nowIso();
  const projectId = 'project-web-agent';
  return {
    projects: [
      {
        id: projectId,
        name: 'Web agent workspace',
        description: 'Session durability, context, and remote agent UI notes.',
        instructions:
          'Prefer concise implementation notes. Keep session state durable and auditable.',
        memory: 'The user is validating the Astra web agent v1 workflow.',
        visibility: 'private',
        starred: true,
        createdAt: now,
        updatedAt: now,
      },
    ],
    chats: [
      {
        id: 'chat-web-agent-notes',
        title: 'Web agent UI refactor',
        lastMessageAt: now,
        lastMessagePreview: 'The old operator console is being replaced by the v1 workspace.',
        projectId,
        createdAt: now,
        backendSessionId: null,
        messages: [
          {
            id: 'msg-seed-user',
            role: 'user',
            content: 'Track the web agent UI refactor.',
            createdAt: now,
            status: 'complete',
          },
          {
            id: 'msg-seed-assistant',
            role: 'assistant',
            content:
              'The workspace is ready to organize chats, projects, knowledge, and artifacts.',
            createdAt: now,
            status: 'complete',
          },
        ],
      },
    ],
    files: {
      [projectId]: [],
    },
  };
}

export function getStore() {
  globalThis.__astraWebStore ??= seedStore();
  return globalThis.__astraWebStore;
}

export function getCurrentUser(): UserSummary {
  return {
    id: 'local-user',
    name: 'Astra user',
    plan: 'free',
  };
}

export function listModelSummaries(): ModelSummary[] {
  return [
    {
      id: 'sonnet-4.6-adaptive',
      name: 'Sonnet 4.6',
      subtitle: 'Responsive everyday work',
      tier: 'included',
    },
    {
      id: 'opus-4.7',
      name: 'Opus 4.7',
      subtitle: 'Most capable for ambitious work',
      tier: 'upgrade',
    },
    {
      id: 'haiku-4.5',
      name: 'Haiku 4.5',
      subtitle: 'Fastest and most efficient',
      tier: 'included',
    },
  ];
}

export function listChats(params: {
  projectId?: string | null;
  q?: string | null;
  cursor?: string | null;
  limit?: number;
  archived?: boolean;
}): ChatListResponse {
  const store = getStore();
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
      const expected = params.projectId === 'null' ? null : params.projectId ?? null;
      if (chat.projectId !== expected) {
        return false;
      }
    }
    if (query) {
      const haystack = `${chat.title ?? ''} ${chat.lastMessagePreview ?? ''}`.toLowerCase();
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

export function listProjects(params: {
  q?: string | null;
  sort?: 'activity' | 'created' | 'name';
  cursor?: string | null;
  limit?: number;
}): ProjectListResponse {
  const store = getStore();
  const limit = params.limit ?? 24;
  const offset = Number(params.cursor ?? 0);
  const query = params.q?.trim().toLowerCase();
  const sort = params.sort ?? 'activity';
  let projects = store.projects.filter((project) => {
    if (!query) {
      return true;
    }
    return `${project.name} ${project.description ?? ''}`.toLowerCase().includes(query);
  });
  projects = projects.sort((a, b) => {
    if (sort === 'name') {
      return a.name.localeCompare(b.name);
    }
    if (sort === 'created') {
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

export function createProject(payload: CreateProjectRequest) {
  const store = getStore();
  const timestamp = nowIso();
  const project: ProjectRecord = {
    id: crypto.randomUUID(),
    name: payload.name.trim(),
    description: payload.description?.trim() || null,
    instructions: payload.instructions?.trim() || null,
    memory: null,
    visibility: 'private',
    starred: false,
    createdAt: timestamp,
    updatedAt: timestamp,
  };
  store.projects.unshift(project);
  store.files[project.id] = [];
  return projectSummary(project);
}

export function getProject(projectId: string): ProjectDetail | null {
  const store = getStore();
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

export function updateProject(projectId: string, payload: Partial<CreateProjectRequest>) {
  const store = getStore();
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
  return getProject(projectId);
}

export function setProjectStar(projectId: string, starred: boolean) {
  const store = getStore();
  const project = store.projects.find((item) => item.id === projectId);
  if (!project) {
    return null;
  }
  project.starred = starred;
  project.updatedAt = nowIso();
  return { starred };
}

export function addProjectFile(projectId: string, file: File): KnowledgeFile | null {
  const store = getStore();
  if (!store.projects.some((project) => project.id === projectId)) {
    return null;
  }
  const timestamp = nowIso();
  const record: KnowledgeFile = {
    id: crypto.randomUUID(),
    filename: file.name,
    mimeType: file.type || 'application/octet-stream',
    sizeBytes: file.size,
    sourceType: 'upload',
    indexStatus: 'indexed',
    indexedAt: timestamp,
    createdAt: timestamp,
  };
  store.files[projectId] ??= [];
  store.files[projectId].unshift(record);
  touchProject(projectId);
  return record;
}

export function removeProjectFile(projectId: string, fileId: string) {
  const store = getStore();
  const files = store.files[projectId];
  if (!files) {
    return false;
  }
  const before = files.length;
  store.files[projectId] = files.filter((file) => file.id !== fileId);
  if (store.files[projectId].length !== before) {
    touchProject(projectId);
    return true;
  }
  return false;
}

export function getChat(chatId: string): ChatDetail | null {
  const store = getStore();
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
    },
    messages: chat.messages,
    project: project ? { id: project.id, name: project.name } : undefined,
    pendingTurn: chat.pendingTurn,
  };
}

export async function createChatWithMessage(payload: {
  message: string;
  model: string;
  options: Omit<ComposerOptions, 'model'>;
  projectId?: string | null;
}) {
  const timestamp = nowIso();
  const provisionalId = crypto.randomUUID();
  const userMessage: ChatMessage = {
    id: crypto.randomUUID(),
    role: 'user',
    content: payload.message,
    createdAt: timestamp,
    status: 'complete',
  };
  const chat: ChatRecord = {
    id: provisionalId,
    title: titleFromMessage(payload.message),
    projectId: payload.projectId ?? null,
    createdAt: timestamp,
    lastMessageAt: timestamp,
    lastMessagePreview: payload.message,
    backendSessionId: null,
    messages: [userMessage],
    pendingTurn: {
      messageId: userMessage.id,
      content: payload.message,
      options: {
        ...payload.options,
        model: payload.model,
      },
    },
  };

  getStore().chats.unshift(chat);
  if (chat.projectId) {
    touchProject(chat.projectId);
  }
  return {
    chatId: chat.id,
    messageId: userMessage.id,
  };
}

export async function sendMessage(chatId: string, payload: {
  content: string;
  options?: ComposerOptions;
}) {
  const store = getStore();
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  const timestamp = nowIso();
  const userMessage: ChatMessage = {
    id: crypto.randomUUID(),
    role: 'user',
    content: payload.content,
    createdAt: timestamp,
    status: 'complete',
  };
  chat.messages.push(userMessage);
  chat.lastMessageAt = timestamp;
  chat.lastMessagePreview = payload.content;
  chat.title ??= titleFromMessage(payload.content);

  const agentResult = await callBackendAgent({
    sessionId: chat.backendSessionId ?? undefined,
    text: payload.content,
    model: payload.options?.model,
    activeSkills: payload.options?.activeSkills,
  });
  if (agentResult.sessionId) {
    chat.backendSessionId = agentResult.sessionId;
  }
  const assistantMessage = appendAssistantMessage(chat, agentResult.assistantText, agentResult.ok);
  if (chat.projectId) {
    touchProject(chat.projectId);
  }
  return { userMessage, assistantMessage };
}

export function beginStreamingMessage(chatId: string, payload: {
  content: string;
  options?: ComposerOptions;
  pendingMessageId?: string;
}) {
  const store = getStore();
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }

  const timestamp = nowIso();
  const pendingUserMessage = payload.pendingMessageId && chat.pendingTurn?.messageId === payload.pendingMessageId
    ? chat.messages.find((item) => item.id === payload.pendingMessageId && item.role === 'user')
    : undefined;
  if (payload.pendingMessageId && !pendingUserMessage) {
    return null;
  }
  const userMessage: ChatMessage = pendingUserMessage ?? {
    id: crypto.randomUUID(),
    role: 'user',
    content: payload.content,
    createdAt: timestamp,
    status: 'complete',
  };
  const assistantMessage: ChatMessage = {
    id: crypto.randomUUID(),
    role: 'assistant',
    content: '',
    createdAt: timestamp,
    reasoning: '',
    reasoningStatus: 'streaming',
    status: 'streaming',
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
  if (chat.projectId) {
    touchProject(chat.projectId);
  }

  return {
    userMessage,
    assistantMessage,
    backendSessionId: chat.backendSessionId ?? undefined,
  };
}

export function updateStreamingAssistantMessage(chatId: string, messageId: string, patch: {
  content?: string;
  reasoning?: string;
  reasoningStatus?: ChatMessage['reasoningStatus'];
  status?: ChatMessage['status'];
  backendSessionId?: string;
}) {
  const store = getStore();
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }

  if (patch.backendSessionId) {
    chat.backendSessionId = patch.backendSessionId;
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
  chat.lastMessageAt = nowIso();
  if (chat.projectId) {
    touchProject(chat.projectId);
  }
  return message;
}

export function moveChat(chatId: string, projectId: string | null) {
  const store = getStore();
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  if (projectId && !store.projects.some((project) => project.id === projectId)) {
    return null;
  }
  chat.projectId = projectId;
  chat.lastMessageAt = nowIso();
  return getChat(chatId);
}

export async function archiveChat(chatId: string, archived: boolean) {
  const chat = getStore().chats.find((item) => item.id === chatId);
  if (!chat) {
    return null;
  }
  await updateBackendSessionArchiveIfNeeded(chat, archived);
  chat.archivedAt = archived ? nowIso() : null;
  return getChat(chatId);
}

export async function deleteChat(chatId: string): Promise<boolean> {
  const store = getStore();
  const chat = store.chats.find((item) => item.id === chatId);
  if (!chat) {
    return false;
  }
  await deleteBackendSessionIfNeeded(chat);
  store.chats = store.chats.filter((item) => item.id !== chatId);
  if (chat.projectId) {
    touchProject(chat.projectId);
  }
  return true;
}

export async function deleteArchivedChats(): Promise<number> {
  const store = getStore();
  const archivedChats = store.chats.filter((chat) => chat.archivedAt);
  for (const chat of archivedChats) {
    await deleteBackendSessionIfNeeded(chat);
  }
  const archivedIds = new Set(archivedChats.map((chat) => chat.id));
  store.chats = store.chats.filter((chat) => !archivedIds.has(chat.id));
  const touchedProjectIds = new Set(
    archivedChats.map((chat) => chat.projectId).filter((projectId): projectId is string => Boolean(projectId)),
  );
  for (const projectId of touchedProjectIds) {
    touchProject(projectId);
  }
  return archivedChats.length;
}

export function getSidebar(): SidebarData {
  const store = getStore();
  const recentChats: Array<{ kind: 'chat'; id: string; title: string; href: string; updatedAt: string }> =
    store.chats.filter((chat) => !chat.archivedAt).map((chat) => ({
      kind: 'chat',
      id: chat.id,
      title: chat.title ?? chat.lastMessagePreview?.slice(0, 48) ?? 'Untitled',
      href: chat.projectId ? `/projects/${chat.projectId}/chats/${chat.id}` : `/chats/${chat.id}`,
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
          kind: 'chat' as const,
          id: chat.id,
          title: chat.title ?? chat.lastMessagePreview?.slice(0, 48) ?? 'Untitled',
          href: `/projects/${project.id}/chats/${chat.id}`,
          updatedAt: chat.lastMessageAt,
        }));
      if (!chats.length) {
        return null;
      }
      const updatedAt = chats[0]?.updatedAt ?? project.updatedAt;
      return {
        project: {
          kind: 'project' as const,
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
      kind: 'chat' as const,
      id: chat.id,
      title: chat.title ?? chat.lastMessagePreview?.slice(0, 48) ?? 'Untitled',
      href: `/chats/${chat.id}`,
      updatedAt: chat.lastMessageAt,
    }));
  const archivedChats: Array<{ kind: 'chat'; id: string; title: string; href: string; updatedAt: string }> =
    store.chats.filter((chat) => chat.archivedAt).map((chat) => ({
      kind: 'chat',
      id: chat.id,
      title: chat.title ?? chat.lastMessagePreview?.slice(0, 48) ?? 'Untitled',
      href: chat.projectId ? `/projects/${chat.projectId}/chats/${chat.id}` : `/chats/${chat.id}`,
      updatedAt: chat.archivedAt ?? chat.lastMessageAt,
    }));
  const recentProjects: Array<{ kind: 'project'; id: string; title: string; href: string; updatedAt: string }> =
    store.projects.map((project) => ({
      kind: 'project',
      id: project.id,
      title: project.name,
      href: `/projects/${project.id}`,
      updatedAt: project.updatedAt,
    }));
  const recents = [...recentChats, ...recentProjects]
    .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt))
    .slice(0, 20);
  const untitled = recentChats.filter((chat) => chat.title === 'Untitled');
  return {
    recents,
    recentProjectGroups,
    recentOtherChats,
    untitled,
    archivedChats: archivedChats.sort((a, b) => b.updatedAt.localeCompare(a.updatedAt)).slice(0, 50),
    user: getCurrentUser(),
  };
}

export function searchData(query: string): SearchResponse {
  const q = query.trim().toLowerCase();
  const store = getStore();
  const projects = store.projects
    .filter((project) => !q || `${project.name} ${project.description ?? ''}`.toLowerCase().includes(q))
    .slice(0, 8)
    .map((project) => ({ id: project.id, name: project.name, updatedAt: project.updatedAt }));
  const chats = store.chats
    .filter((chat) => !chat.archivedAt)
    .filter((chat) => !q || `${chat.title ?? ''} ${chat.lastMessagePreview ?? ''}`.toLowerCase().includes(q))
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
  };
}

async function deleteBackendSessionIfNeeded(chat: ChatRecord): Promise<void> {
  if (!chat.backendSessionId) {
    return;
  }

  const config = await getRuntimeConfig();
  if (config.mode !== 'live' || !config.apiUrl) {
    throw new Error('Cannot delete persisted session: runtime API is not configured.');
  }
  if (!config.accessToken) {
    throw new Error('Cannot delete persisted session: runtime authentication is missing.');
  }

  const response = await fetch(
    new URL(`/sessions/${encodeURIComponent(chat.backendSessionId)}`, config.apiUrl).toString(),
    {
      method: 'DELETE',
      headers: {
        Authorization: `Bearer ${config.accessToken}`,
      },
      cache: 'no-store',
    },
  );

  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;
    try {
      const body = (await response.json()) as { detail?: string; error?: string };
      detail = body.detail ?? body.error ?? detail;
    } catch {
      // Preserve the HTTP status.
    }
    throw new Error(`Cannot delete persisted session ${chat.backendSessionId}: ${detail}`);
  }
}

async function updateBackendSessionArchiveIfNeeded(
  chat: ChatRecord,
  archived: boolean,
): Promise<void> {
  if (!chat.backendSessionId) {
    return;
  }

  const config = await getRuntimeConfig();
  if (config.mode !== 'live' || !config.apiUrl) {
    throw new Error('Cannot archive persisted session: runtime API is not configured.');
  }
  if (!config.accessToken) {
    throw new Error('Cannot archive persisted session: runtime authentication is missing.');
  }

  const response = await fetch(
    new URL(`/sessions/${encodeURIComponent(chat.backendSessionId)}`, config.apiUrl).toString(),
    {
      method: 'PUT',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${config.accessToken}`,
      },
      body: JSON.stringify({ status: archived ? 'archived' : 'active' }),
      cache: 'no-store',
    },
  );

  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;
    try {
      const body = (await response.json()) as { detail?: string; error?: string };
      detail = body.detail ?? body.error ?? detail;
    } catch {
      // Preserve the HTTP status.
    }
    throw new Error(`Cannot update persisted session ${chat.backendSessionId}: ${detail}`);
  }
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

function touchProject(projectId: string) {
  const project = getStore().projects.find((item) => item.id === projectId);
  if (project) {
    project.updatedAt = nowIso();
  }
}

function appendAssistantMessage(chat: ChatRecord, text: string, ok: boolean): ChatMessage {
  const timestamp = nowIso();
  const message: ChatMessage = {
    id: crypto.randomUUID(),
    role: 'assistant',
    content: text,
    createdAt: timestamp,
    status: ok ? 'complete' : 'failed',
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
  const timeout = setTimeout(() => controller.abort(), AGENT_RESPONSE_TIMEOUT_MS);

  try {
    const config = await getRuntimeConfig();
    if (config.mode !== 'live' || !config.apiUrl) {
      throw new Error(config.message);
    }
    const model = await resolveBackendModelName(config, params.model);
    const activeSkills = normalizedActiveSkills(params.activeSkills);

    const response = await fetch(new URL('/chat', config.apiUrl).toString(), {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        ...(config.accessToken ? { Authorization: `Bearer ${config.accessToken}` } : {}),
      },
      body: JSON.stringify({
        message: params.text,
        session_id: params.sessionId,
        model,
        allow_skills: activeSkills.length ? activeSkills : undefined,
        context: {
          source: 'web_v1',
          edge_profile: activeSkills.length ? { active_skills: activeSkills } : undefined,
        },
      }),
      cache: 'no-store',
      signal: controller.signal,
    });

    if (!response.ok) {
      let detail = `${response.status} ${response.statusText}`;
      try {
        const body = (await response.json()) as { detail?: string; error?: string };
        detail = body.detail ?? body.error ?? detail;
      } catch {
        // Preserve status text when the server returns a non-JSON error body.
      }
      throw new Error(detail);
    }

    const parsed = (await response.json()) as BackendChatResponse;
    if (parsed.run_id) {
      const streamed = await readRunStream(config, parsed.run_id);
      if (streamed.error) {
        throw new Error(streamed.error);
      }
      if (streamed.assistantText.trim()) {
        return {
          ok: true,
          sessionId: parsed.session_id,
          assistantText: streamed.assistantText.trim(),
        };
      }
    }

    return {
      ok: true,
      sessionId: parsed.session_id,
      assistantText:
        parsed.run_id
          ? 'Astra completed the run without returning visible text.'
          : 'The run was accepted by Astra.',
    };
  } catch (error) {
    const message =
      error instanceof Error && error.name === 'AbortError'
        ? `timed out after ${AGENT_RESPONSE_TIMEOUT_MS / 1000}s`
        : error instanceof Error
          ? error.message
          : 'unknown error';
    return {
      ok: false,
      assistantText: `I could not reach the Astra runtime from the web UI. The message was saved locally for this preview. (${message})`,
    };
  } finally {
    clearTimeout(timeout);
  }
}

async function readRunStream(config: RuntimeConfig, runId: string): Promise<StreamResult> {
  if (!config.apiUrl || !config.accessToken) {
    return { assistantText: '', error: 'Missing runtime authentication' };
  }

  const startedAt = Date.now();
  let nextOffset = 0;
  let assistantText = '';

  while (Date.now() - startedAt < AGENT_STREAM_TIMEOUT_MS) {
    const controller = new AbortController();
    const remainingMs = Math.max(1, AGENT_STREAM_TIMEOUT_MS - (Date.now() - startedAt));
    const timeout = setTimeout(() => controller.abort(), remainingMs);

    try {
      const response = await fetch(
        new URL(
          `/chat/runs/${encodeURIComponent(runId)}/stream?last_index=${nextOffset}`,
          config.apiUrl,
        ).toString(),
        {
          headers: {
            Authorization: `Bearer ${config.accessToken}`,
          },
          cache: 'no-store',
          signal: controller.signal,
        },
      );

      if (!response.ok) {
        let detail = `${response.status} ${response.statusText}`;
        try {
          const body = (await response.json()) as { detail?: string; error?: string };
          detail = body.detail ?? body.error ?? detail;
        } catch {
          // Preserve status text when body is not JSON.
        }
        return { assistantText, error: detail };
      }

      const parsed = parseRunSseText(await response.text());
      if (parsed.assistantText) {
        assistantText = parsed.assistantText;
      }
      if (typeof parsed.nextOffset === 'number' && parsed.nextOffset > nextOffset) {
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
        error instanceof Error && error.name === 'AbortError'
          ? `timed out after ${AGENT_STREAM_TIMEOUT_MS / 1000}s while waiting for Astra`
          : error instanceof Error
            ? error.message
            : 'unknown stream error';
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
  let assistantText = '';
  let finalText = '';
  let error: string | undefined;
  let finished = false;
  let nextOffset = 0;

  const dataLines = text
    .split(/\r?\n/)
    .filter((line) => line.startsWith('data:'))
    .map((line) => line.slice(5).trim())
    .filter((line) => line && line !== '[DONE]');

  for (const line of dataLines) {
    try {
      const event = JSON.parse(line) as Record<string, unknown>;
      const type = typeof event.type === 'string' ? event.type : '';
      if (typeof event.index === 'number' && Number.isFinite(event.index)) {
        nextOffset = Math.max(nextOffset, Math.trunc(event.index) + 1);
      }

      if (type === 'text_delta' && typeof event.content === 'string') {
        assistantText += event.content;
      } else if (type === 'text_done' && typeof event.full_text === 'string') {
        finalText = event.full_text;
      } else if (type === 'turn_complete' && typeof event.assistant_text === 'string') {
        finalText = event.assistant_text;
      } else if (type === 'error' && typeof event.message === 'string') {
        error = event.message;
      } else if (
        type === 'run_finished' &&
        event.status === 'failed' &&
        typeof event.error === 'string'
      ) {
        error = event.error;
        finished = true;
      } else if (type === 'run_finished') {
        finished = true;
      }
    } catch {
      // Ignore malformed SSE frames.
    }
  }

  return { assistantText: finalText || assistantText, error, finished, nextOffset };
}

export async function resolveBackendModelName(config: RuntimeConfig, model?: string) {
  if (!model || !config.apiUrl || !config.accessToken) {
    return model;
  }

  try {
    const response = await fetch(new URL('/models', config.apiUrl).toString(), {
      headers: {
        Authorization: `Bearer ${config.accessToken}`,
      },
      cache: 'no-store',
    });
    if (!response.ok) {
      return model;
    }

    const models = (await response.json()) as BackendModelListItem[];
    const matched = models.find((item) => item.model_id === model || item.name === model);
    return matched?.name ?? model;
  } catch {
    return model;
  }
}
