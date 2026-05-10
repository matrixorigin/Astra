export function isAuthRequiredError(error: unknown) {
  return error instanceof Error && error.message === 'AUTH_REQUIRED';
}
