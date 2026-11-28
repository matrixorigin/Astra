import { AstraApiError } from "@astra/sdk";
import { RuntimeClientError } from "@/lib/runtime-client";

export type WorkActionError = {
  ok: false;
  status: number;
  code: string | null;
  retryable: boolean;
};

export const WORK_OBSERVATION_TIMEOUT_MS = 15_000;

export function classifyWorkActionError(error: unknown): WorkActionError | null {
  if (error instanceof DOMException && error.name === "TimeoutError") {
    return { ok: false, status: 504, code: "work_observation_timeout", retryable: true };
  }
  if (error instanceof RuntimeClientError) {
    return {
      ok: false,
      status: error.status ?? 500,
      code: error.code ?? null,
      retryable: (error.status ?? 500) >= 500,
    };
  }
  if (error instanceof AstraApiError) {
    return {
      ok: false,
      status: error.status,
      code: error.code ?? null,
      retryable: error.retryable ?? error.status >= 500,
    };
  }
  return null;
}
