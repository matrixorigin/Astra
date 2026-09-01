import { AstraApiError } from "@astra/sdk";
import { notFound } from "next/navigation";
import { WorkOverviewPage } from "@/components/app/work-overview-page";
import { requireRuntimeClient } from "@/lib/runtime-client";
import {
  getWorkBranchPresentation,
  RequestedWorkBranchNotFound,
} from "@/lib/work-overview";

export default async function WorkPage({
  params,
  searchParams,
}: {
  params: Promise<{ workId: string }>;
  searchParams?: Promise<{ branch?: string }>;
}) {
  const { workId } = await params;
  const requestedBranchId = (await searchParams)?.branch;
  const runtime = await requireRuntimeClient({
    auth: "required",
    operation: "open Work",
  });

  try {
    const { snapshot: initial, catalog, selectedBranch } =
      await getWorkBranchPresentation(runtime.sdk, workId, requestedBranchId);
    const branchId = selectedBranch.branch_id;
    const deliveryBranch = catalog.branches.find((branch) => branch.is_delivery)!;
    const [
      attachmentResult,
      transcriptResult,
      archivedBranchesResult,
      patchArtifactsResult,
      patchMaterializationsResult,
      patchCommitsResult,
    ] =
      await Promise.allSettled([
      runtime.sdk.attachWorkBranch(workId, branchId, {
        requestId: `web-open:${crypto.randomUUID()}`,
      }),
      runtime.sdk.getWorkBranchTranscript(workId, branchId, { limit: 50 }),
      runtime.sdk.listArchivedWorkBranches(workId, { limit: 20 }),
      runtime.sdk.listWorkPatchArtifacts(workId, branchId, { limit: 10 }),
      selectedBranch.is_delivery
        ? Promise.resolve(undefined)
        : runtime.sdk.listWorkPatchMaterializations(workId, deliveryBranch.branch_id, {
            sourceBranchId: branchId,
            limit: 10,
          }),
      runtime.sdk.listWorkPatchCommits(workId, deliveryBranch.branch_id, { limit: 10 }),
    ]);
    const readOrThrow = <T,>(result: PromiseSettledResult<T>): T | null => {
      if (result.status === "fulfilled") return result.value;
      const error = result.reason;
      if (
        error instanceof AstraApiError &&
        (error.category === "availability" || error.category === "degraded")
      ) {
        return null;
      }
      throw error;
    };
    return (
      <WorkOverviewPage
        initial={initial}
        branchCatalog={catalog}
        selectedBranch={selectedBranch}
        attachment={readOrThrow(attachmentResult)}
        transcript={readOrThrow(transcriptResult)}
        archivedBranches={readOrThrow(archivedBranchesResult)}
        patchArtifacts={readOrThrow(patchArtifactsResult)}
        patchMaterializations={readOrThrow(patchMaterializationsResult)}
        patchCommits={readOrThrow(patchCommitsResult)}
      />
    );
  } catch (error) {
    if (error instanceof RequestedWorkBranchNotFound) {
      notFound();
    }
    if (error instanceof AstraApiError && error.status === 404) {
      notFound();
    }
    throw error;
  }
}
