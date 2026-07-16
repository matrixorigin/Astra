import { NextRequest, NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat } from "@/lib/api/web-store";
import type { ChatInsightsResponse } from "@/lib/api/types";
import { requireRuntimeClient, runtimeErrorDetail } from "@/lib/runtime-client";
import type { ReflectReport } from "@astra/sdk";

type EvidenceKey = "audit" | "reflection" | "decision trace";

function settledValue<T>(
  key: EvidenceKey,
  result: PromiseSettledResult<T>,
  warnings: string[],
): T | null {
  if (result.status === "fulfilled") {
    return result.value;
  }
  warnings.push(`${key}: ${runtimeErrorDetail(result.reason)}`);
  return null;
}

function normalizeReflectReport(report: ReflectReport | null): ReflectReport | null {
  if (!report) {
    return null;
  }
  const untrusted = report as ReflectReport & {
    overview?: unknown;
    diagnoses?: unknown;
    insights?: unknown;
    recommendations?: unknown;
  };
  return {
    ...report,
    overview:
      untrusted.overview &&
      typeof untrusted.overview === "object" &&
      !Array.isArray(untrusted.overview)
        ? (untrusted.overview as Record<string, unknown>)
        : {},
    diagnoses: Array.isArray(untrusted.diagnoses) ? untrusted.diagnoses : [],
    insights: Array.isArray(untrusted.insights) ? untrusted.insights : [],
    recommendations: Array.isArray(untrusted.recommendations)
      ? untrusted.recommendations.filter(
          (recommendation): recommendation is string =>
            typeof recommendation === "string" && recommendation.trim().length > 0,
        )
      : [],
  };
}

export async function GET(
  _request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const { chatId } = await context.params;
  const chat = getChat(auth.user.user_id, chatId);
  if (!chat) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }
  const sessionId = chat.session?.backendSessionId?.trim();
  if (!sessionId) {
    return NextResponse.json(
      {
        error: "Insights become available after the first durable run starts.",
        code: "session_not_bound",
      },
      { status: 409 },
    );
  }

  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "load web agent insights",
    });
    const [auditResult, reflectionResult, decisionTraceResult] =
      await Promise.allSettled([
        runtime.sdk.getSessionAudit(sessionId),
        runtime.sdk.getSessionReflect(sessionId, {
          focus: "auto",
          last_n: 30,
          question:
            "What should the user know now about progress, risks, and the best next action?",
        }),
        runtime.sdk.getSessionDecisionTrace(sessionId, {
          focus: "tool_surface",
          last_n: 30,
          question:
            "Summarize the important tool and routing decisions with their evidence.",
        }),
      ]);
    const warnings: string[] = [];
    const reflection = normalizeReflectReport(
      settledValue("reflection", reflectionResult, warnings),
    );
    const decisionTrace = normalizeReflectReport(
      settledValue("decision trace", decisionTraceResult, warnings),
    );
    const payload: ChatInsightsResponse = {
      sessionId,
      audit: settledValue("audit", auditResult, warnings),
      reflection,
      decisionTrace,
      warnings,
      generatedAt: new Date().toISOString(),
    };
    return NextResponse.json(payload);
  } catch (error) {
    return NextResponse.json(
      {
        error: runtimeErrorDetail(error),
        code: "insights_unavailable",
      },
      { status: 502 },
    );
  }
}
