import workProjection from "../../../../packages/sdk/src/__tests__/__fixtures__/sse/work-turn-projection.json";
const streamHarness = vi.hoisted(() => ({
  real: false,
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
  return { ...original, SSEClient: function(options: any) {
    return streamHarness.real ? new original.SSEClient(options) : new TestSSEClient(options);
  } };
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
const refreshInteractions = vi.hoisted(() => vi.fn());
vi.mock("@/app/(workspace)/works/[workId]/actions", () => ({
  forceTakeoverWorkBranchAction: forceTakeover,
  observeWorkBranchControlAction: observeControl,
  abortWorkBranchControlAction: abortControl,
  getWorkReauthenticationOptionsAction: reauthOptions,
  respondWorkBranchInteractionAction: respondInteraction,
  refreshWorkBranchInteractionsAction: refreshInteractions,
}));

import { StrictMode } from "react";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { WorkTurnComposer } from "@/components/app/work-turn-composer";
import { ToastProvider } from "@/components/ui/toast";
import type { WorkBranchInteractionPageV1 } from "@astra/sdk";

async function pollInteractions(interactions: WorkBranchInteractionPageV1["interactions"]) {
  refreshInteractions.mockResolvedValueOnce({ ok: true, page: {
    schema_version: 1, work_id: "work-1", branch_id: "branch-1", interactions,
  } });
  await act(async () => { document.dispatchEvent(new Event("visibilitychange")); });
}

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
  streamHarness.real = false;
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

test("shows hook configuration warnings and continues the same Work turn", async () => {
  render(<ToastProvider><WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" /></ToastProvider>);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Continue" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const client = streamHarness.instances[0]!;
  const message = "Hook configuration .astra/hooks.json at hooks[0].event: invalid event";
  act(() => {
    client.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "run-1" });
    client.options.onEvent({ type: "warning", code: "hook_configuration", message });
    client.options.onEvent({ type: "text_delta", content: "answer" });
  });
  expect(screen.getByRole("alert")).toHaveTextContent(message);
  expect(client.close).not.toHaveBeenCalled();
  expect(screen.getByText("answer")).toBeInTheDocument();
  act(() => {
    client.options.onEvent({ type: "run_finished", run_id: "run-1", owner_generation: 4, status: "completed" });
    client.options.onEvent({ type: "turn_complete", run_id: "run-1", continuation_owner: "server", assistant_text: "answer" });
  });
  expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled();
  expect(refresh).toHaveBeenCalledOnce();
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
    instance.options.onEvent({ type: "run_finished", run_id: "run-1", owner_generation: 4, status: "completed" });
    expect(refresh).not.toHaveBeenCalled();
    instance.options.onEvent({ type: "turn_complete", continuation_owner: "server" });
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

test("isolates prompt answers when the observed request changes", async () => {
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
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("answer from A");
  await pollInteractions([request("prompt-b", "run-b")]);
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
  await pollInteractions([interaction("approval-b", "run-b", "B")]);
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
  await pollInteractions([interaction("approval-b", "run-b", "B")]);
  await act(async () => {
    rejectOld(new Error("old request failed"));
  });
  expect(screen.getByText("B")).toBeInTheDocument();
  expect(screen.queryByText(/The answer could not be confirmed/)).not.toBeInTheDocument();
});

test("answers a nested run approval using its own durable identity", async () => {
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
      approval_kind: "protected_bash_write",
      detail: "Run child step",
    });
  });
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    attachmentId: "attachment-1",
    runId: "child-run",
    requestId: "child-approval",
    response: { kind: "approval", decision: "allow", approvalKind: "protected_bash_write" },
  }));
});

test("Work polling replaces a resolved child approval after a disconnect", async () => {
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1" };
  render(<WorkTurnComposer {...props} />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Start" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  act(() => {
    instance.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root-run" });
    instance.options.onEvent({ type: "approval_required", run_id: "child-a", request_id: "old", tool: "bash", approval_kind: "protected_bash_write", display_label: "Resolved child" });
    instance.options.onStateChange("disconnected");
  });
  expect(screen.getByText("Resolved child")).toBeInTheDocument();
  await pollInteractions([{ schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "child-b", request_id: "new", kind: "approval", tool: "bash", approval_kind: "protected_bash_write", display_label: "Waiting child" }]);
  expect(screen.queryByText("Resolved child")).not.toBeInTheDocument();
  expect(screen.getByText("Waiting child")).toBeInTheDocument();
});

