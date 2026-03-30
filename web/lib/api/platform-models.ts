import { apiFetch, tryApiFetch } from '@/lib/api/client';
import type { ModelDetail, ModelSummary } from '@/lib/models/platform';

type ApiModel = {
  model_id: string;
  name: string;
  provider: string;
  base_url: string;
  description: string | null;
  is_active: boolean;
  context_window: number | null;
  max_completion_tokens: number | null;
  input_modalities: string[];
  output_modalities: string[];
  supported_parameters: string[];
  pricing: { prompt: number; completion: number };
  architecture: string | null;
  tags: string[];
  quirks: {
    preserve_reasoning_content: boolean;
    no_parallel_tool_calls: boolean;
    tool_choice_required: boolean;
    strict_tool_call_ids: boolean;
    no_system_message: boolean;
    system_as_user_prefix: boolean;
  };
};

function normalizeModel(raw: ApiModel): ModelSummary {
  return {
    modelId: raw.model_id,
    name: raw.name,
    provider: raw.provider,
    baseUrl: raw.base_url,
    description: raw.description,
    isActive: raw.is_active,
    contextWindow: raw.context_window,
    maxCompletionTokens: raw.max_completion_tokens,
    inputModalities: raw.input_modalities ?? [],
    outputModalities: raw.output_modalities ?? [],
    supportedParameters: raw.supported_parameters ?? [],
    pricing: raw.pricing ?? { prompt: 0, completion: 0 },
    architecture: raw.architecture,
    tags: raw.tags ?? [],
    quirks: {
      preserveReasoningContent: raw.quirks?.preserve_reasoning_content ?? false,
      noParallelToolCalls: raw.quirks?.no_parallel_tool_calls ?? false,
      toolChoiceRequired: raw.quirks?.tool_choice_required ?? false,
      strictToolCallIds: raw.quirks?.strict_tool_call_ids ?? false,
      noSystemMessage: raw.quirks?.no_system_message ?? false,
      systemAsUserPrefix: raw.quirks?.system_as_user_prefix ?? false,
    },
  };
}

export async function getModels(): Promise<ModelSummary[]> {
  const response = await tryApiFetch<ApiModel[]>('/models');
  return response ? response.map(normalizeModel) : [];
}

export async function getModelDetail(name: string): Promise<ModelDetail | null> {
  try {
    const raw = await apiFetch<ApiModel>(`/models/${encodeURIComponent(name)}`);
    return normalizeModel(raw);
  } catch {
    return null;
  }
}
