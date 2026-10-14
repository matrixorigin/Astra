const streamHarness = vi.hoisted(() => ({
  instances: [] as Array<{
    options: Record<string, any>;
    close: ReturnType<typeof vi.fn>;
  }>,
}));

vi.mock("@astra/sdk", async (importOriginal) => {
  const original = await importOriginal<typeof import("@astra/sdk")>();
  class TestSSEClient {
    options: Record<string, any>;
    close = vi.fn();

    constructor(options: Record<string, any>) {
      this.options = options;
      streamHarness.instances.push(this);
    }

    async connect() {
      this.options.onStateChange?.("connecting");
    }
  }
  return { ...original, SSEClient: TestSSEClient };
});

const refresh = vi.fn();
vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh }),
}));

const forceTakeover = vi.hoisted(() => vi.fn());
const observeControl = vi.hoisted(() => vi.fn());
const abortControl = vi.hoisted(() => vi.fn());
const reauthOptions = vi.hoisted(() => vi.fn());
const respondInteraction = vi.hoisted(() => vi.fn());
vi.mock("@/app/(workspace)/works/[workId]/actions", () => ({
  forceTakeoverWorkBranchAction: forceTakeover,
  observeWorkBranchControlAction: observeControl,
  abortWorkBranchControlAction: abortControl,
  getWorkReauthenticationOptionsAction: reauthOptions,
  respondWorkBranchInteractionAction: respondInteraction,
}));

import { StrictMode } from "react";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { WorkTurnComposer } from "@/components/app/work-turn-composer";

beforeEach(() => {
  vi.clearAllMocks();
  reauthOptions.mockResolvedValue({ method: "password" });
  respondInteraction.mockResolvedValue({
    ok: true,
    receipt: {
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-1",
      run_id: "run-1",
      request_id: "approval-1",
      outcome: "resolved",
    },
  });
  streamHarness.instances.length = 0;
  Object.defineProperty(globalThis.crypto, "randomUUID", {
    configurable: true,
    value: vi.fn(() => "00000000-0000-4000-8000-000000000011"),
  });
});

test("keeps continuation unavailable without a durable attachment", () => {
  render(<WorkTurnComposer workId="work-1" branchId="branch-1" />);

  expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Send guidance" })).toBeDisabled();
  expect(screen.getByPlaceholderText(/Reconnect to continue/i)).toBeInTheDocument();
  expect(streamHarness.instances).toHaveLength(0);
});

test("Memoria takeover requests fresh verification and submits no password", async () => {
  reauthOptions.mockResolvedValue({ method: "memoria", verification_url: "https://thememoria.ai/astra/reauthenticate" });
  forceTakeover.mockResolvedValue({ ok:false, status:403, code:"reauthentication_required", retryable:false });
  render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" branchRevision={3} controlBasis={{ writer_epoch:4, canonical_root_hash:"a".repeat(64) }} />);
  fireEvent.change(screen.getByRole("textbox", { name:"Guide this Work" }), {target:{value:"Continue"}});
  fireEvent.click(screen.getByRole("button", {name:"Send guidance"}));
  act(() => {
    streamHarness.instances[0]!.options.onEvent({ type:"error", code:"writer_conflict", message:"Active elsewhere", retryable:false, http_status:409, action_hints:["refresh_work"] });
    streamHarness.instances[0]!.options.onStateChange("disconnected");
  });
  fireEvent.click(screen.getByRole("button", {name:"Continue here"}));
  const input = await screen.findByLabelText("One-time verification proof");
  expect(screen.queryByLabelText("Password")).not.toBeInTheDocument();
  expect(screen.getByRole("link", {name:/Verify your identity/})).toHaveAttribute("href", "https://thememoria.ai/astra/reauthenticate?purpose=session_forced_takeover");
  const fresh = `msu_${"a".repeat(64)}`;
  fireEvent.change(input, {target:{value:fresh}});
  fireEvent.click(screen.getByRole("button", {name:"Confirm"}));
  await waitFor(() => expect(forceTakeover).toHaveBeenCalled());
  expect(forceTakeover.mock.calls[0][0]).toMatchObject({ memoriaProof:fresh });
  expect(forceTakeover.mock.calls[0][0]).not.toHaveProperty("password");
  expect(await screen.findByLabelText("One-time verification proof")).toHaveValue("");
});