test("a delayed resume cannot clear a newer approval from the same child", async () => {
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1" };
  render(<WorkTurnComposer {...props} />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Start" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  act(() => {
    instance.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root-run" });
    instance.options.onEvent({ type: "approval_required", run_id: "child", request_id: "old", tool: "bash", approval_kind: "protected_bash_write", display_label: "Old approval" });
  });
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  await waitFor(() => expect(screen.queryByText("Old approval")).not.toBeInTheDocument());
  await pollInteractions([{ schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "child", request_id: "new", kind: "approval", tool: "bash", approval_kind: "protected_bash_write", display_label: "New approval" }]);
  expect(screen.getByText("New approval")).toBeInTheDocument();
  act(() => instance.options.onEvent({ type: "run_resumed", run_id: "child", request_id: "old", reason: "tool_approval" }));
  expect(screen.getByText("New approval")).toBeInTheDocument();
});

test("a poll started before a new approval cannot erase that approval", async () => {
  let deliver!: (value: unknown) => void;
  refreshInteractions.mockImplementationOnce(() => new Promise(resolve => { deliver = resolve; }));
  render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Start" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  act(() => instance.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root-run" }));
  act(() => document.dispatchEvent(new Event("visibilitychange")));
  await waitFor(() => expect(refreshInteractions).toHaveBeenCalledOnce());
  act(() => instance.options.onEvent({ type: "approval_required", run_id: "child", request_id: "new", tool: "bash", approval_kind: "standard", display_label: "New approval" }));
  await act(async () => deliver({ ok: true, page: { schema_version: 1, work_id: "work-1", branch_id: "branch-1", interactions: [] } }));
  expect(screen.getByText("New approval")).toBeInTheDocument();
});

test("a poll from an unconfirmed response cannot erase the retryable request", async () => {
  let rejectAnswer!: (error: Error) => void;
  let deliverPoll!: (value: unknown) => void;
  respondInteraction.mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectAnswer = reject; }));
  refreshInteractions.mockImplementationOnce(() => new Promise(resolve => { deliverPoll = resolve; }));
  render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" initialInteractions={{
    schema_version: 1, work_id: "work-1", branch_id: "branch-1", interactions: [{
      schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "child", request_id: "request", kind: "approval", tool: "bash", approval_kind: "standard", display_label: "Pending approval",
    }],
  }} />);
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  act(() => document.dispatchEvent(new Event("visibilitychange")));
  await waitFor(() => expect(refreshInteractions).toHaveBeenCalledOnce());
  await act(async () => rejectAnswer(new Error("connection lost")));
  await act(async () => deliverPoll({ ok: true, page: { schema_version: 1, work_id: "work-1", branch_id: "branch-1", interactions: [] } }));
  expect(screen.getByText("Pending approval")).toBeInTheDocument();
});

test("an unconfirmed response does not block another Work interaction", async () => {
  let finishFirst!: (value: unknown) => void;
  respondInteraction.mockImplementationOnce(() => new Promise(resolve => { finishFirst = resolve; }));
  const approval = (runId: string, label: string) => ({
    schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", run_id: runId,
    request_id: "request", kind: "approval" as const, tool: "bash", display_label: label,
  });
  const first = approval("child-a", "First approval");
  const second = approval("child-b", "Second approval");
  render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" pollInteractions initialInteractions={{
    schema_version: 1, work_id: "work-1", branch_id: "branch-1", interactions: [first],
  }} />);
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  expect(screen.getByRole("button", { name: "Allow this request" })).toBeDisabled();
  await pollInteractions([second]);
  expect(screen.getByText("Second approval")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Allow this request" })).toBeEnabled();
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenCalledTimes(2));
  await waitFor(() => expect(screen.queryByText("Second approval")).not.toBeInTheDocument());
  await pollInteractions([first]);
  expect(screen.getByRole("button", { name: "Allow this request" })).toBeDisabled();
  await act(async () => finishFirst({ ok: true, receipt: { outcome: "resolved" } }));
});

