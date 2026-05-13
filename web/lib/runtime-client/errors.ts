import { AstraApiError, readAstraErrorDetail } from '@astra/sdk';

export type RuntimeErrorContext = {
  operation: string;
  path: string;
  status?: number;
  detail: string;
  cause?: unknown;
};

export class RuntimeClientError extends Error {
  readonly operation: string;
  readonly path: string;
  readonly status?: number;
  readonly detail: string;
  override readonly cause?: unknown;

  constructor(context: RuntimeErrorContext) {
    super(context.detail, { cause: context.cause });
    this.name = 'RuntimeClientError';
    this.operation = context.operation;
    this.path = context.path;
    this.status = context.status;
    this.detail = context.detail;
    this.cause = context.cause;
  }
}

export const readRuntimeErrorDetail = readAstraErrorDetail;

export function runtimeErrorDetail(error: unknown, fallback = 'Astra runtime request failed.'): string {
  if (error instanceof RuntimeClientError) {
    return error.detail;
  }
  if (error instanceof AstraApiError) {
    return error.body || error.message;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return fallback;
}
