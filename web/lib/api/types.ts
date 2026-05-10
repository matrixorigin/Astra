export type PlanTier = 'free' | 'pro' | 'team';
export type Visibility = 'private' | 'team' | 'public';
export type MessageRole = 'user' | 'assistant' | 'system';
export type MessageStatus = 'pending' | 'streaming' | 'complete' | 'failed';
export type FileIndexStatus = 'pending' | 'extracting' | 'chunking' | 'embedding' | 'indexed' | 'failed';

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

export type ComposerOptions = {
  webSearch: boolean;
  thinking: boolean;
  model: string;
  activeSkills?: string[];
  style?: string;
};

export type ChatSummary = {
  id: string;
  title: string | null;
  lastMessageAt: string;
  lastMessagePreview?: string;
  projectId: string | null;
  archivedAt?: string | null;
};

export type ChatMessage = {
  id: string;
  role: MessageRole;
  content: string;
  reasoning?: string;
  reasoningStatus?: 'streaming' | 'complete';
  attachments?: AttachmentRef[];
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
  };
  messages: ChatMessage[];
  project?: { id: string; name: string };
  pendingTurn?: {
    messageId: string;
    content: string;
    options: ComposerOptions;
  };
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
};

export type SendMessageResponse = {
  userMessage: ChatMessage;
  assistantMessage: ChatMessage;
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
  chats: Array<{ id: string; title: string | null; projectId: string | null; updatedAt: string }>;
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
