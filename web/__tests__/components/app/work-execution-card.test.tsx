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
  loadWorkExecutionAction,
  loadWorkExecutionTargetsAction,
  observeWorkBranchControlAction,
  observeWorkExecutionSwitchAction,
  retryWorkExecutionSwitchAction,
  switchWorkExecutionAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { WorkExecutionCard } from "@/components/app/work-execution-card";

const refreshMock = vi.fn();
const acquireControl = vi.mocked(acquireWorkBranchControlAction);
const loadExecution = vi.mocked(loadWorkExecutionAction);
const loadTargets = vi.mocked(loadWorkExecutionTargetsAction);
const observeControl = vi.mocked(observeWorkBranchControlAction);
const observeExecution = vi.mocked(observeWorkExecutionSwitchAction);
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

const controlSucceeded = {
  schema_version: 2 as const,
  operation_id: "control-1",
  work_id: "work-1",
  branch_id: "branch-1",
  attachment_id: "attachment-1",
  kind: "acquire_branch_control" as const,
  state: "succeeded" as const,
  outcome: "acquired" as const,
  branch_revision: 4,
  control_basis: { writer_epoch: 3, canonical_root_hash: null },
  created_at: "2026-08-01T00:00:00Z",
  completed_at: "2026-08-01T00:00:01Z",
};

const controlPending = {
  ...controlSucceeded,
  operation_id: "control-pending",
  state: "pending" as const,
  outcome: "pending" as const,
  control_basis: null,
  completed_at: null,
};

beforeEach(() => {
  vi.clearAllMocks();
  loadExecution.mockReset();
  loadExecution.mockResolvedValue({ ok: true, execution });
  observeControl.mockReset();
  observeExecution.mockReset();
  loadTargets.mockResolvedValue({ ok: true, page: targets });
  acquireControl.mockResolvedValue({
    ok: true,
    operation: controlSucceeded,
  });
  switchExecution.mockResolvedValue({ ok: true, operation: succeeded });
  retryExecution.mockResolvedValue({ ok: true, operation: succeeded });
});

test("hydrates a persisted failed move after a page reload", async () => {
  const failed = {
    ...succeeded,
    state: "failed" as const,
    failure_code: "edge_attestation_command_failed",
  };
  observeExecution.mockResolvedValue({ ok: true, operation: failed });
  render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={{ ...execution, operation_id: failed.operation_id, state: "needs_attention" }}
      attachment={{ ...attachment, mode: "controller" }}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );

  expect(await screen.findByRole("button", { name: "Retry move" })).toBeInTheDocument();
  expect(observeExecution).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    operationId: "switch-1",
  });
});

test("keeps an interrupted switching move recoverable after reload", async () => {
  const switching = { ...succeeded, state: "switching" as const };
  observeExecution
    .mockResolvedValueOnce({ ok: true, operation: switching })
    .mockResolvedValueOnce({ ok: true, operation: succeeded });
  render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={{ ...execution, operation_id: switching.operation_id, state: "switching" }}
      attachment={{ ...attachment, mode: "controller" }}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );

  expect(await screen.findByRole("button", { name: "Resume move" })).toBeInTheDocument();
  await waitFor(() => expect(observeExecution).toHaveBeenCalledTimes(2));
  expect(screen.queryByRole("button", { name: "Resume move" })).not.toBeInTheDocument();
});

test("acquires controller before retrying a persisted failed move from read-only attachment", async () => {
  const failed = {
    ...succeeded,
    state: "failed" as const,
    failure_code: "edge_attestation_command_failed",
  };
  observeExecution.mockResolvedValue({ ok: true, operation: failed });
  render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={{ ...execution, operation_id: failed.operation_id, state: "needs_attention" }}
      attachment={attachment}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );

  fireEvent.click(await screen.findByRole("button", { name: "Retry move" }));
  await waitFor(() => expect(acquireControl).toHaveBeenCalledTimes(1));
  await waitFor(() =>
    expect(retryExecution).toHaveBeenCalledWith({
      workId: "work-1",
      branchId: "branch-1",
      operationId: "switch-1",
      attachmentId: "attachment-1",
    }),
  );
});

test("waits for controller acquisition before resuming a switching move", async () => {
  const switching = { ...succeeded, state: "switching" as const };
  observeExecution.mockResolvedValue({ ok: true, operation: switching });
  acquireControl.mockResolvedValueOnce({ ok: true, operation: controlPending });
  observeControl.mockResolvedValueOnce({ ok: true, operation: controlSucceeded });
  render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={{ ...execution, operation_id: switching.operation_id, state: "switching" }}
      attachment={attachment}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );

  fireEvent.click(await screen.findByRole("button", { name: "Resume move" }));
  await waitFor(() => expect(observeControl).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    operationId: "control-pending",
  }));
  await waitFor(() => expect(retryExecution).toHaveBeenCalledTimes(1));
});

