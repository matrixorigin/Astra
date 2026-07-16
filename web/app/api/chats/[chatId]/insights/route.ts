import { NextRequest, NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat } from "@/lib/api/web-store";
import type { ChatInsightsResponse } from "@/lib/api/types";
import { requireRuntimeClient, runtimeErrorDetail } from "@/lib/runtime-client";

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
    const payload: ChatInsightsResponse = {
      sessionId,
      audit: settledValue("audit", auditResult, warnings),
      reflection: settledValue("reflection", reflectionResult, warnings),
      decisionTrace: settledValue(
        "decision trace",
        decisionTraceResult,
        warnings,
      ),
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
