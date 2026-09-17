import { AstraApiError, type WorkBranchAttachmentV1 } from "@astra/sdk";
import { cookies, headers } from "next/headers";
import { notFound } from "next/navigation";
import { WorkOverviewPage } from "@/components/app/work-overview-page";
import { requireRuntimeClient } from "@/lib/runtime-client";
import {
  WEB_CLIENT_ID_COOKIE,
  WEB_CLIENT_ID_HEADER,
} from "@/lib/runtime-config";
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
  const requestHeaders = await headers();
  const cookieStore = await cookies();
  const forwardedClientId = requestHeaders.get(WEB_CLIENT_ID_HEADER);
  const cookieClientId = cookieStore.get(WEB_CLIENT_ID_COOKIE)?.value;
  const clientId =
    (forwardedClientId && /^[A-Za-z0-9._:-]{1,128}$/u.test(forwardedClientId)
      ? forwardedClientId
      : cookieClientId && /^[A-Za-z0-9._:-]{1,128}$/u.test(cookieClientId)
        ? cookieClientId
        : crypto.randomUUID());
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
      activityResult,
      executionResult,
      transcriptResult,
      archivedBranchesResult,
      patchArtifactsResult,
      patchMaterializationsResult,
      patchCommitsResult,
      recoveryPointsResult,
    ] =
      await Promise.allSettled([
      runtime.sdk.attachWorkBranch(workId, branchId, {
        // This page is refreshed as Work events arrive. Keep one logical
        // read attachment for this Work/branch so each refresh renews the
        // existing bounded slot instead of allocating another one.
        requestId: `web-open:${clientId}:${workId}:${branchId}`,
        clientId,
        surface: "web",
      }),
      runtime.sdk.getWorkBranchActivity(workId, branchId),
      runtime.sdk.getWorkBranchExecution(workId, branchId),
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
      runtime.sdk.listWorkBranchRecoveryPoints(workId, branchId, { limit: 10 }),
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
    const attachmentNotice =
      attachmentResult.status === "rejected"
        ? attachmentRequestNotice(attachmentResult.reason)
        : undefined;
    const attachmentRead =
      attachmentResult.status === "fulfilled"
        ? { value: attachmentResult.value, notice: undefined }
        : attachmentNotice
          ? { value: null, notice: attachmentNotice }
          : {
              value: readOrThrow<WorkBranchAttachmentV1>(attachmentResult),
              notice: undefined,
            };
    return (
      <WorkOverviewPage
        initial={initial}
        branchCatalog={catalog}
        selectedBranch={selectedBranch}
        attachment={attachmentRead.value}
        attachmentNotice={attachmentRead.notice}
        initialActivity={readOrThrow(activityResult)}
        initialExecution={readOrThrow(executionResult)}
        transcript={readOrThrow(transcriptResult)}
        archivedBranches={readOrThrow(archivedBranchesResult)}
        patchArtifacts={readOrThrow(patchArtifactsResult)}
        patchMaterializations={readOrThrow(patchMaterializationsResult)}
        patchCommits={readOrThrow(patchCommitsResult)}
        recoveryPoints={readOrThrow(recoveryPointsResult)}
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

function attachmentRequestNotice(error: unknown): string | undefined {
  if (!(error instanceof AstraApiError) || !error.path.endsWith("/attachments")) {
    return undefined;
  }
  if (
    error.status === 400 &&
    error.code === "invalid_work_attachment_request"
  ) {
    return "The Server rejected the read attachment request. This can happen when Web and Server builds use different Work contracts. Restart the Astra Server from this checkout, then refresh the page. Saved Work data remains available.";
  }
  if (error.status === 426 && error.code === "unsupported_client_version") {
    return "This Astra Server needs the matching Web Work contract. Restart the Server from this checkout, then refresh the page. Saved Work data remains available.";
  }
  return undefined;
}