test("submits one typed Work turn and applies only decoded visible text", async () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
    />,
  );

  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Inspect, plan, and begin." },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));

  expect(streamHarness.instances).toHaveLength(1);
  const instance = streamHarness.instances[0]!;
  expect(instance.options.url).toBe(
    "/api/works/work-1/branches/branch-1/turns",
  );
  expect(JSON.parse(instance.options.body)).toEqual({
    request_id: "web-work-turn:00000000-0000-4000-8000-000000000011",
    attachment_id: "attachment-1",
    message: "Inspect, plan, and begin.",
  });
  expect(screen.getByText("Inspect, plan, and begin.")).toBeInTheDocument();

  act(() => {
    instance.options.onEvent({
      type: "work_turn_started",
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-1",
      run_id: "run-1",
    });
    instance.options.onEvent({
      type: "context_meta",
      system_prompt_tokens: 42,
      context_manifest_trace: { source: "canonical" },
    });
    instance.options.onEvent({ type: "ping" });
    instance.options.onEvent({ type: "reasoning_delta", content: "private reasoning" });
    instance.options.onEvent({ type: "text_delta", content: "I started the work." });
    instance.options.onEvent({ type: "turn_complete" });
  });

  expect(screen.getByText("I started the work.")).toBeInTheDocument();
  expect(screen.queryByText("private reasoning")).not.toBeInTheDocument();
  expect(refresh).toHaveBeenCalledTimes(1);
});

test("renders an approval discovered after another surface started the Work", async () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [
          {
            schema_version: 1,
            work_id: "work-1",
            branch_id: "branch-1",
            run_id: "run-1",
            request_id: "approval-1",
            kind: "approval",
            tool: "bash",
            approval_kind: "standard",
            display_label: "Run the migration",
            detail: "Apply the pending schema change",
          },
        ],
      }}
    />,
  );

  expect(screen.getByText("Astra needs approval")).toBeInTheDocument();
  expect(screen.getByText("Run the migration")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenCalledTimes(1));
  expect(respondInteraction.mock.calls[0]?.[0]).toMatchObject({
    workId: "work-1",
    branchId: "branch-1",
    attachmentId: "attachment-1",
    runId: "run-1",
    requestId: "approval-1",
    response: { kind: "approval", decision: "allow" },
  });
  expect(refresh).toHaveBeenCalled();
});

test("keeps a pending Work question actionable after the TUI is gone", async () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [
          {
            schema_version: 1,
            work_id: "work-1",
            branch_id: "branch-1",
            run_id: "run-1",
            request_id: "prompt-1",
            kind: "user_prompt",
            prompt: {
              questions: [
                {
                  header: "Environment",
                  id: "placement",
                  question: "Where should this run?",
                  options: [{ label: "staging" }, { label: "production" }],
                  multi_select: false,
                },
              ],
            },
          },
        ],
      }}
    />,
  );
  fireEvent.change(screen.getByRole("combobox"), { target: { value: "staging" } });
  fireEvent.click(screen.getByRole("button", { name: "Submit answer" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenCalledTimes(1));
  expect(respondInteraction.mock.calls[0]?.[0]).toMatchObject({
    runId: "run-1",
    requestId: "prompt-1",
    response: {
      kind: "user_prompt",
      cancelled: false,
      answers: {
        answers: [
          {
            id: "placement",
            question: "Where should this run?",
            answers: ["staging"],
            multi_select: false,
            annotation: null,
          },
        ],
      },
    },
  });
});

