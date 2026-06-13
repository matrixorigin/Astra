import { runtimeErrorDetail } from "@/lib/runtime-client/errors";

export type RuntimeCancelSettlement =
  | { status: "completed" }
  | { status: "failed"; error: unknown };

export async function settleRuntimeCancel(
  cancelPromise: Promise<unknown>,
  timeoutMs?: number,
  onLateSettled?: (settled: RuntimeCancelSettlement) => void,
) {
  if (!timeoutMs || timeoutMs <= 0) {
    await cancelPromise;
    return false;
  }

  let timeoutId: ReturnType<typeof setTimeout> | undefined;
  const handledCancel = cancelPromise.then(
    () => ({ status: "completed" as const }),
    (error: unknown) => ({ status: "failed" as const, error }),
  );
  const timeout = new Promise<{ status: "pending" }>((resolve) => {
    timeoutId = setTimeout(() => resolve({ status: "pending" }), timeoutMs);
  });
  const result = await Promise.race([handledCancel, timeout]);
  if (timeoutId) {
    clearTimeout(timeoutId);
  }
  if (result.status === "failed") {
    throw result.error;
  }
  if (result.status === "pending") {
    void handledCancel.then((settled) => {
      if (settled.status === "failed") {
        console.warn(
          "Runtime cancel failed after Web stop response:",
          runtimeErrorDetail(settled.error),
        );
      }
      try {
        onLateSettled?.(settled);
      } catch (error) {
        console.error("Late cancel settlement callback error:", error);
      }
    });
    return true;
  }
  return false;
}
