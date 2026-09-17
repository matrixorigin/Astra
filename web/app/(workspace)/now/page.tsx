import type { WorkCatalogCursorV1 } from "@astra/sdk";
import { WorkNowPage } from "@/components/app/work-now-page";
import { requireRuntimeClient } from "@/lib/runtime-client";

function scalar(value: string | string[] | undefined): string | undefined {
  return typeof value === "string" ? value : undefined;
}

export default async function NowPage({
  searchParams,
}: {
  searchParams: Promise<Record<string, string | string[] | undefined>>;
}) {
  const params = await searchParams;
  const createdAt = scalar(params.before_created_at);
  const workId = scalar(params.before_work_id);
  const cursor: WorkCatalogCursorV1 | undefined =
    createdAt && workId ? { created_at: createdAt, work_id: workId } : undefined;
  const runtime = await requireRuntimeClient({
    auth: "required",
    operation: "open Now",
  });
  const page = await runtime.sdk.listWorks({ cursor, limit: 20 });
  return <WorkNowPage page={page} isLatest={cursor === undefined} />;
}