test("preserves commas in a single freeform answer", async () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [
          {
            schema_version: 1,
            work_id: "work-1",
            branch_id: "branch-1",
            run_id: "run-1",
            request_id: "prompt-1",
            kind: "user_prompt",
            prompt: { questions: [{ question: "Describe the change" }] },
          },
        ],
      }}
    />,
  );
  fireEvent.change(screen.getByPlaceholderText("Type your answer"), {
    target: { value: "hello, world" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Submit answer" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenCalledTimes(1));
  expect(respondInteraction.mock.calls[0]?.[0].response.answers.answers[0].answers).toEqual([
    "hello, world",
  ]);
});

test("does not submit the Other sentinel and requires its freeform value", async () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [
          {
            schema_version: 1,
            work_id: "work-1",
            branch_id: "branch-1",
            run_id: "run-1",
            request_id: "prompt-1",
            kind: "user_prompt",
            prompt: {
              questions: [
                {
                  question: "Which environments?",
                  options: ["staging", "production"],
                  multi_select: true,
                  allow_freeform: true,
                },
              ],
            },
          },
        ],
      }}
    />,
  );

  const checkboxes = screen.getAllByRole("checkbox");
  fireEvent.click(checkboxes[0]!);
  fireEvent.click(checkboxes[2]!);
  fireEvent.click(screen.getByRole("button", { name: "Submit answer" }));
  expect(respondInteraction).not.toHaveBeenCalled();
  expect(screen.getByRole("alert")).toHaveTextContent(/Enter an answer for Other/i);

  fireEvent.change(screen.getByPlaceholderText("Type your answer"), {
    target: { value: "preview, canary" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Submit answer" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenCalledTimes(1));
  expect(respondInteraction.mock.calls[0]?.[0].response.answers.answers[0].answers).toEqual([
    "staging",
    "preview",
    "canary",
  ]);
  expect(respondInteraction.mock.calls[0]?.[0].response.answers.answers[0].answers).not.toContain(
    "__astra_other__",
  );
});

test("isolates prompt answers when the observed request changes", () => {
  const request = (requestId: string, runId: string) => ({
    schema_version: 1 as const,
    work_id: "work-1",
    branch_id: "branch-1",
    run_id: runId,
    request_id: requestId,
    kind: "user_prompt" as const,
    prompt: { questions: [{ question: "What should we call it?" }] },
  });
  const { rerender } = render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [request("prompt-a", "run-a")],
      }}
    />,
  );
  fireEvent.change(screen.getByPlaceholderText("Type your answer"), {
    target: { value: "answer from A" },
  });
  rerender(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [request("prompt-b", "run-b")],
      }}
    />,
  );
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("");
});

test("does not let a delayed old interaction response clear a newer card", async () => {
  let resolveOld!: (value: unknown) => void;
  respondInteraction.mockReturnValueOnce(
    new Promise((resolve) => {
      resolveOld = resolve;
    }),
  );
  const interaction = (requestId: string, runId: string, label: string) => ({
    schema_version: 1 as const,
    work_id: "work-1",
    branch_id: "branch-1",
    run_id: runId,
    request_id: requestId,
    kind: "approval" as const,
    tool: "bash",
    approval_kind: "standard" as const,
    display_label: label,
  });
  const { rerender } = render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [interaction("approval-a", "run-a", "A")],
      }}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  rerender(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [interaction("approval-b", "run-b", "B")],
      }}
    />,
  );
  await act(async () => {
    resolveOld({
      ok: true,
      receipt: {
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        run_id: "run-a",
        request_id: "approval-a",
        outcome: "resolved",
      },
    });
  });
  expect(screen.getByText("B")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Allow this request" })).toBeInTheDocument();
});

