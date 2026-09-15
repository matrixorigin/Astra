vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh: refreshMock }),
}));

vi.mock("@/app/(workspace)/works/[workId]/actions", () => ({
  acquireWorkBranchControlAction: vi.fn(),
  loadWorkExecutionAction: vi.fn(),
  loadWorkExecutionTargetsAction: vi.fn(),
  observeWorkBranchControlAction: vi.fn(),
  observeWorkExecutionSwitchAction: vi.fn(),
  retryWorkExecutionSwitchAction: vi.fn(),
  switchWorkExecutionAction: vi.fn(),
}));

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import {
  acquireWorkBranchControlAction,
  loadWorkExecutionTargetsAction,
  retryWorkExecutionSwitchAction,
  switchWorkExecutionAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { WorkExecutionCard } from "@/components/app/work-execution-card";

const refreshMock = vi.fn();
const acquireControl = vi.mocked(acquireWorkBranchControlAction);
const loadTargets = vi.mocked(loadWorkExecutionTargetsAction);
const switchExecution = vi.mocked(switchWorkExecutionAction);
const retryExecution = vi.mocked(retryWorkExecutionSwitchAction);

const execution = {
  schema_version: 1 as const,
  work_id: "work-1",
  branch_id: "branch-1",
  initialized: true,
  generation: 3,
  state: "ready" as const,
  placement: "edge" as const,
  executor_id: "edge-laptop",
  executor_name: "Laptop",
  operation_id: null,
  attempt: null,
  failure_code: null,
};

const attachment = {
  schema_version: 1 as const,
  work_id: "work-1",
  branch_id: "branch-1",
  attachment_id: "attachment-1",
  attachment_epoch: 1,
  branch_revision: 4,
  mode: "read_only" as const,
  sync: "current" as const,
  control_basis: { writer_epoch: 2, canonical_root_hash: null },
  head: null,
  attached_at: "2026-08-01T00:00:00Z",
  expires_at: "2026-08-01T01:00:00Z",
};

const targets = {
  schema_version: 1 as const,
  work_id: "work-1",
  branch_id: "branch-1",
  targets: [
    {
      executor_id: "edge-laptop",
      display_name: "Laptop",
      hostname: "laptop.local",
      capabilities: [],
      connected: true,
    },
    {
      executor_id: "edge-desktop",
      display_name: "Desktop",
      hostname: "desktop.local",
      capabilities: [],
      connected: true,
    },
  ],
};

const succeeded = {
  schema_version: 1 as const,
  work_id: "work-1",
  branch_id: "branch-1",
  operation_id: "switch-1",
  request_id: "request-1",
  state: "succeeded" as const,
  expected_generation: 3,
  switching_generation: 4,
  completed_generation: 5,
  attempt: 1,
  target: { kind: "edge" as const, executor_id: "edge-desktop" },
  failure_code: null,
};

beforeEach(() => {
  vi.clearAllMocks();
  loadTargets.mockResolvedValue({ ok: true, page: targets });
  acquireControl.mockResolvedValue({
    ok: true,
    operation: {
      schema_version: 2,
      operation_id: "control-1",
      work_id: "work-1",
      branch_id: "branch-1",
      attachment_id: "attachment-1",
      kind: "acquire_branch_control",
      state: "succeeded",
      outcome: "acquired",
      branch_revision: 4,
      control_basis: { writer_epoch: 3, canonical_root_hash: null },
      created_at: "2026-08-01T00:00:00Z",
      completed_at: "2026-08-01T00:00:01Z",
    },
  });
  switchExecution.mockResolvedValue({ ok: true, operation: succeeded });
  retryExecution.mockResolvedValue({ ok: true, operation: succeeded });
});

test("loads owner targets only when the user opens the move picker", async () => {
  render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={execution}
      attachment={attachment}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );

  expect(screen.getByText("Running on Edge · Laptop")).toBeInTheDocument();
  expect(loadTargets).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Move to another Edge" }));
  expect(await screen.findByText("Desktop")).toBeInTheDocument();
  expect(loadTargets).toHaveBeenCalledWith({ workId: "work-1", branchId: "branch-1" });
});

test("takes control explicitly before moving and uses the displayed generation", async () => {
  render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={execution}
      attachment={attachment}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Move to another Edge" }));
  fireEvent.click(await screen.findByRole("button", { name: /Desktop/i }));

  await waitFor(() =>
    expect(switchExecution).toHaveBeenCalledWith(
      expect.objectContaining({
        workId: "work-1",
        branchId: "branch-1",
        attachmentId: "attachment-1",
        expectedGeneration: 3,
        targetExecutorId: "edge-desktop",
      }),
    ),
  );
  expect(acquireControl).toHaveBeenCalledTimes(1);
  expect(refreshMock).toHaveBeenCalled();
});

test("keeps a failed durable move retryable without submitting a new target", async () => {
  switchExecution.mockResolvedValueOnce({
    ok: true,
    operation: { ...succeeded, state: "failed", completed_generation: 6, failure_code: "edge_attestation_command_failed" },
  });
  render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={execution}
      attachment={{ ...attachment, mode: "controller" }}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Move to another Edge" }));
  fireEvent.click(await screen.findByRole("button", { name: /Desktop/i }));
  expect(await screen.findByRole("button", { name: "Retry move" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Retry move" }));
  await waitFor(() => expect(retryExecution).toHaveBeenCalledWith(expect.objectContaining({
    operationId: "switch-1",
    attachmentId: "attachment-1",
  })));
});
