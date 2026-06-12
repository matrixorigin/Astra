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
  content?: unknown;
  createdAt?: string | null;
};

export type ComposerOptions = {
  webSearch: boolean;
  thinking: boolean;
  model: string;
  activeSkills?: string[];
  style?: string;
};

export type WorkspaceSelection =
  | {
      kind: 'server_sandbox';
    }
  | {
      kind: 'edge_workspace';
      edgeAgentId: string;
      displayName?: string | null;
      cwd: string;
    };

export type EdgeStatusResponse = {
  edges: Array<{
    edge_agent_id: string;
    hostname?: string | null;
    workspace_dir?: string | null;
    connected_secs: number;
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
  reasoning?: string;
  reasoningStatus?: 'streaming' | 'complete';
  attachments?: AttachmentRef[];
  artifacts?: ChatArtifactRef[];
  createdAt: string;
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
  };
  pendingTurn?: {
    messageId: string;
    content: string;
    options: ComposerOptions;
  };
  workspaceSelection?: WorkspaceSelection;
  workspaceSelectionExplicit?: boolean;
};

export type WorkSurfaceResponse = {
  sessionId: string | null;
  runId: string | null;
  status?: string | null;
  workspace?: Record<string, unknown> | null;
  executor?: Record<string, unknown> | null;
  transport?: string | null;
  fallbackPolicy?: string | null;
  tasks: Array<Record<string, unknown>>;
  events: Array<Record<string, unknown>>;
  generatedAt: string;
  warnings?: string[];
};

export type WorkSurfaceRunResponse = {
  runId: string;
  sessionId: string | null;
  status?: string | null;
  workspace?: Record<string, unknown> | null;
  executor?: Record<string, unknown> | null;
  transport?: string | null;
  fallbackPolicy?: string | null;
  events: Array<Record<string, unknown>>;
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
  activeRun: {
    runId: string;
    status: string;
    waitingFor?: string | null;
  };
};

export type ActiveRunMutationResponse = {
  activeRun?: {
    runId: string;
    status: string;
    waitingFor?: string | null;
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
  offset: number;
  nextOffset: number | null;
};

export type HarnessTemplate = {
  template_id: string;
  name: string;
  description: string;
  built_in: boolean;
  input_schema_json: unknown;
  workflow_json: unknown;
};

export type HarnessNodeCatalogItem = {
  node_type: string;
  description: string;
  input_schema_json: unknown;
  output_schema_json: unknown;
};

export type HarnessRun = {
  harness_run_id: string;
  harness_id: string;
  version_id: string;
  user_id: string;
  session_id: string | null;
  status: string;
  input_json: Record<string, unknown>;
  output_json: Record<string, unknown>;
  error: string | null;
  created_at: string;
  updated_at: string;
};

export type HarnessItem = {
  item_id: string;
  harness_run_id: string;
  item_type: string;
  locator_json: Record<string, unknown>;
  input_json: Record<string, unknown>;
  proposed_output_json: Record<string, unknown>;
  final_output_json: Record<string, unknown>;
  status: string;
  confidence: number | null;
  assigned_to: string | null;
  created_at: string;
  updated_at: string;
};

export type HarnessCitation = {
  citation_id: string;
  harness_run_id: string;
  item_id: string;
  skill_draft_id: string | null;
  skill_rule_id: string | null;
  source_id: string | null;
  source_locator_json: Record<string, unknown>;
  artifact_id: string | null;
  quote_hash: string | null;
  evidence_text_preview: string | null;
  relevance_score: number | null;
  created_by_node_id: string | null;
  created_at: string;
};

export type HarnessSkillRule = {
  skill_rule_id: string;
  skill_draft_id: string;
  harness_run_id: string;
  rule_type: string;
  statement: string;
  rationale: string;
  status: string;
  confidence: number | null;
  source_count: number;
  created_by_node_id: string | null;
  created_at: string;
  updated_at: string;
  citations: HarnessCitation[];
};

export type HarnessSkillDraft = {
  skill_draft_id: string;
  harness_run_id: string;
  candidate_name: string;
  description: string;
  target_scope: string;
  publish_visibility: string;
  content_markdown: string;
  source_summary_json: Record<string, unknown>;
  status: string;
  confidence: number | null;
  created_by_node_id: string | null;
  revision: number;
  published_version_id: string | null;
  created_at: string;
  updated_at: string;
  rules: HarnessSkillRule[];
};

export type SkillifyRunRequest = {
  session_ids: string[];
  source_files?: Array<{
    file_name: string;
    mime_type?: string | null;
    content: string;
  }>;
  skill_name?: string | null;
  topic?: string | null;
  target_scope?: 'personal' | 'project';
};

export type HarnessDecisionRequest = {
  decision: 'approve' | 'reject' | 'edit' | 'request_revision';
  after_json?: Record<string, unknown>;
  reason?: string;
  idempotency_key?: string;
};

export type SkillifyDraftRequest = {
  skill_name?: string | null;
  version?: string | null;
  description?: string | null;
};

export type SkillifyDraft = {
  harness_run_id: string;
  skill_name: string;
  version_id: string;
  content_markdown: string;
  approved_item_count: number;
};

export type SkillifyPublishRequest = {
  visibility?: 'private' | 'public';
  version?: string | null;
  description?: string | null;
};

export type SkillifyPublishRecord = {
  harness_run_id: string;
  skill_draft_id: string;
  skill_name: string;
  version_id: string;
  visibility: string;
  content_markdown: string;
  approved_rule_count: number;
};
