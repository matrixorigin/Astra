import { NextResponse } from "next/server";
import type { RuntimeModelListItem } from "@astra/sdk";
import type { ModelSummary } from "@/lib/api/types";
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
  return value;
}

function toModelSummary(model: RuntimeModelListItem): ModelSummary | null {
  const id = model.offering_id;
  const name = model.name.trim();
  if (!id || !name) {
    return null;
  }

  const parts = [
    model.access_label,
    model.execution_placement === "edge" ? "Runs on this device" : "Runs on server",
    model.description,
    typeof model.architecture === "string" ? model.architecture : null,
    formatTokens(model.context_window),
    formatThinking(model.thinking_capability),
  ].filter((part): part is string => Boolean(part));

  return {
    id,
    name,
    subtitle: parts.join(" · "),
    tier: "included",
    accessLabel: model.access_label,
    executionPlacement: model.execution_placement,
  };
}

export async function GET() {
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "list runtime models",
    });
    const projection = await runtime.sdk.getModelAccess();
    const items = projection.offerings
      .filter((model) => model.is_active)
      .map(toModelSummary)
      .filter((model): model is ModelSummary => model !== null);
    const defaultOfferingId = projection.default_offering_id;
    const defaultResolution = projection.default_resolution ?? null;
    if (
      (items.length === 0 && defaultOfferingId !== null) ||
      (items.length > 0 &&
        defaultResolution?.state !== "invalid" &&
        (!defaultOfferingId ||
          !items.some((model) => model.id === defaultOfferingId)))
    ) {
      throw new Error(
        "Model Access returned a default outside the effective Offering catalog.",
      );
    }

    return NextResponse.json({
      items,
      accesses: projection.accesses,
      defaultOfferingId,
      defaultResolution,
      catalogRevision: projection.catalog_revision,
      observedAt: projection.observed_at,
      source: "astra",
    });
  } catch (error) {
    return NextResponse.json(
      {
        error: "model_access_unavailable",
        detail:
          error instanceof Error
            ? error.message
            : "Failed to load Model Access.",
        action: "sign_in_or_retry",
      },
      { status: 503 },
    );
  }
}
