import { apiFetch, getWebDataMode } from '@/lib/api/client';
import type { MemoryIntrospectionData, SkillsIntrospectionData } from '@/lib/models/platform';

type ApiMemoryIntrospectionResponse = {
  episodic: {
    turns: number;
    total_events: number;
    tool_intensity: string;
    session_depth: string;
  };
  semantic: {
    ctx_snapshots: number;
    peak_tokens: number;
    context_managed_tokens: number | null;
    last_assembly_ms: number | null;
    llm_prompt_tokens: number | null;
    llm_completion_tokens: number | null;
    llm_total_tokens: number | null;
    health: Record<string, unknown> | null;
  };
  procedural: {
    skill_selections: number;
    accuracy_rate: number | null;
  };
  profile: string[] | null;
};

function normalizeMemoryIntrospection(
  raw: ApiMemoryIntrospectionResponse,
): MemoryIntrospectionData {
  return {
    episodic: {
      turns: raw.episodic.turns,
      totalEvents: raw.episodic.total_events,
      toolIntensity: raw.episodic.tool_intensity,
      sessionDepth: raw.episodic.session_depth,
    },
    semantic: {
      ctxSnapshots: raw.semantic.ctx_snapshots,
      peakTokens: raw.semantic.peak_tokens,
      contextManagedTokens: raw.semantic.context_managed_tokens,
      lastAssemblyMs: raw.semantic.last_assembly_ms,
      llmPromptTokens: raw.semantic.llm_prompt_tokens,
      llmCompletionTokens: raw.semantic.llm_completion_tokens,
      llmTotalTokens: raw.semantic.llm_total_tokens,
      health: raw.semantic.health,
    },
    procedural: {
      skillSelections: raw.procedural.skill_selections,
      accuracyRate: raw.procedural.accuracy_rate,
    },
    profile: raw.profile,
  };
}

export async function getMemoryIntrospection(
  sessionId: string,
): Promise<MemoryIntrospectionData | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    const raw = await apiFetch<ApiMemoryIntrospectionResponse>(
      `/introspection/memory?session_id=${sessionId}`,
    );
    return normalizeMemoryIntrospection(raw);
  } catch {
    return null;
  }
}

export async function getSkillsIntrospection(): Promise<SkillsIntrospectionData | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    return await apiFetch<SkillsIntrospectionData>('/introspection/skills');
  } catch {
    return null;
  }
}
