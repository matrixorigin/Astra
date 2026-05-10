import { NextResponse } from 'next/server';
import { apiFetch } from '@/lib/api/client';
import type { ModelSummary } from '@/lib/api/types';
import { listModelSummaries } from '@/lib/api/web-store';

export const dynamic = 'force-dynamic';

type BackendModel = {
  model_id?: string;
  name?: string;
  provider?: string;
  description?: string | null;
  is_active?: boolean;
  context_window?: number;
  max_completion_tokens?: number | null;
  architecture?: string | null;
  thinking_capability?: string | Record<string, unknown> | null;
};

function getModelItems(payload: unknown): BackendModel[] {
  if (Array.isArray(payload)) {
    return payload.filter((item): item is BackendModel => Boolean(item) && typeof item === 'object');
  }

  if (!payload || typeof payload !== 'object') {
    return [];
  }

  const record = payload as Record<string, unknown>;
  const items = record.items ?? record.models;
  if (!Array.isArray(items)) {
    return [];
  }

  return items.filter((item): item is BackendModel => Boolean(item) && typeof item === 'object');
}

function formatTokens(tokens?: number) {
  if (!tokens || tokens <= 0) {
    return null;
  }
  if (tokens >= 1000) {
    return `${Math.round(tokens / 1000)}k context`;
  }
  return `${tokens} context`;
}

function formatThinking(value: BackendModel['thinking_capability']) {
  if (!value) {
    return null;
  }
  if (typeof value === 'string') {
    return value;
  }
  const kind = value.kind;
  return typeof kind === 'string' ? kind : 'thinking';
}

function toModelSummary(model: BackendModel): ModelSummary | null {
  const id = model.name ?? model.model_id;
  if (!id) {
    return null;
  }

  const parts = [
    model.provider,
    model.description,
    model.architecture,
    formatTokens(model.context_window),
    formatThinking(model.thinking_capability),
  ].filter((part): part is string => Boolean(part));

  return {
    id,
    name: model.name ?? id,
    subtitle: parts.length > 0 ? parts.join(' · ') : 'Imported Astra model',
    tier: 'included',
  };
}

export async function GET() {
  try {
    const payload = await apiFetch<unknown>('/models');
    const items = getModelItems(payload)
      .filter((model) => model.is_active !== false)
      .map(toModelSummary)
      .filter((model): model is ModelSummary => model !== null);

    if (items.length > 0) {
      return NextResponse.json({ items, source: 'astra' });
    }
  } catch {
    // The web shell can still render before the user logs in or before the API is configured.
  }

  return NextResponse.json({ items: listModelSummaries(), source: 'fallback' });
}