test("does not surface a delayed old interaction error on a newer card", async () => {
  let rejectOld!: (reason: unknown) => void;
  respondInteraction.mockReturnValueOnce(
    new Promise((_resolve, reject) => {
      rejectOld = reject;
    }),
  );
  const interaction = (requestId: string, runId: string, label: string) => ({
    schema_version: 1 as const,
    work_id: "work-1",
    branch_id: "branch-1",
    run_id: runId,
    request_id: requestId,
    kind: "approval" as const,
    tool: "bash",
    approval_kind: "standard" as const,
    display_label: label,
  });
  const { rerender } = render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [interaction("approval-a", "run-a", "A")],
      }}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  rerender(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      initialInteractions={{
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        interactions: [interaction("approval-b", "run-b", "B")],
      }}
    />,
  );
  await act(async () => {
    rejectOld(new Error("old request failed"));
  });
  expect(screen.getByText("B")).toBeInTheDocument();
  expect(screen.queryByText(/The answer could not be confirmed/)).not.toBeInTheDocument();
});

test("does not offer a root Work answer button for a nested run request", () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
    />,
  );
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Start" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  act(() => {
    instance.options.onEvent({
      type: "work_turn_started",
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-1",
      run_id: "root-run",
    });
    instance.options.onEvent({ type: "run_started", run_id: "child-run" });
    instance.options.onEvent({
      type: "approval_required",
      run_id: "child-run",
      request_id: "child-approval",
      tool: "bash",
      approval_kind: "standard",
      detail: "Run child step",
    });
  });
  expect(screen.queryByRole("button", { name: "Allow this request" })).not.toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent(/nested Work step is waiting/i);
});

test("keeps the root interaction card when a child run fails", () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
    />,
  );
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Keep working" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  act(() => {
    instance.options.onEvent({
      type: "work_turn_started",
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-1",
      run_id: "root-run",
    });
    instance.options.onEvent({
      type: "user_prompt_required",
      run_id: "root-run",
      request_id: "root-prompt",
      prompt: { questions: [{ question: "Continue?" }] },
    });
    instance.options.onEvent({
      type: "run_error",
      run_id: "child-run",
      message: "child failed",
    });
  });
  expect(screen.getByRole("button", { name: "Submit answer" })).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent(/nested Work step failed/i);
});

test("refreshes the Work projection once per committed Task Graph revision", () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
    />,
  );
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Break this into tracked tasks." },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;

  act(() => {
    instance.options.onEvent({
      type: "work_task_graph_changed",
      schema_version: 1,
      graph_revision: 2,
      branch_revision: 3,
    });
    // Durable stream replay may repeat the same invalidation.
    instance.options.onEvent({
      type: "work_task_graph_changed",
      schema_version: 1,
      graph_revision: 2,
      branch_revision: 3,
    });
  });

  expect(
    screen.queryByText(/runtime returned an invalid Work event/i),
  ).not.toBeInTheDocument();
  expect(refresh).toHaveBeenCalledTimes(1);
});

test("reconnects a dropped stream with the same durable request identity", async () => {
  render(
    <StrictMode>
      <WorkTurnComposer
        workId="work-1"
        branchId="branch-1"
        attachmentId="attachment-1"
      />
    </StrictMode>,
  );
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Continue safely" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const first = streamHarness.instances[0]!;

  act(() => first.options.onStateChange("disconnected"));
  fireEvent.click(await screen.findByRole("button", { name: "Reconnect" }));

  expect(streamHarness.instances).toHaveLength(2);
  expect(JSON.parse(streamHarness.instances[1]!.options.body).request_id).toBe(
    JSON.parse(first.options.body).request_id,
  );
});

test("fails closed when a Work stream exposes internal session identity", () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
    />,
  );
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Continue safely" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;

  act(() =>
    instance.options.onEvent({
      type: "run_started",
      run_id: "run-1",
      session_id: "internal-session",
    }),
  );

  expect(screen.getByRole("alert")).toHaveTextContent(/invalid Work event/i);
  expect(instance.close).toHaveBeenCalledTimes(1);
  expect(screen.queryByRole("button", { name: "Reconnect" })).not.toBeInTheDocument();
});

