import type {
  ExplainAnalyzeEventV1,
  ArtifactPublicationV1,
  ReflectReport,
  RuntimeSkillListCursor,
  RuntimeTranscriptItemResponse,
  SessionAuditSummary,
} from '@astra/sdk';
import type { WorkSurfaceEvent } from '@/lib/work-surface';

export type PlanTier = 'free' | 'pro' | 'team';
export type Visibility = 'private' | 'team' | 'public';
export type MessageRole = 'user' | 'assistant' | 'system';
export type MessageStatus = 'pending' | 'streaming' | 'complete' | 'failed';
export type FileIndexStatus =
  | 'pending'
  | 'extracting'
  | 'chunking'
  | 'embedding'
  | 'indexed'
  | 'failed';

export type UserSummary = {
  id: string;
  name: string;
  plan: PlanTier;
};

export type RecentItem = {
  kind: 'chat' | 'project';
  id: string;
  title: string;
  href: string;
  updatedAt: string;
};

export type RecentProjectGroup = {
  project: RecentItem;
  chats: RecentItem[];
  updatedAt: string;
};

export type SidebarData = {
  recents: RecentItem[];
  recentProjectGroups: RecentProjectGroup[];
  recentOtherChats: RecentItem[];
  untitled: RecentItem[];
  archivedChats: RecentItem[];
  user: UserSummary;
};

export type AttachmentRef = {
  id: string;
  filename: string;
  sizeBytes?: number;
  mimeType?: string;
  url?: string;
};

export type ChatArtifactRef = {
  id: string;
  kind: string;
  source?: string | null;
  title?: string | null;
  filename?: string | null;
  sizeBytes?: number | null;
  contentType?: string | null;
  renderer?: string | null;
  downloadFilename?: string | null;
  downloadUrl?: string | null;
  content?: unknown;
  createdAt?: string | null;
};

export type ComposerOptions = {
  webSearch: boolean;
  thinking: boolean;
  model: string;
  activeSkills?: string[];
  /** Explicit runtime tools selected through product capability surfaces. */
  activeTools?: string[];
  style?: string;
};

export type WorkspaceAuthority = 'read_only' | 'read_write';

export type WorkspaceSelection =
  | {
      kind: 'server_sandbox';
      authority?: WorkspaceAuthority;
    }
  | {
      kind: 'edge_workspace';
      edgeAgentId: string;
      displayName?: string | null;
      cwd: string;
      authority?: WorkspaceAuthority;
    };

export type EdgeStatusResponse = {
  edges: Array<{
    edge_agent_id: string;
    hostname?: string | null;
    workspace_dir?: string | null;
    capabilities?: unknown;
    connected_secs: number;
  }>;
};

export type RuntimeCapabilityProvider = {
  provider_id: string;
  kind: 'server' | 'edge';
  display_name: string;
  status: 'ready';
};

export type RuntimeCapabilitiesResponse = {
  tools: Array<{
    name: string;
    providers: RuntimeCapabilityProvider[];
  }>;
};

export type ChatSummary = {
  id: string;
  title: string | null;
  lastMessageAt: string;
  lastMessagePreview?: string;
  projectId: string | null;
  archivedAt?: string | null;
  model?: string | null;
};

export type ChatMessage = {
  id: string;
  role: MessageRole;
  content: string;
  activeSkills?: string[];
  activeTools?: string[];
  reasoning?: string;
  reasoningStatus?: 'streaming' | 'complete';
  attachments?: AttachmentRef[];
  artifacts?: ChatArtifactRef[];
  artifactPublication?: ArtifactPublicationV1;
  explainAnalyzeEvents?: ExplainAnalyzeEventV1[];
  explainAnalyzeDegraded?: boolean;
  explainAnalyzeUnrecoverable?: boolean;
  explainAnalyzeRepairToken?: string;
  createdAt: string;
  completedAt?: string | null;
  status?: MessageStatus;
};

