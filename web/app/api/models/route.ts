import { NextResponse } from "next/server";
import type { RuntimeModelListItem } from "@astra/sdk";
import type { ModelSummary } from "@/lib/api/types";
import { listModelSummaries } from "@/lib/api/web-store";
import { requireRuntimeClient } from "@/lib/runtime-client";

export const dynamic = "force-dynamic";

function formatTokens(tokens?: number) {
  if (!tokens || tokens <= 0) {
    return null;
  }
  if (tokens >= 1000) {
    return `${Math.round(tokens / 1000)}k context`;
  }
  return `${tokens} context`;
}

function formatThinking(value: RuntimeModelListItem["thinking_capability"]) {
  if (!value) {
    return null;
  }
  if (typeof value === "string") {
    return value;
  }
  const kind = value.kind;
  return typeof kind === "string" ? kind : "thinking";
}

function toModelSummary(model: RuntimeModelListItem): ModelSummary | null {
  const id = model.model_id ?? model.name;
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
    subtitle: parts.length > 0 ? parts.join(" · ") : "Imported Astra model",
    tier: "included",
  };
}

export async function GET() {
  let runtime: Awaited<ReturnType<typeof requireRuntimeClient>>;
  try {
    runtime = await requireRuntimeClient({
      auth: "optional",
      operation: "list runtime models",
    });
  } catch {
    return NextResponse.json({
      items: listModelSummaries(),
      source: "fallback",
    });
  }

  const authenticated = Boolean(runtime.config.accessToken);
  try {
    const items = (await runtime.sdk.listModels())
      .filter((model) => model.is_active !== false)
      .map(toModelSummary)
      .filter((model): model is ModelSummary => model !== null);

    if (items.length > 0) {
      return NextResponse.json({ items, source: "astra" });
    }

    if (authenticated) {
      return NextResponse.json(
        {
          error: "runtime_models_unavailable",
          detail:
            "Runtime returned no active models for the authenticated user.",
        },
        { status: 502 },
      );
    }
  } catch (error) {
    if (authenticated) {
      return NextResponse.json(
        {
          error: "runtime_models_unavailable",
          detail:
            error instanceof Error
              ? error.message
              : "Failed to list runtime models.",
        },
        { status: 502 },
      );
    }
  }

  return NextResponse.json({ items: listModelSummaries(), source: "fallback" });
}