test("does not offer reconnect after a typed terminal run error", () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
    />,
  );
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Continue safely" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;

  act(() => {
    instance.options.onEvent({ type: "run_error", code: "provider_failed" });
    instance.options.onStateChange("disconnected");
  });

  expect(screen.getByRole("alert")).toHaveTextContent(/run failed/i);
  expect(screen.queryByRole("button", { name: "Reconnect" })).not.toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled();
});

test("confirms forced takeover and resumes the same durable turn", async () => {
  const succeeded = {
    schema_version: 2,
    operation_id: "operation-1",
    work_id: "work-1",
    branch_id: "branch-1",
    attachment_id: "attachment-1",
    kind: "force_takeover",
    state: "succeeded",
    outcome: "taken_over",
    branch_revision: 3,
    control_basis: { writer_epoch: 4, canonical_root_hash: "a".repeat(64) },
    created_at: "2026-08-01T00:00:00Z",
    completed_at: "2026-08-01T00:00:01Z",
  } as const;
  forceTakeover.mockResolvedValue({
    ok: true,
    operation: {
      ...succeeded,
      state: "pending",
      outcome: "pending",
      completed_at: null,
      progress: { phase: "preparing", abortable: true },
    },
  });
  observeControl.mockResolvedValue({ ok: true, operation: succeeded });
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      branchRevision={3}
      controlBasis={{ writer_epoch: 4, canonical_root_hash: "a".repeat(64) }}
    />,
  );
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Continue without losing this guidance" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const first = streamHarness.instances[0]!;
  const firstBody = JSON.parse(first.options.body);

  act(() => {
    first.options.onEvent({
      type: "error",
      code: "writer_conflict",
      message: "This Work is active elsewhere. You can keep viewing it here.",
      retryable: false,
      http_status: 409,
      action_hints: ["refresh_work"],
    });
    first.options.onStateChange("disconnected");
  });

  expect(screen.queryByRole("button", { name: "Reconnect" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Continue here" }));
  expect(forceTakeover).not.toHaveBeenCalled();
  fireEvent.change(await screen.findByLabelText("Password"), {
    target: { value: "correct horse battery staple" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
  expect(forceTakeover).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    attachmentId: "attachment-1",
    requestId: "web-work-control:00000000-0000-4000-8000-000000000011",
    expectedBranchRevision: 3,
    expectedControlBasis: {
      writer_epoch: 4,
      canonical_root_hash: "a".repeat(64),
    },
    password: "correct horse battery staple",
  });
  expect(await screen.findByText("Preparing to take control here")).toBeVisible();
  await waitFor(() => expect(streamHarness.instances).toHaveLength(2));
  expect(observeControl).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    operationId: "operation-1",
  });
  expect(screen.getByText("Continue without losing this guidance")).toBeVisible();
  expect(JSON.parse(streamHarness.instances[1]!.options.body)).toEqual(firstBody);
});

test("stops a durable takeover only while the server marks it abortable", async () => {
  forceTakeover.mockResolvedValue({
    ok: true,
    operation: {
      schema_version: 2,
      operation_id: "operation-abort",
      work_id: "work-1",
      branch_id: "branch-1",
      attachment_id: "attachment-1",
      kind: "force_takeover",
      state: "pending",
      outcome: "pending",
      branch_revision: 3,
      control_basis: { writer_epoch: 4, canonical_root_hash: "a".repeat(64) },
      progress: { phase: "preparing", abortable: true },
      created_at: "2026-08-01T00:00:00Z",
      completed_at: null,
    },
  });
  abortControl.mockResolvedValue({ ok: true });
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      branchRevision={3}
      controlBasis={{ writer_epoch: 4, canonical_root_hash: "a".repeat(64) }}
    />,
  );
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Continue safely here" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const stream = streamHarness.instances[0]!;
  act(() =>
    stream.options.onEvent({
      type: "error",
      code: "writer_conflict",
      message: "This Work is active elsewhere. You can keep viewing it here.",
      retryable: false,
      http_status: 409,
      action_hints: ["refresh_work"],
    }),
  );
  fireEvent.click(screen.getByRole("button", { name: "Continue here" }));
  fireEvent.change(await screen.findByLabelText("Password"), { target: { value: "password" } });
  fireEvent.click(screen.getByRole("button", { name: "Confirm" }));

  fireEvent.click(await screen.findByRole("button", { name: "Stop taking control" }));
  await waitFor(() =>
    expect(abortControl).toHaveBeenCalledWith({
      workId: "work-1",
      branchId: "branch-1",
      operationId: "operation-abort",
    }),
  );
  expect(await screen.findByRole("alert")).toHaveTextContent(/taking control was stopped/i);
  expect(streamHarness.instances).toHaveLength(1);
});

