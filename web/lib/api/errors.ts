export class WebApiError extends Error {
  readonly status: number;
  readonly detail: string;

  constructor(status: number, detail: string) {
    super(detail);
    this.name = 'WebApiError';
    this.status = status;
    this.detail = detail;
  }
}

export function isAuthRequiredError(error: unknown) {
  return error instanceof Error && error.message === 'AUTH_REQUIRED';
}

export function isNotFoundError(error: unknown) {
  return error instanceof WebApiError && error.status === 404;
}
