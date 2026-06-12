export class WebApiError extends Error {
  readonly status: number;
  readonly detail: string;
  readonly code?: string;

  constructor(status: number, detail: string, code?: string) {
    super(detail);
    this.name = "WebApiError";
    this.status = status;
    this.detail = detail;
    this.code = code;
  }
}

export function isAuthRequiredError(error: unknown) {
  return error instanceof Error && error.message === "AUTH_REQUIRED";
}

export function isNotFoundError(error: unknown) {
  return error instanceof WebApiError && error.status === 404;
}
