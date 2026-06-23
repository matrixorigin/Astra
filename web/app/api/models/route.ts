import { NextResponse } from 'next/server';
import type { RuntimeModelListItem } from '@astra/sdk';
import type { ModelSummary } from '@/lib/api/types';
import { listModelSummaries } from '@/lib/api/web-store';
import { requireRuntimeClient } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

function formatTokens(tokens?: number) {
  if (!tokens || tokens <= 0) {
    return null;
  }
  if (tokens >= 1000) {
    return `${Math.round(tokens / 1000)}k context`;
  }
  return `${tokens} context`;
}

function formatThinking(value: RuntimeModelListItem['thinking_capability']) {
  if (!value) {
    return null;
  }
  if (typeof value === 'string') {
    return value;
  }
  const kind = value.kind;
  return typeof kind === 'string' ? kind : 'thinking';
}

function toModelSummary(model: RuntimeModelListItem): ModelSummary | null {
  const name = model.name?.trim();
  if (!name) {
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
    id: name,
    name,
    subtitle: parts.length > 0 ? parts.join(' · ') : 'Imported Astra model',
    tier: 'included',
  };
}

export async function GET() {
  try {
    const runtime = await requireRuntimeClient({
      auth: 'optional',
      operation: 'list runtime models',
    });
    const items = (await runtime.sdk.listModels())
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