test("keeps unsent answers separate across concurrent Work interactions", async () => {
  render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Start" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  const a = { schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", run_id: "child-a", request_id: "a", kind: "user_prompt" as const, prompt: { questions: [{ question: "Describe A" }] } };
  const b = { schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", run_id: "child-b", request_id: "b", kind: "user_prompt" as const, prompt: { questions: [{ question: "Describe B" }] } };
  act(() => {
    instance.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root-run" });
    instance.options.onEvent({ type: "user_prompt_required", run_id: "child-a", request_id: "a", prompt: a.prompt });
  });
  const input = screen.getByPlaceholderText("Type your answer");
  act(() => input.focus());
  fireEvent.change(input, { target: { value: "my unfinished answer" } });
  await pollInteractions([b]);
  expect(screen.getByText("Describe B")).toBeInTheDocument();
  expect(input).not.toBeInTheDocument();
  expect(screen.getByPlaceholderText("Type your answer")).not.toHaveFocus();
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("");
  fireEvent.change(screen.getByPlaceholderText("Type your answer"), { target: { value: "answer for B" } });
  await pollInteractions([a]);
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("my unfinished answer");
  await pollInteractions([b]);
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("answer for B");
  fireEvent.click(screen.getByRole("button", { name: "Submit answer" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenCalledWith(expect.objectContaining({
    runId: "child-b", requestId: "b",
    response: expect.objectContaining({ kind: "user_prompt", answers: { answers: [expect.objectContaining({ question: "Describe B", answers: ["answer for B"] })] } }),
  })));
  await pollInteractions([a]);
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("my unfinished answer");
});

test.each(["live", "replayed"])("keeps a pending answer when the Work run pauses (%s)", async mode => {
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1", pollInteractions: true };
  const initial = { schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", interactions: [] };
  const { rerender } = render(<WorkTurnComposer {...props} initialInteractions={initial} />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Start" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  const interaction = { schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", run_id: "child", request_id: "question", kind: "user_prompt" as const, prompt: { questions: [{ question: "Describe the change" }] } };
  act(() => {
    instance.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root" });
    instance.options.onEvent({ type: "user_prompt_required", run_id: "child", request_id: "question", prompt: interaction.prompt });
  });
  fireEvent.change(screen.getByPlaceholderText("Type your answer"), { target: { value: "unfinished answer" } });
  act(() => {
    instance.options.onEvent({ type: "run_finished", run_id: "root", owner_generation: 4, status: "paused", ...(mode === "replayed" ? { index: 12 } : {}) });
    if (mode === "live") instance.options.onEvent({ type: "turn_complete", continuation_owner: "server" });
  });
  rerender(<WorkTurnComposer {...props} initialInteractions={null} />);
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("unfinished answer");
  await pollInteractions([interaction]);
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("unfinished answer");
});

test.each(["runtime", "unverified"] as const)("keeps a child answer when the root cancellation is %s", async origin => {
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1" };
  const initial = { schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", interactions: [] };
  const { rerender } = render(<WorkTurnComposer {...props} initialInteractions={initial} />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Start" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  const interaction = { schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", run_id: "child", request_id: "question", kind: "user_prompt" as const, prompt: { questions: [{ question: "Describe the change" }] } };
  act(() => {
    instance.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root" });
    instance.options.onEvent({ type: "user_prompt_required", run_id: "child", request_id: "question", prompt: interaction.prompt });
  });
  fireEvent.change(screen.getByPlaceholderText("Type your answer"), { target: { value: "unfinished answer" } });
  act(() => instance.options.onEvent({ type: "run_finished", run_id: "root", owner_generation: 4, status: "cancelled", cancellation_origin: origin }));
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("unfinished answer");
  // A refresh started before the child request may finish after the root.
  rerender(<WorkTurnComposer {...props} initialInteractions={{ ...initial }} />);
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("unfinished answer");
  const stale = {
    ...interaction,
    run_id: "old-child",
    request_id: "old-question",
    prompt: { questions: [{ question: "Old question" }] },
  };
  rerender(<WorkTurnComposer {...props} initialInteractions={{ ...initial, interactions: [stale] }} />);
  expect(screen.queryByText("Old question")).not.toBeInTheDocument();
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("unfinished answer");
  refreshInteractions.mockResolvedValueOnce({ ok: false, retryable: true });
  await act(async () => {
    document.dispatchEvent(new Event("visibilitychange"));
  });
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("unfinished answer");
  rerender(<WorkTurnComposer {...props} initialInteractions={null} />);
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("unfinished answer");
  act(() => instance.options.onEvent({ type: "run_finished", run_id: "other-child", owner_generation: 4, status: "completed" }));
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("unfinished answer");
  await pollInteractions([interaction]);
  fireEvent.click(screen.getByRole("button", { name: "Submit answer" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenCalledWith(expect.objectContaining({
    runId: "child", requestId: "question",
    response: expect.objectContaining({ kind: "user_prompt", answers: { answers: [expect.objectContaining({ answers: ["unfinished answer"] })] } }),
  })));
});

test.each([null, undefined])("an unavailable interaction snapshot (%s) is retried until a successful empty read", async initialInteractions => {
  render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" initialInteractions={initialInteractions} />);
  expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled();
  refreshInteractions.mockResolvedValueOnce({ ok: false, retryable: true });
  await act(async () => { document.dispatchEvent(new Event("visibilitychange")); });
  expect(refreshInteractions).toHaveBeenCalledTimes(1);
  await pollInteractions([]);
  expect(refreshInteractions).toHaveBeenCalledTimes(2);
  await act(async () => { document.dispatchEvent(new Event("visibilitychange")); });
  expect(refreshInteractions).toHaveBeenCalledTimes(2);
});

test("a late interaction poll cannot replace a newer successful snapshot", async () => {
  let finishPoll!: (result: unknown) => void;
  refreshInteractions.mockImplementationOnce(() => new Promise(resolve => { finishPoll = resolve; }));
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1" };
  const page = { schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", interactions: [] };
  const { rerender } = render(<WorkTurnComposer {...props} initialInteractions={null} />);
  act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  await waitFor(() => expect(refreshInteractions).toHaveBeenCalledOnce());
  rerender(<WorkTurnComposer {...props} initialInteractions={page} />);
  await act(async () => finishPoll({ ok: true, page: { ...page, interactions: [{
    schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "child", request_id: "approval", kind: "approval", tool: "bash", display_label: "Old request",
  }] } }));
  expect(screen.queryByText("Old request")).not.toBeInTheDocument();
});

test("polling confirms an empty interaction snapshot before clearing a recovered child", async () => {
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1" };
  const page = { schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", interactions: [{
    schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", run_id: "child", request_id: "approval", kind: "approval" as const, tool: "bash", display_label: "Pending child",
  }] };
  const { rerender } = render(<WorkTurnComposer {...props} initialInteractions={page} />);
  expect(screen.getByText("Pending child")).toBeInTheDocument();
  rerender(<WorkTurnComposer {...props} initialInteractions={{ ...page, interactions: [] }} />);
  expect(screen.getByText("Pending child")).toBeInTheDocument();
  await pollInteractions([]);
  expect(screen.queryByText("Pending child")).not.toBeInTheDocument();
});

test("runtime cancellation removes the root question while a child remains recoverable", async () => {
  render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" pollInteractions />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Start" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  act(() => {
    instance.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root" });
    instance.options.onEvent({ type: "user_prompt_required", run_id: "root", request_id: "question", prompt: { questions: [{ question: "Root question" }] } });
  });
  fireEvent.change(screen.getByPlaceholderText("Type your answer"), { target: { value: "root draft" } });
  act(() => instance.options.onEvent({ type: "run_finished", run_id: "root", owner_generation: 4, status: "cancelled", cancellation_origin: "runtime" }));
  expect(screen.queryByText("Root question")).not.toBeInTheDocument();
  await pollInteractions([{ schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "child", request_id: "approval", kind: "approval", tool: "bash", display_label: "Child approval" }]);
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenCalledWith(expect.objectContaining({
    runId: "child", requestId: "approval", response: expect.objectContaining({ kind: "approval", decision: "allow" }),
  })));
});

test("user cancellation clears a child interaction and its draft", async () => {
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1", pollInteractions: true };
  const { rerender } = render(<WorkTurnComposer {...props} />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Start" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  const interaction = { schema_version: 1 as const, work_id: "work-1", branch_id: "branch-1", run_id: "child", request_id: "question", kind: "user_prompt" as const, prompt: { questions: [{ question: "Describe the change" }] } };
  act(() => {
    instance.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root" });
    instance.options.onEvent({ type: "user_prompt_required", run_id: "child", request_id: "question", prompt: interaction.prompt });
  });
  fireEvent.change(screen.getByPlaceholderText("Type your answer"), { target: { value: "discarded answer" } });
  act(() => instance.options.onEvent({ type: "run_finished", run_id: "root", owner_generation: 4, status: "cancelled", cancellation_origin: "user" }));
  expect(screen.queryByPlaceholderText("Type your answer")).not.toBeInTheDocument();
  rerender(<WorkTurnComposer {...props} initialInteractions={null} />);
  expect(screen.queryByPlaceholderText("Type your answer")).not.toBeInTheDocument();
  await pollInteractions([interaction]);
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("");
});

test("switching Work replaces a focused prompt without carrying its draft", () => {
  const interaction = (workId: string, question: string) => ({
    schema_version: 1 as const, work_id: workId, branch_id: "branch-1", run_id: "run", request_id: "request",
    kind: "user_prompt" as const, prompt: { questions: [{ question }] },
  });
  const page = (workId: string, question: string) => ({
    schema_version: 1 as const, work_id: workId, branch_id: "branch-1", interactions: [interaction(workId, question)],
  });
  const { rerender } = render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" initialInteractions={page("work-1", "First Work")} />);
  const input = screen.getByPlaceholderText("Type your answer");
  act(() => input.focus());
  fireEvent.change(input, { target: { value: "private draft" } });
  rerender(<WorkTurnComposer workId="work-2" branchId="branch-1" attachmentId="attachment-2" initialInteractions={null} />);
  expect(screen.queryByText("First Work")).not.toBeInTheDocument();
  rerender(<WorkTurnComposer workId="work-2" branchId="branch-1" attachmentId="attachment-2" initialInteractions={page("work-2", "Second Work")} />);
  expect(screen.getByText("Second Work")).toBeInTheDocument();
  expect(screen.getByPlaceholderText("Type your answer")).toHaveValue("");
});

test("an answered request from another Work does not hide this Work's request", async () => {
  const page = (workId: string, label: string) => ({
    schema_version: 1 as const, work_id: workId, branch_id: "branch-1", interactions: [{
      schema_version: 1 as const, work_id: workId, branch_id: "branch-1", run_id: "run", request_id: "request",
      kind: "approval" as const, tool: "bash", display_label: label,
    }],
  });
  const { rerender } = render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" initialInteractions={page("work-1", "First request")} />);
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  await waitFor(() => expect(screen.queryByText("First request")).not.toBeInTheDocument());
  rerender(<WorkTurnComposer workId="work-2" branchId="branch-1" attachmentId="attachment-2" initialInteractions={page("work-2", "Second request")} />);
  expect(screen.getByText("Second request")).toBeInTheDocument();
});

test("keeps the current approval and picks up another waiting child from polling", async () => {
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1" };
  render(<WorkTurnComposer {...props} />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), {
    target: { value: "Start" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const instance = streamHarness.instances[0]!;
  act(() => {
    instance.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root-run" });
    for (const [runId, requestId, label] of [["child-a", "request-a", "First"], ["child-b", "request-b", "Second"]]) {
      instance.options.onEvent({ type: "approval_required", run_id: runId, request_id: requestId, tool: "bash", approval_kind: "protected_bash_write", display_label: label });
    }
  });
  expect(screen.getByText("First")).toBeInTheDocument();
  expect(screen.queryByText("Second")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  await waitFor(() => expect(screen.queryByText("First")).not.toBeInTheDocument());
  await pollInteractions([{ schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "child-b", request_id: "request-b", kind: "approval", tool: "bash", approval_kind: "protected_bash_write", display_label: "Second" }]);
  expect(screen.getByText("Second")).toBeInTheDocument();
  act(() => instance.options.onEvent({ type: "run_resumed", run_id: "child-a" }));
  expect(screen.getByText("Second")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Deny" }));
  await waitFor(() => expect(respondInteraction).toHaveBeenLastCalledWith(expect.objectContaining({
    runId: "child-b", requestId: "request-b", response: { kind: "approval", decision: "deny" },
  })));
});

test("an older page snapshot cannot restore a sibling answered during overlapping responses", async () => {
  let finishB!: (value: unknown) => void;
  respondInteraction.mockImplementationOnce(() => new Promise(resolve => { finishB = resolve; }));
  const approval = (runId: string, label: string) => ({
    schema_version: 1 as const,
    work_id: "work-1",
    branch_id: "branch-1",
    run_id: runId,
    request_id: `request-${runId}`,
    kind: "approval" as const,
    tool: "bash",
    display_label: label,
  });
  const page = (interaction: ReturnType<typeof approval> | null) => ({
    schema_version: 1 as const,
    work_id: "work-1",
    branch_id: "branch-1",
    interactions: interaction ? [interaction] : [],
  });
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1" };
  const b = approval("child-b", "Approval B");
  const a = approval("child-a", "Approval A");
  const { rerender } = render(<WorkTurnComposer {...props} initialInteractions={page(b)} />);

  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  refreshInteractions.mockResolvedValueOnce({ ok: true, page: page(a) });
  rerender(<WorkTurnComposer {...props} initialInteractions={page(a)} />);
  expect(await screen.findByText("Approval A")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  await waitFor(() => expect(screen.queryByText("Approval A")).not.toBeInTheDocument());

  await act(async () => finishB({ ok: true, receipt: { outcome: "resolved" } }));
  refreshInteractions.mockResolvedValueOnce({ ok: true, page: page(null) });
  rerender(<WorkTurnComposer {...props} initialInteractions={page(b)} />);
  await waitFor(() => expect(refreshInteractions).toHaveBeenCalledTimes(2));
  expect(screen.queryByText("Approval B")).not.toBeInTheDocument();
});

test("retries interaction reconciliation when a stale page refresh fails", async () => {
  const approval = (runId: string, label: string) => ({
    schema_version: 1 as const,
    work_id: "work-1",
    branch_id: "branch-1",
    run_id: runId,
    request_id: `request-${runId}`,
    kind: "approval" as const,
    tool: "bash",
    display_label: label,
  });
  const page = (interaction: ReturnType<typeof approval> | null) => ({
    schema_version: 1 as const,
    work_id: "work-1",
    branch_id: "branch-1",
    interactions: interaction ? [interaction] : [],
  });
  const props = { workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1" };
  const b = approval("child-b", "Approval B");
  const a = approval("child-a", "Approval A");
  const { rerender } = render(<WorkTurnComposer {...props} initialInteractions={page(b)} />);

  fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
  await waitFor(() => expect(screen.queryByText("Approval B")).not.toBeInTheDocument());
  refreshInteractions.mockResolvedValueOnce({ ok: false, retryable: true });
  rerender(<WorkTurnComposer {...props} initialInteractions={page(b)} />);
  await waitFor(() => expect(refreshInteractions).toHaveBeenCalledTimes(1));
  expect(screen.queryByText("Approval B")).not.toBeInTheDocument();

  await pollInteractions([a]);
  expect(await screen.findByText("Approval A")).toBeInTheDocument();
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
  expect(screen.getByRole("alert")).toHaveTextContent(/nested Work step reported an error/i);
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

test.each(["missing receipt", "completed receipt", "diagnostic", "5xx", "408", "protocol failure"])("reconnects a dropped stream with the same durable request identity (prior=%s)", async (prior) => {
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

  act(() => {
    if (prior !== "5xx" && prior !== "408") first.options.onEvent({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "root" });
    first.options.onEvent({ type: "text_delta", content: "Useful partial answer" });
    if (prior === "5xx" || prior === "408") {
      // Use the same HTTP decoder the public SSE client invokes.
      void first.options.decodeHttpError(new Response("gateway unavailable", { status: prior === "5xx" ? 503 : 408 }))
        .then((event: unknown) => first.options.onEvent(event));
    }
    if (prior === "protocol failure") first.options.onEvent({ type: "run_started", run_id: "root", session_id: "private" });
    if (prior === "diagnostic") {
      first.options.onEvent({ type: "run_error", run_id: "root", message: "Storage unavailable", error_kind: "database_error" });
      first.options.onEvent({ type: "error", run_id: "root", code: "stream_observation_interrupted", message: "Run outcome unconfirmed", retryable: false });
    }
    if (prior === "completed receipt") first.options.onEvent({ type: "run_finished", run_id: "root", owner_generation: 4, status: "completed" });
    first.options.onStateChange("disconnected");
  });
  expect(screen.getByText("Useful partial answer")).toBeInTheDocument();
  if (prior === "5xx" || prior === "408") await waitFor(() =>
    expect(screen.getByRole("alert")).toHaveTextContent("could not be confirmed"));
  expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeDisabled();
  if (prior === "diagnostic") expect(screen.getByRole("alert")).toHaveTextContent("Storage unavailable");
  fireEvent.click(await screen.findByRole("button", { name: "Reconnect" }));

  expect(streamHarness.instances).toHaveLength(2);
  expect(JSON.parse(streamHarness.instances[1]!.options.body).request_id).toBe(
    JSON.parse(first.options.body).request_id,
  );
  act(() => {
    const retry = streamHarness.instances[1]!;
    retry.options.onEvent({ type: "text_done", full_text: "Durably saved answer" });
    retry.options.onEvent({ type: "run_finished", run_id: "root", owner_generation: 4, index: 12, status: "completed" });
  });
  expect(screen.getByText("Durably saved answer")).toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled();
  expect(refresh).toHaveBeenCalledOnce();
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
  expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeDisabled();
  fireEvent.click(screen.getByRole("button", { name: "Reconnect" }));
  expect(JSON.parse(streamHarness.instances[1]!.options.body).request_id).toBe(JSON.parse(instance.options.body).request_id);
});

test("does not offer reconnect after an owned failed receipt", () => {
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
    instance.options.onEvent({ type: "run_finished", run_id: "root", owner_generation: 4, status: "failed", error: "The run failed." });
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


test("a definite pre-admission HTTP rejection releases the rejected request", async () => {
  render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" />);
  fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Continue" } });
  fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
  const first = streamHarness.instances[0]!;
  const rejection = await first.options.decodeHttpError(new Response('{"code":"invalid_request"}', { status: 400 }));
  act(() => {
    first.options.onEvent(rejection);
    first.options.onStateChange("disconnected");
  });
  expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled();
  expect(screen.queryByRole("button", { name: "Reconnect" })).not.toBeInTheDocument();
});

test.each(["live", "replay"])("the real Work stream consumes canonical producer facts through %s completion", async mode => {
  streamHarness.real = true;
  let source!: ReadableStreamDefaultController<Uint8Array>;
  const body = new ReadableStream<Uint8Array>({ start(controller) { source = controller; } });
  const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(body));
  const emit = (...events: unknown[]) => source.enqueue(new TextEncoder().encode(
    events.map(event => 'data: ' + JSON.stringify(event) + '\n\n').join("")));
  const rendered = render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" />);
  try {
    fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Continue" } });
    fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
    emit({ type: "work_turn_started", schema_version: 1, work_id: "work-1", branch_id: "branch-1", run_id: "run-1" },
      ...workProjection.events.slice(0, -1),
      { type: "future_observation", status: "completed", graph_revision: 999 });
    await screen.findByText("Durably saved Work answer");
    expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeDisabled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(refresh).not.toHaveBeenCalled();
    const receipt: Record<string, unknown> = { ...workProjection.events.at(-1)! };
    if (mode === "live") delete receipt.index;
    emit(receipt);
    if (mode === "live") {
      // The producer's live receipt must not release the request before projection.
      await new Promise(resolve => setTimeout(resolve, 0));
      expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeDisabled();
      emit({ type: "turn_complete", continuation_owner: "server", assistant_text: "Final Work answer" });
    }
    source.enqueue(new TextEncoder().encode('data: [DONE]\n\n'));
    await waitFor(() => expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled());
    expect(refresh).toHaveBeenCalledOnce();
    expect(fetch).toHaveBeenCalledTimes(1);
  } finally {
    rendered.unmount();
    fetch.mockRestore();
  }
});

test.each([
  { status: 503, admitted: true }, { status: 408, admitted: true },
  { status: 403, admitted: false }, { status: 409, admitted: false },
])("an uncertain Work survives recovery HTTP$status (admitted=$admitted)", async ({ status, admitted }) => {
  streamHarness.real = true;
  let sent = false;
  const first = new ReadableStream<Uint8Array>({
    pull(controller) {
      if (sent) controller.error(new Error("socket reset"));
      else {
        sent = true;
        controller.enqueue(new TextEncoder().encode(
          'data: {"type":"work_turn_started","schema_version":1,"work_id":"work-1","branch_id":"branch-1","run_id":"run-1"}\n\n'
          + 'data: {"type":"text_delta","content":"Retained partial answer"}\n\n'));
      }
    },
  });
  const recovered = workProjection.events.map(event => 'data: ' + JSON.stringify(event) + '\n\n').join("")
    + 'data: [DONE]\n\n';
  const fetch = vi.spyOn(globalThis, "fetch")
    .mockResolvedValueOnce(admitted ? new Response(first) : new Response("Gateway timeout", { status: 408 }))
    .mockResolvedValueOnce(new Response("gateway unavailable", { status }))
    .mockResolvedValueOnce(new Response(recovered));
  const rendered = render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1" />);
  try {
    fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Continue" } });
    fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
    fireEvent.click(await screen.findByRole("button", { name: "Reconnect" }));
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(2));
    if (admitted) expect(screen.getByText("Retained partial answer")).toBeInTheDocument();
    else await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("original turn remains unconfirmed"));
    expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeDisabled();
    fireEvent.click(await screen.findByRole("button", { name: "Reconnect" }));
    await waitFor(() => expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled());
    expect(screen.getByText("Durably saved Work answer")).toBeInTheDocument();
    expect(fetch).toHaveBeenCalledTimes(3);
    const ids = fetch.mock.calls.map(([, init]) => JSON.parse(String(init?.body)).request_id);
    expect(new Set(ids).size).toBe(1);
    expect(refresh).toHaveBeenCalledOnce();
  } finally {
    rendered.unmount();
    fetch.mockRestore();
  }
});

test.each([
  { status: 403, history: "none" },
  { status: 403, history: "refreshed" },
  { status: 403, history: "answered" },
  { status: 409, history: "none" },
  { status: 409, history: "refreshed" },
  { status: 409, history: "answered" },
])("first HTTP$status releases guidance after $history interaction history", async ({ status, history }) => {
  streamHarness.real = true;
  const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(
    JSON.stringify({ code: status === 409 ? "writer_conflict" : "invalid_request", retryable: false }),
    { status },
  ));
  const props = {
    workId: "work-1", branchId: "branch-1", attachmentId: "attachment-1",
    branchRevision: 3, controlBasis: { writer_epoch: 4, canonical_root_hash: "a".repeat(64) },
  };
  const page: NonNullable<Parameters<typeof WorkTurnComposer>[0]["initialInteractions"]> = {
    schema_version: 1, work_id: "work-1", branch_id: "branch-1",
    interactions: history === "none" ? [] : [{
      schema_version: 1, work_id: "work-1", branch_id: "branch-1",
      run_id: "run-1", request_id: "approval-1", kind: "approval",
      tool: "bash", approval_kind: "standard", display_label: "Earlier approval",
    }],
  };
  const rendered = render(<WorkTurnComposer {...props} initialInteractions={page} />);
  try {
    if (history === "refreshed") {
      // Another surface answers the old interaction; this same composer survives refresh.
      expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeDisabled();
      rendered.rerender(<WorkTurnComposer {...props} initialInteractions={{ ...page, interactions: [] }} />);
      await pollInteractions([]);
    } else if (history === "answered") {
      // A local answer makes guidance available before new server props arrive.
      fireEvent.click(screen.getByRole("button", { name: "Allow this request" }));
      await waitFor(() => expect(respondInteraction).toHaveBeenCalledOnce());
    }
    await waitFor(() => expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled());
    fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Keep this guidance safe" } });
    fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
    await waitFor(() => expect(fetch).toHaveBeenCalledOnce());
    if (status === 409) {
      fireEvent.click(await screen.findByRole("button", { name: "Keep viewing" }));
      expect(screen.getByRole("textbox", { name: "Guide this Work" })).toHaveValue("Keep this guidance safe");
      expect(screen.queryByText("You", { exact: true })).not.toBeInTheDocument();
      expect(screen.queryByText("Astra", { exact: true })).not.toBeInTheDocument();
      expect(forceTakeover).not.toHaveBeenCalled();
    }
    await waitFor(() => expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled());
    expect(screen.queryByRole("button", { name: "Reconnect" })).not.toBeInTheDocument();
    expect(fetch).toHaveBeenCalledOnce();
  } finally {
    rendered.unmount();
    fetch.mockRestore();
  }
});


test.each(["takeover denial", "keep viewing"])("writer-conflict navigation preserves the uncertain request (%s)", async action => {
  streamHarness.real = true;
  forceTakeover.mockResolvedValue({ ok: true, operation: {
    schema_version: 2, operation_id: "operation-recover", work_id: "work-1", branch_id: "branch-1",
    attachment_id: "attachment-1", kind: "force_takeover", state: "succeeded", outcome: "taken_over",
    branch_revision: 3, control_basis: { writer_epoch: 5, canonical_root_hash: null },
    created_at: "2026-08-01T00:00:00Z", completed_at: "2026-08-01T00:00:01Z",
  } });
  const recovered = workProjection.events.map(event => 'data: ' + JSON.stringify(event) + '\n\n').join("")
    + 'data: [DONE]\n\n';
  const fetch = vi.spyOn(globalThis, "fetch")
    .mockResolvedValueOnce(new Response("Gateway timeout", { status: 408 }))
    .mockResolvedValueOnce(new Response(JSON.stringify({ code: "writer_conflict", category: "conflict",
      retryable: false, action_hints: ["refresh_work"] }), { status: 409 }));
  if (action === "takeover denial") fetch.mockResolvedValueOnce(new Response("Recovery denied", { status: 403 }));
  fetch.mockResolvedValueOnce(new Response(recovered));
  const rendered = render(<WorkTurnComposer workId="work-1" branchId="branch-1" attachmentId="attachment-1"
    branchRevision={3} controlBasis={{ writer_epoch: 4, canonical_root_hash: null }} />);
  try {
    fireEvent.change(screen.getByRole("textbox", { name: "Guide this Work" }), { target: { value: "Preserve this guidance" } });
    fireEvent.click(screen.getByRole("button", { name: "Send guidance" }));
    fireEvent.click(await screen.findByRole("button", { name: "Reconnect" }));
    await screen.findByRole("button", { name: "Continue here" });
    if (action === "keep viewing") {
      fireEvent.click(screen.getByRole("button", { name: "Keep viewing" }));
      expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeDisabled();
      expect(screen.getByRole("alert")).toHaveTextContent("original turn remains unconfirmed");
      expect(fetch).toHaveBeenCalledTimes(2);
    }
    fireEvent.click(screen.getByRole("button", { name: "Continue here" }));
    fireEvent.change(await screen.findByLabelText("Password"), { target: { value: "test reauthentication proof" } });
    fireEvent.click(screen.getByRole("button", { name: "Confirm" }));
    if (action === "takeover denial") {
      await waitFor(() => expect(fetch).toHaveBeenCalledTimes(3));
      expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeDisabled();
      fireEvent.click(await screen.findByRole("button", { name: "Reconnect" }));
    }
    await waitFor(() => expect(screen.getByRole("textbox", { name: "Guide this Work" })).toBeEnabled());
    expect(screen.getByText("Durably saved Work answer")).toBeInTheDocument();
    expect(fetch).toHaveBeenCalledTimes(action === "takeover denial" ? 4 : 3);
    const bodies = fetch.mock.calls.map(([, init]) => JSON.parse(String(init?.body)));
    expect(bodies.every(body => body.request_id === bodies[0].request_id
      && body.attachment_id === "attachment-1" && body.message === "Preserve this guidance")).toBe(true);
  } finally { rendered.unmount(); fetch.mockRestore(); }
});