export type ChatDetail = {
  chat: {
    id: string;
    title: string | null;
    projectId: string | null;
    createdAt: string;
    updatedAt: string;
    archivedAt?: string | null;
    model?: string | null;
  };
  session?: {
    chatId: string;
    backendSessionId?: string | null;
    persisted: boolean;
    messageCount: number;
  };
  messages: ChatMessage[];
  project?: { id: string; name: string };
  activeRun?: {
    runId: string;
    status: string;
    waitingFor?: string | null;
    assistantMessageId?: string | null;
    nextEventIndex?: number | null;
  };
  pendingTurn?: {
    messageId: string;
    content: string;
    options: ComposerOptions;
  };
  workspaceSelection?: WorkspaceSelection;
  workspaceSelectionExplicit?: boolean;
};

export type WorkSurfaceRunResponse = {
  runId: string;
  sessionId: string | null;
  status?: string | null;
  workspace?: Record<string, unknown> | null;
  executor?: Record<string, unknown> | null;
  transport?: string | null;
  fallbackPolicy?: string | null;
  events: WorkSurfaceEvent[];
  transcript?: RuntimeTranscriptItemResponse[];
  transcriptComplete?: boolean;
  transcriptWarning?: string | null;
  generatedAt: string;
};

export type ChatInsightsResponse = {
  sessionId: string;
  audit: SessionAuditSummary | null;
  reflection: ReflectReport | null;
  decisionTrace: ReflectReport | null;
  warnings: string[];
  generatedAt: string;
};

export type ChatListResponse = {
  items: ChatSummary[];
  nextCursor: string | null;
};

export type CreateChatRequest = {
  message: string;
  attachments?: AttachmentRef[];
  model: string;
  options: Omit<ComposerOptions, 'model'>;
  projectId?: string | null;
  workspaceSelection?: WorkspaceSelection | null;
};

export type CreateChatResponse = {
  chatId: string;
  messageId: string;
};

export type SendMessageRequest = {
  content: string;
  attachments?: AttachmentRef[];
  options?: ComposerOptions;
  pendingMessageId?: string;
  workspace?: WorkspaceSelection;
};

export type SendMessageResponse = {
  userMessage: ChatMessage;
  assistantMessage: ChatMessage;
};

export type QueueRunInputResponse = {
  userMessage: ChatMessage;
  assistantMessage: ChatMessage;
  activeRun: {
    runId: string;
    status: string;
    waitingFor?: string | null;
    assistantMessageId?: string | null;
    nextEventIndex?: number | null;
  };
};

export type ActiveRunMutationResponse = {
  activeRun?: {
    runId: string;
    status: string;
    waitingFor?: string | null;
    assistantMessageId?: string | null;
    nextEventIndex?: number | null;
  };
  cancelPending?: boolean;
};

export type ProjectSummary = {
  id: string;
  name: string;
  description: string | null;
  updatedAt: string;
  starred: boolean;
  visibility: Visibility;
};

export type KnowledgeFile = {
  id: string;
  filename: string;
  mimeType: string;
  sizeBytes: number;
  sourceType: 'upload' | 'text' | 'github';
  indexStatus: FileIndexStatus;
  indexedAt: string | null;
  createdAt: string;
};

export type ProjectDetail = {
  project: ProjectSummary & {
    instructions: string | null;
    memory: string | null;
    createdAt: string;
  };
  chats: ChatSummary[];
  files: KnowledgeFile[];
};

export type ProjectListResponse = {
  items: ProjectSummary[];
  nextCursor: string | null;
};

export type CreateProjectRequest = {
  name: string;
  description?: string | null;
  instructions?: string | null;
};

export type SearchResponse = {
  projects: Array<{ id: string; name: string; updatedAt: string }>;
  chats: Array<{
    id: string;
    title: string | null;
    projectId: string | null;
    updatedAt: string;
  }>;
};

export type ModelSummary = {
  id: string;
  name: string;
  subtitle: string;
  tier: 'included' | 'upgrade';
  accessLabel: string;
  executionPlacement: 'server' | 'edge';
};

export type SkillSummary = {
  id: string;
  name: string;
  version: string;
  description: string | null;
  source: string | null;
  category: string | null;
  status: string | null;
};

export type SkillListResponse = {
  items: SkillSummary[];
  total: number;
  limit: number;
  nextCursor: RuntimeSkillListCursor | null;
};