test("does not retry a recovered move when controller acquisition is denied", async () => {
  const failed = {
    ...succeeded,
    state: "failed" as const,
    failure_code: "edge_attestation_command_failed",
  };
  observeExecution.mockResolvedValue({ ok: true, operation: failed });
  acquireControl.mockResolvedValueOnce({
    ok: false,
    status: 409,
    code: "writer_conflict",
    retryable: false,
  });
  render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={{ ...execution, operation_id: failed.operation_id, state: "needs_attention" }}
      attachment={attachment}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );

  fireEvent.click(await screen.findByRole("button", { name: "Retry move" }));
  await waitFor(() => expect(acquireControl).toHaveBeenCalledTimes(1));
  expect(retryExecution).not.toHaveBeenCalled();
});

test("refreshes placement and generation after a switching move settles", async () => {
  const moved = {
    ...execution,
    generation: 5,
    executor_id: "edge-desktop",
    executor_name: "Desktop",
    operation_id: "switch-1",
  };
  loadExecution.mockResolvedValueOnce({ ok: true, execution: moved });
  const switching = { ...succeeded, state: "switching" as const };
  observeExecution
    .mockResolvedValueOnce({ ok: true, operation: switching })
    .mockResolvedValueOnce({ ok: true, operation: succeeded });

  render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={{ ...execution, operation_id: switching.operation_id, state: "switching" }}
      attachment={{ ...attachment, mode: "controller" }}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );

  await waitFor(() => expect(screen.getByText("Running on Edge · Desktop")).toBeInTheDocument());
  expect(screen.getByText("Durable generation 5")).toBeInTheDocument();
});

test("ignores a delayed refresh from a previous branch", async () => {
  let resolveOld: ((value: { ok: true; execution: typeof execution }) => void) | undefined;
  loadExecution.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        resolveOld = resolve as typeof resolveOld;
      }),
  );
  const { rerender } = render(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-1"
      initialExecution={execution}
      attachment={{ ...attachment, branch_id: "branch-1" }}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Refresh" }));

  const nextExecution = {
    ...execution,
    branch_id: "branch-2",
    executor_id: "edge-next",
    executor_name: "Next",
  };
  rerender(
    <WorkExecutionCard
      workId="work-1"
      branchId="branch-2"
      initialExecution={nextExecution}
      attachment={{ ...attachment, branch_id: "branch-2" }}
      branchRevision={4}
      controlBasis={attachment.control_basis}
    />,
  );
  resolveOld?.({ ok: true, execution: { ...execution, executor_id: "stale-old" } });
  await waitFor(() => expect(screen.getByText("Running on Edge · Next")).toBeInTheDocument());
  expect(screen.queryByText(/stale-old/)).not.toBeInTheDocument();
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
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "Move to another Edge" })).not.toBeDisabled(),
  );
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
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "Move to another Edge" })).not.toBeDisabled(),
  );
});

test("does not leave target loading stuck after a refresh supersedes its request", async () => {
  let resolveTargets: ((value: { ok: true; page: typeof targets }) => void) | undefined;
  loadTargets.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        resolveTargets = resolve as typeof resolveTargets;
      }),
  );
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
  await waitFor(() => expect(loadTargets).toHaveBeenCalledTimes(1));
  fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "Move to another Edge" })).not.toBeDisabled(),
  );

  resolveTargets?.({ ok: true, page: targets });
  fireEvent.click(screen.getByRole("button", { name: "Move to another Edge" }));
  await waitFor(() => expect(loadTargets).toHaveBeenCalledTimes(2));
  expect(await screen.findByText("Desktop")).toBeInTheDocument();
});

test("refreshes an empty target directory before reopening the picker", async () => {
  loadTargets
    .mockResolvedValueOnce({ ok: true, page: { ...targets, targets: [] } })
    .mockResolvedValueOnce({ ok: true, page: targets });
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
  expect(await screen.findByText(/No other connected Edge is available/)).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
  await waitFor(() => expect(loadExecution).toHaveBeenCalledTimes(1));
  fireEvent.click(screen.getByRole("button", { name: "Move to another Edge" }));
  expect(await screen.findByText("Desktop")).toBeInTheDocument();
  expect(loadTargets).toHaveBeenCalledTimes(2);
});