test("does not invent execution location when stopping a takeover is unconfirmed", async () => {
  forceTakeover.mockResolvedValue({
    ok: true,
    operation: {
      schema_version: 2,
      operation_id: "operation-unknown-stop",
      work_id: "work-1",
      branch_id: "branch-1",
      attachment_id: "attachment-1",
      kind: "force_takeover",
      state: "pending",
      outcome: "pending",
      branch_revision: 3,
      control_basis: { writer_epoch: 4, canonical_root_hash: "a".repeat(64) },
      progress: { phase: "preparing", abortable: true },
      created_at: "2026-08-01T00:00:00Z",
      completed_at: null,
    },
  });
  abortControl.mockRejectedValue(new Error("connection lost after commit"));
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      branchRevision={3}
      controlBasis={{ writer_epoch: 4, canonical_root_hash: "a".repeat(64) }}
    />,
  );
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Continue safely here" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  act(() =>
    streamHarness.instances[0]!.options.onEvent({
      type: "error",
      code: "writer_conflict",
      message: "This Work is active elsewhere. You can keep viewing it here.",
      retryable: false,
      http_status: 409,
      action_hints: ["refresh_work"],
    }),
  );
  fireEvent.click(screen.getByRole("button", { name: "Continue here" }));
  fireEvent.change(await screen.findByLabelText("Password"), {
    target: { value: "password" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
  fireEvent.click(await screen.findByRole("button", { name: "Stop taking control" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    /could not confirm whether taking control stopped.*refresh to see the recorded status/i,
  );
  expect(screen.queryByRole("alert")).not.toHaveTextContent(/active on the other device/i);
});

test("keeps viewing without losing guidance or leaving optimistic messages behind", () => {
  render(
    <WorkTurnComposer
      workId="work-1"
      branchId="branch-1"
      attachmentId="attachment-1"
      branchRevision={3}
      controlBasis={{ writer_epoch: 4, canonical_root_hash: "a".repeat(64) }}
    />,
  );
  const composer = screen.getByRole("textbox", { name: "Guide this Work" });
  fireEvent.change(composer, { target: { value: "Keep this guidance safe" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const first = streamHarness.instances[0]!;

  act(() => {
    first.options.onEvent({
      type: "error",
      code: "writer_conflict",
      message: "This Work is active elsewhere. You can keep viewing it here.",
      retryable: false,
      http_status: 409,
      action_hints: ["refresh_work"],
    });
  });
  fireEvent.click(screen.getByRole("button", { name: "Keep viewing" }));

  expect(screen.getByRole("textbox", { name: "Guide this Work" })).toHaveValue(
    "Keep this guidance safe",
  );
  expect(screen.queryByText("You")).not.toBeInTheDocument();
  expect(screen.queryByText("Astra")).not.toBeInTheDocument();
  expect(first.close).toHaveBeenCalledTimes(1);
  expect(forceTakeover).not.toHaveBeenCalled();
  expect(refresh).toHaveBeenCalledTimes(1);
});
