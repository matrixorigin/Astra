"use client";

import {
  SSEClient,
  decodeWorkTurnStreamEventV1,
  type StreamEvent,
  type WorkApiErrorV1,
  type WorkBranchControlBasisV1,
  type WorkBranchControlOperationV2,
  type WorkBranchInteractionPageV1,
  type WorkBranchInteractionV1,
  type WorkTurnStreamEvent,
} from "@astra/sdk";
import rehypeHighlight from "rehype-highlight";
import rehypeKatex from "rehype-katex";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import { ArrowUp, RotateCcw } from "lucide-react";
import { useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  abortWorkBranchControlAction,
  forceTakeoverWorkBranchAction,
  getWorkReauthenticationOptionsAction,
  observeWorkBranchControlAction,
  respondWorkBranchInteractionAction,
} from "@/app/(workspace)/works/[workId]/actions";

type LocalMessage = { id: string; role: "user" | "assistant"; text: string };
type PendingTurn = { requestId: string; message: string };
type TurnState = "idle" | "connecting" | "working" | "waiting" | "disconnected";
type PromptQuestion = {
  id?: string;
  question?: string;
  header?: string;
  options?: PromptOption[];
  multi_select?: boolean;
  allow_freeform?: boolean;
};
type PromptOption = { label?: string; description?: string } | string;

type PromptAnswer = string | string[];
const PROMPT_OTHER_VALUE = "__astra_other__";

const markdownRemarkPlugins = [remarkGfm, remarkMath];
const markdownRehypePlugins = [rehypeKatex, rehypeHighlight];

function localTurnPath(workId: string, branchId: string) {
  return `/api/works/${encodeURIComponent(workId)}/branches/${encodeURIComponent(branchId)}/turns`;
}

function sameInteractionIdentity(
  left: WorkBranchInteractionV1 | null | undefined,
  right: WorkBranchInteractionV1 | null | undefined,
) {
  return left?.run_id === right?.run_id && left?.request_id === right?.request_id;
}

function eventRunId(event: WorkTurnStreamEvent): string | null {
  const runId = (event as { run_id?: unknown }).run_id;
  return typeof runId === "string" && runId.length > 0 ? runId : null;
}

function takeoverPhaseLabel(operation: WorkBranchControlOperationV2) {
  switch (operation.progress?.phase) {
    case "awaiting_reauthentication":
      return "Confirming your identity";
    case "preparing":
      return "Preparing to take control here";
    case "fencing":
      return "Stopping new work on the other device";
    case "sealing_effects":
      return "Preserving uncertain external effects for review";
    case "activating":
      return "Opening the Work here";
    default:
      return "Taking control here";
  }
}

async function decodeHttpError(response: Response): Promise<StreamEvent> {
  let body: Partial<WorkApiErrorV1> = {};
  try {
    body = (await response.json()) as Partial<WorkApiErrorV1>;
  } catch {
    // Status remains sufficient for deterministic recovery UI.
  }
  return {
    type: "error",
    code: typeof body.code === "string" ? body.code : "work_turn_unavailable",
    message:
      response.status === 401
        ? "Your sign-in expired."
        : body.code === "writer_conflict"
          ? "This Work is active elsewhere. You can keep viewing it here."
          : body.code === "attachment_fenced"
            ? "This view is no longer attached. Refresh before continuing."
        : response.status === 409
          ? "This Work changed before the turn could start."
          : "The Work turn could not start.",
    retryable: body.retryable === true,
    http_status: response.status,
    action_hints: Array.isArray(body.action_hints) ? body.action_hints : [],
  };
}

export function WorkTurnComposer({
  workId,
  branchId,
  attachmentId,
  branchRevision,
  controlBasis,
  initialDraft = "",
  initialInteractions,
  onActivityChange,
}: {
  workId: string;
  branchId: string;
  attachmentId?: string;
  branchRevision?: number;
  controlBasis?: WorkBranchControlBasisV1;
  initialDraft?: string;
  initialInteractions?: WorkBranchInteractionPageV1 | null;
  onActivityChange?: (active: boolean) => void;
}) {
  const [draft, setDraft] = useState(initialDraft);
  const [messages, setMessages] = useState<LocalMessage[]>([]);
  const [state, setState] = useState<TurnState>("idle");
  const [error, setError] = useState<string | null>(null);
  const [canReconnect, setCanReconnect] = useState(false);
  const [controlConflict, setControlConflict] = useState(false);
  const [takingControl, setTakingControl] = useState(false);
  const [confirmingTakeover, setConfirmingTakeover] = useState(false);
  const [takeoverPassword, setTakeoverPassword] = useState("");
  const [takeoverMethod, setTakeoverMethod] = useState<{ method: "password" } | { method: "memoria"; verification_url: string }>({ method: "password" });
  const [controlOperation, setControlOperation] =
    useState<WorkBranchControlOperationV2 | null>(null);
  const [abortingControl, setAbortingControl] = useState(false);
  const [pendingInteraction, setPendingInteraction] =
    useState<WorkBranchInteractionV1 | null>(
      initialInteractions?.interactions[0] ?? null,
    );
  const pendingInteractionRef = useRef<WorkBranchInteractionV1 | null>(
    initialInteractions?.interactions[0] ?? null,
  );
  const [interactionSubmitting, setInteractionSubmitting] = useState(false);
  const [promptAnswers, setPromptAnswers] = useState<Record<string, PromptAnswer>>({});
  const [currentControlBasis, setCurrentControlBasis] = useState(controlBasis);
  const pending = useRef<PendingTurn | null>(null);
  const controlBlocked = useRef(false);
  const controlRequestId = useRef<string | null>(null);
  const activeControlOperationId = useRef<string | null>(null);
  const rootRunId = useRef<string | null>(
    initialInteractions?.interactions[0]?.run_id ?? null,
  );
  const activeRunId = useRef<string | null>(
    initialInteractions?.interactions[0]?.run_id ?? null,
  );
  const stream = useRef<SSEClient | null>(null);
  const assistantMessageId = useRef<string | null>(null);
  const refreshedGraphRevision = useRef(0);
  const mounted = useRef(true);
  const router = useRouter();

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      stream.current?.close();
      onActivityChange?.(false);
    };
  }, [onActivityChange]);

  useEffect(() => {
    onActivityChange?.(pending.current !== null);
  }, [onActivityChange, state]);

  useEffect(() => {
    setCurrentControlBasis(controlBasis);
  }, [controlBasis]);

  useEffect(() => {
    if (!pending.current) {
      const interaction = initialInteractions?.interactions[0] ?? null;
      const changed = !sameInteractionIdentity(pendingInteractionRef.current, interaction);
      pendingInteractionRef.current = interaction;
      setPendingInteraction(interaction);
      rootRunId.current = interaction?.run_id ?? rootRunId.current;
      activeRunId.current = interaction?.run_id ?? activeRunId.current;
      if (changed) setPromptAnswers({});
    }
  }, [initialInteractions]);

  function updateAssistant(text: string, replace = false) {
    const id = assistantMessageId.current;
    if (!id) return;
    setMessages((current) =>
      current.map((message) =>
        message.id === id
          ? { ...message, text: replace ? text : `${message.text}${text}` }
          : message,
      ),
    );
  }

  function handleEvent(raw: StreamEvent) {
    let event: WorkTurnStreamEvent;
    try {
      event = decodeWorkTurnStreamEventV1(raw);
    } catch {
      pending.current = null;
      stream.current?.close();
      setState("disconnected");
      setCanReconnect(false);
      setError("The runtime returned an invalid Work event. No further events were applied.");
      return;
    }

    switch (event.type) {
      case "work_turn_started":
        rootRunId.current = event.run_id;
        activeRunId.current = event.run_id;
        setState("working");
        setError(null);
        break;
      case "run_started":
        if (event.run_id) {
          if (rootRunId.current && event.run_id !== rootRunId.current) break;
          rootRunId.current ??= event.run_id;
          activeRunId.current = event.run_id;
        }
        setState("working");
        setError(null);
        break;
      case "text_delta":
        updateAssistant(event.content);
        break;
      case "text_done":
        updateAssistant(event.full_text, true);
        break;
      case "run_waiting":
      case "run_blocked":
        if (eventRunId(event) && rootRunId.current && eventRunId(event) !== rootRunId.current) {
          break;
        }
        setState("waiting");
        break;
      case "approval_required":
      case "user_prompt_required": {
        const runId = event.run_id ?? rootRunId.current ?? activeRunId.current;
        if (!runId) {
          setError("The Work is waiting for input, but its durable run identity was missing. Refresh to retry.");
          setState("disconnected");
          break;
        }
        if (rootRunId.current && runId !== rootRunId.current) {
          setState("waiting");
          setError(
            "A nested Work step is waiting for input. This view can observe it, but the nested request must be answered from its owning run.",
          );
          break;
        }
        const interaction: WorkBranchInteractionV1 = event.type === "approval_required"
          ? {
              schema_version: 1,
              work_id: workId,
              branch_id: branchId,
              run_id: runId,
              request_id: event.request_id,
              kind: "approval",
              tool: event.tool,
              approval_kind: event.approval_kind,
              path: event.path,
              detail: event.detail,
              display_label: event.display_label,
            }
          : {
              schema_version: 1,
              work_id: workId,
              branch_id: branchId,
              run_id: runId,
              request_id: event.request_id,
              kind: "user_prompt",
              prompt: event.prompt,
        };
        activeRunId.current = runId;
        rootRunId.current ??= runId;
        const changed = !sameInteractionIdentity(pendingInteractionRef.current, interaction);
        pendingInteractionRef.current = interaction;
        setPendingInteraction(interaction);
        if (changed) setPromptAnswers({});
        setState("waiting");
        setError(null);
        break;
      }
      case "work_task_graph_changed":
        if (event.graph_revision > refreshedGraphRevision.current) {
          refreshedGraphRevision.current = event.graph_revision;
          // React's server refresh preserves the active composer while
          // replacing the Work projections with the committed graph revision.
          router.refresh();
        }
        break;
      case "turn_complete":
        if (eventRunId(event) && rootRunId.current && eventRunId(event) !== rootRunId.current) {
          break;
        }
        if (event.assistant_text) updateAssistant(event.assistant_text, true);
        pending.current = null;
        pendingInteractionRef.current = null;
        setPendingInteraction(null);
        setPromptAnswers({});
        setState("idle");
        setCanReconnect(false);
        setError(null);
        rootRunId.current = null;
        activeRunId.current = null;
        router.refresh();
        break;
      case "error":
        if (event.code === "writer_conflict") {
          controlBlocked.current = true;
          setControlConflict(true);
        } else if (event.retryable !== true) {
          pending.current = null;
        }
        setState("disconnected");
        setCanReconnect(event.code !== "writer_conflict" && event.retryable === true);
        setError(event.message);
        if (
          event.code !== "writer_conflict" &&
          event.action_hints?.includes("refresh_work")
        ) {
          router.refresh();
        }
        break;
      case "run_error":
        if (eventRunId(event) && rootRunId.current && eventRunId(event) !== rootRunId.current) {
          setState("waiting");
          setError("A nested Work step failed; the root Work state is still available.");
          break;
        }
        pending.current = null;
        pendingInteractionRef.current = null;
        setPendingInteraction(null);
        setPromptAnswers({});
        setState("disconnected");
        setCanReconnect(false);
        setError("The Work run failed. Its durable state is preserved.");
        break;
      default:
        break;
    }
  }

  function connect(turn: PendingTurn) {
    stream.current?.close();
    setState("connecting");
    setError(null);
    setCanReconnect(false);
    const client = new SSEClient({
      url: localTurnPath(workId, branchId),
      method: "POST",
      body: JSON.stringify({
        request_id: turn.requestId,
        attachment_id: attachmentId,
        message: turn.message,
      }),
      maxRetries: 0,
      decodeHttpError,
      onEvent: handleEvent,
      onStateChange: (connection) => {
        if (
          mounted.current &&
          connection === "disconnected" &&
          pending.current &&
          !controlBlocked.current
        ) {
          setState("disconnected");
          setCanReconnect(true);
          setError("The stream disconnected. Reconnect to the same durable turn.");
        }
      },
    });
    stream.current = client;
    void client.connect();
  }

  function submit() {
    const message = draft.trim();
    if (!message || !attachmentId || pending.current) return;
    const requestId = `web-work-turn:${crypto.randomUUID()}`;
    const assistantId = `assistant:${requestId}`;
    assistantMessageId.current = assistantId;
    pending.current = { requestId, message };
    setMessages((current) => [
      ...current,
      { id: `user:${requestId}`, role: "user", text: message },
      { id: assistantId, role: "assistant", text: "" },
    ]);
    setDraft("");
    connect(pending.current);
  }

  async function answerInteraction(
    response:
      | { kind: "approval"; decision: "allow" | "deny"; reason?: string }
      | { kind: "user_prompt"; cancelled: boolean; answers?: unknown },
  ) {
    const interaction = pendingInteraction;
    if (!interaction || !attachmentId || interactionSubmitting) return;
    setInteractionSubmitting(true);
    setError(null);
    try {
      const result = await respondWorkBranchInteractionAction({
        workId,
        branchId,
        attachmentId,
        runId: interaction.run_id,
        requestId: interaction.request_id,
        response,
      });
      if (!mounted.current) return;
      // Polling can discover a newer request while this response is in
      // flight. Never let the older response clear or overwrite the newer
      // interaction card.
      if (!sameInteractionIdentity(pendingInteractionRef.current, interaction)) {
        router.refresh();
        return;
      }
      if (!result.ok) {
        setError(
          result.code === "attachment_fenced"
            ? "This Work view expired. Refresh to answer the request from a fresh attachment."
            : result.code === "interaction_response_conflict"
              ? "Another surface already answered this request. Refresh to see the current Work state."
              : result.retryable
                ? "The answer was not confirmed. You can safely try again."
                : "This answer was rejected. Refresh to see the recorded Work state.",
        );
        return;
      }
      setPendingInteraction(null);
      pendingInteractionRef.current = null;
      setPromptAnswers({});
      setState(result.receipt.outcome === "authority_lost" ? "disconnected" : "working");
      setError(
        result.receipt.outcome === "idempotent"
          ? "This request was already answered. Refreshing the Work."
          : result.receipt.outcome === "queued"
            ? "Answer recorded. The Work will continue when its execution authority is available."
            : result.receipt.outcome === "authority_lost"
              ? "Answer recorded, but the previous executor is no longer authoritative. Refresh to review the Work state."
              : result.receipt.outcome === "superseded"
                ? "A newer instruction superseded this request. Refresh to review the Work state."
                : null,
      );
      router.refresh();
    } catch {
      if (
        mounted.current &&
        sameInteractionIdentity(pendingInteractionRef.current, interaction)
      ) {
        setError("The answer could not be confirmed. Refresh before trying again.");
      } else if (mounted.current) {
        router.refresh();
      }
    } finally {
      if (mounted.current) setInteractionSubmitting(false);
    }
  }

  function promptQuestions(interaction: WorkBranchInteractionV1): PromptQuestion[] {
    const prompt = interaction.prompt;
    if (!prompt || typeof prompt !== "object" || Array.isArray(prompt)) return [];
    const questions = (prompt as { questions?: unknown }).questions;
    if (!Array.isArray(questions)) return [];
    return questions.filter(
      (question): question is PromptQuestion =>
        Boolean(question && typeof question === "object" && !Array.isArray(question)),
    );
  }

  function promptQuestionKey(question: PromptQuestion, index: number) {
    return `${index}:${question.question ?? question.header ?? "question"}`;
  }

  function promptOptionLabel(option: PromptOption) {
    return typeof option === "string" ? option : option.label ?? "Option";
  }

  function submitPrompt() {
    if (!pendingInteraction || pendingInteraction.kind !== "user_prompt") return;
    const answers = promptQuestions(pendingInteraction).map((question, index) => {
      const key = promptQuestionKey(question, index);
      const raw = promptAnswers[key];
      const other = promptAnswers[`${key}:other`];
      let values: string[];
      if (question.multi_select) {
        const rawSelected = Array.isArray(raw) ? raw : [];
        const otherSelected = rawSelected.includes(PROMPT_OTHER_VALUE);
        const selected = rawSelected.filter((value) => value !== PROMPT_OTHER_VALUE);
        const freeform = otherSelected
          ? typeof other === "string" ? other : ""
          : !question.options?.length && typeof raw === "string" ? raw : "";
        if (otherSelected && !freeform.trim()) {
          setError("Enter an answer for Other before submitting the Work request.");
          return;
        }
        values = [
          ...selected,
          ...freeform.split(",").map((value) => value.trim()).filter(Boolean),
        ];
      } else {
        const selected = typeof raw === "string" ? raw : "";
        values = [
          selected === PROMPT_OTHER_VALUE
            ? typeof other === "string" ? other.trim() : ""
            : selected.trim(),
        ].filter(Boolean);
      }
      if (values.length === 0) {
        setError("Answer each question before submitting the Work request.");
        return;
      }
      return {
        ...(question.id !== undefined ? { id: question.id } : {}),
        question: question.question ?? question.header ?? "",
        answers: values,
        multi_select: question.multi_select === true,
        annotation: null,
      };
    });
    if (answers.some((answer) => answer === undefined)) return;
    void answerInteraction({ kind: "user_prompt", cancelled: false, answers: { answers } });
  }

  function viewLive() {
    const turn = pending.current;
    if (turn) {
      setDraft(turn.message);
      setMessages((current) =>
        current.filter((message) => !message.id.endsWith(turn.requestId)),
      );
    }
    pending.current = null;
    stream.current?.close();
    controlBlocked.current = false;
    setControlConflict(false);
    setConfirmingTakeover(false);
    setTakeoverPassword("");
    setControlOperation(null);
    activeControlOperationId.current = null;
    setState("idle");
    setError(null);
    router.refresh();
  }

  function finishTakeover(operation: WorkBranchControlOperationV2, turn: PendingTurn) {
    activeControlOperationId.current = null;
    setControlOperation(null);
    controlRequestId.current = null;
    if (operation.state === "succeeded") {
      if (operation.control_basis) setCurrentControlBasis(operation.control_basis);
      controlBlocked.current = false;
      setControlConflict(false);
      setConfirmingTakeover(false);
      setError(null);
      connect(turn);
      return;
    }
    if (operation.outcome === "aborted") {
      setConfirmingTakeover(false);
      setError("Taking control was stopped. Refresh to confirm the recorded Work status.");
      return;
    }
    if (operation.outcome === "head_conflict" && operation.control_basis) {
      setCurrentControlBasis(operation.control_basis);
      setError("The branch advanced. Continue here again from the latest safe point.");
      return;
    }
    if (operation.outcome === "branch_revision_conflict") {
      setError("The Work plan changed. Refreshing the current branch before continuing.");
      router.refresh();
      return;
    }
    setError("This Work is still running elsewhere. You can keep viewing it here.");
  }

  async function observeTakeover(operationId: string, turn: PendingTurn) {
    let attempt = 0;
    while (mounted.current && activeControlOperationId.current === operationId) {
      const delay = Math.min(350 * 2 ** Math.floor(attempt / 4), 2_000);
      await new Promise((resolve) => window.setTimeout(resolve, delay));
      if (!mounted.current || activeControlOperationId.current !== operationId) return;
      const result = await observeWorkBranchControlAction({ workId, branchId, operationId });
      if (!mounted.current || activeControlOperationId.current !== operationId) return;
      if (!result.ok) {
        setError("Could not refresh the recorded taking-control status. Refresh before trying again.");
        return;
      }
      setControlOperation(result.operation);
      if (result.operation.state !== "pending") {
        finishTakeover(result.operation, turn);
        return;
      }
      attempt += 1;
    }
  }

  async function abortTakeover() {
    const operationId = activeControlOperationId.current;
    if (!operationId || abortingControl) return;
    setAbortingControl(true);
    try {
      const result = await abortWorkBranchControlAction({ workId, branchId, operationId });
      if (!mounted.current || activeControlOperationId.current !== operationId) return;
      if (result.ok) {
        activeControlOperationId.current = null;
        setControlOperation(null);
        setTakingControl(false);
        setConfirmingTakeover(false);
        setError("Taking control was stopped. Refresh to confirm the recorded Work status.");
      } else {
        setError(
          result.code === "control_operation_not_abortable"
            ? "Taking control has already started and cannot be stopped here. Refresh to see its status."
            : "Could not confirm whether taking control stopped. Refresh to see the recorded status.",
        );
      }
    } catch {
      if (mounted.current && activeControlOperationId.current === operationId) {
        setError("Could not confirm whether taking control stopped. Refresh to see the recorded status.");
      }
    } finally {
      if (mounted.current) setAbortingControl(false);
    }
  }

  async function checkTakeover() {
    const turn = pending.current;
    const operationId = activeControlOperationId.current;
    if (!turn || !operationId || takingControl) return;
    setTakingControl(true);
    try {
      await observeTakeover(operationId, turn);
    } catch {
      if (mounted.current) {
        setError("Could not refresh the recorded taking-control status. Refresh before trying again.");
      }
    } finally {
      if (mounted.current) setTakingControl(false);
    }
  }

  async function continueHere() {
    const turn = pending.current;
    if (
      !turn ||
      !attachmentId ||
      branchRevision === undefined ||
      !currentControlBasis ||
      takeoverPassword.length === 0 ||
      takingControl
    ) {
      return;
    }
    setTakingControl(true);
    setError(null);
    const requestId =
      controlRequestId.current ?? `web-work-control:${crypto.randomUUID()}`;
    controlRequestId.current = requestId;
    try {
      // A failed exchange may already have consumed the one-time evidence.
      // Do not leave it available for an accidental retry.
      if (takeoverMethod.method === "memoria") setTakeoverPassword("");
      const result = await forceTakeoverWorkBranchAction({
        workId,
        branchId,
        attachmentId,
        requestId,
        expectedBranchRevision: branchRevision,
        expectedControlBasis: currentControlBasis,
        ...(takeoverMethod.method === "memoria" ? { memoriaProof: takeoverPassword } : { password: takeoverPassword }),
      });
      if (!mounted.current) return;
      if (!result.ok) {
        if (
          !result.retryable &&
          result.code !== "reauthentication_required" &&
          result.status !== 403
        ) {
          controlRequestId.current = null;
        }
        setError(
          result.status === 401
              ? "Identity verification was not accepted or your sign-in expired. The Work stayed on the other device."
            : result.code === "reauthentication_required" || result.status === 403
              ? "Identity verification was not accepted. The Work stayed on the other device."
            : result.retryable
              ? "This Work could not be continued here yet. You can safely try again."
              : "This Work could not continue on this device.",
        );
        return;
      }
      controlRequestId.current = null;
      setTakeoverPassword("");
      const operation = result.operation;
      if (operation.state === "pending") {
        activeControlOperationId.current = operation.operation_id;
        setControlOperation(operation);
        setError("Taking control here…");
        await observeTakeover(operation.operation_id, turn);
        return;
      }
      finishTakeover(operation, turn);
    } catch {
      if (mounted.current) {
        setError("Taking control could not be confirmed. Refresh to see the recorded status before retrying.");
      }
    } finally {
      if (mounted.current) setTakingControl(false);
    }
  }

  const busy = pending.current !== null || pendingInteraction !== null;
  return (
    <section className="rounded-card border border-border/80 bg-surface shadow-[0_1px_2px_rgba(15,23,42,0.025)]">
      {messages.length > 0 ? (
        <div className="space-y-5 border-b border-border/70 px-5 py-5">
          {messages.map((message) => (
            <div
              key={message.id}
              className={message.role === "user" ? "ml-auto max-w-[85%]" : "max-w-[92%]"}
            >
              <p className="text-[11px] font-semibold uppercase tracking-[0.08em] text-text-muted">
                {message.role === "user" ? "You" : "Astra"}
              </p>
              {message.text ? (
                <div className="astra-markdown mt-1 text-sm leading-6 text-text">
                  <ReactMarkdown
                    remarkPlugins={markdownRemarkPlugins}
                    rehypePlugins={markdownRehypePlugins}
                  >
                    {message.text}
                  </ReactMarkdown>
                </div>
              ) : busy ? (
                <p className="mt-1 text-sm leading-6 text-text-muted">Working…</p>
              ) : null}
            </div>
          ))}
        </div>
      ) : null}

      {pendingInteraction ? (
        <div className="border-b border-accent/20 bg-accent/5 px-5 py-4" role="status">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div>
              <p className="text-sm font-semibold text-text">
                {pendingInteraction.kind === "approval" ? "Astra needs approval" : "Astra needs your answer"}
              </p>
              <p className="mt-1 text-xs leading-5 text-text-secondary">
                Answer this request from the current Work. It does not move execution to this device.
              </p>
            </div>
            <span className="rounded-full border border-accent/20 px-2 py-1 text-[11px] font-medium text-accent">
              {pendingInteraction.kind === "approval" ? "Approval" : "Question"}
            </span>
          </div>

          {pendingInteraction.kind === "approval" ? (
            <div className="mt-3 rounded-control border border-border/70 bg-surface px-3 py-3">
              <p className="text-sm font-medium text-text">
                {pendingInteraction.display_label ?? pendingInteraction.tool ?? "Requested action"}
              </p>
              {pendingInteraction.detail || pendingInteraction.path ? (
                <p className="mt-1 whitespace-pre-wrap text-xs leading-5 text-text-secondary">
                  {pendingInteraction.detail ?? pendingInteraction.path}
                </p>
              ) : null}
              <div className="mt-3 flex flex-wrap gap-2">
                <Button
                  size="sm"
                  onClick={() => void answerInteraction({ kind: "approval", decision: "allow" })}
                  disabled={interactionSubmitting || !attachmentId}
                >
                  Allow this request
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => void answerInteraction({ kind: "approval", decision: "deny" })}
                  disabled={interactionSubmitting || !attachmentId}
                >
                  Deny
                </Button>
              </div>
            </div>
          ) : (
            <div className="mt-3 space-y-3">
              {promptQuestions(pendingInteraction).map((question, index) => {
                const key = promptQuestionKey(question, index);
                const options = question.options ?? [];
                const value = promptAnswers[key];
                const freeformKey = `${key}:other`;
                const selectedValues = Array.isArray(value) ? value : [];
                const showOther =
                  question.allow_freeform === true &&
                  ((!question.multi_select && value === PROMPT_OTHER_VALUE) ||
                    (question.multi_select && selectedValues.includes(PROMPT_OTHER_VALUE)));
                return (
                  <fieldset key={key} className="block rounded-control border border-border/70 bg-surface px-3 py-3">
                    <span className="text-sm font-medium text-text">
                      {question.question ?? question.header ?? "Your answer"}
                    </span>
                    {options.length > 0 && !question.multi_select ? (
                      <select
                        value={typeof value === "string" ? value : ""}
                        onChange={(event) =>
                          setPromptAnswers((current) => ({ ...current, [key]: event.target.value }))
                        }
                        className="mt-2 block w-full rounded-control border border-border bg-surface px-3 py-2 text-sm text-text outline-none focus:border-accent"
                        disabled={interactionSubmitting || !attachmentId}
                      >
                        <option value="">Choose an answer</option>
                        {options.map((option, optionIndex) => {
                          const label = promptOptionLabel(option);
                          return <option key={`${key}:${optionIndex}`} value={label}>{label}</option>;
                        })}
                        {question.allow_freeform ? <option value={PROMPT_OTHER_VALUE}>Other…</option> : null}
                      </select>
                    ) : options.length > 0 && question.multi_select ? (
                      <div className="mt-2 space-y-2">
                        {options.map((option, optionIndex) => {
                          const label = promptOptionLabel(option);
                          const checked = selectedValues.includes(label);
                          return (
                            <label key={`${key}:${optionIndex}`} className="flex items-start gap-2 text-sm text-text-secondary">
                              <input
                                type="checkbox"
                                checked={checked}
                                onChange={(event) =>
                                  setPromptAnswers((current) => {
                                    const selected = Array.isArray(current[key]) ? current[key] as string[] : [];
                                    return {
                                      ...current,
                                      [key]: event.target.checked
                                        ? [...selected, label]
                                        : selected.filter((item) => item !== label),
                                    };
                                  })
                                }
                                disabled={interactionSubmitting || !attachmentId}
                              />
                              <span>
                                <span className="block text-text">{label}</span>
                                {typeof option !== "string" && option.description ? (
                                  <span className="block text-xs text-text-muted">{option.description}</span>
                                ) : null}
                              </span>
                            </label>
                          );
                        })}
                        {question.allow_freeform ? (
                          <label className="flex items-start gap-2 text-sm text-text-secondary">
                            <input
                              type="checkbox"
                              checked={selectedValues.includes(PROMPT_OTHER_VALUE)}
                              onChange={(event) =>
                                setPromptAnswers((current) => {
                                  const selected = Array.isArray(current[key]) ? current[key] as string[] : [];
                                  return {
                                    ...current,
                                    [key]: event.target.checked
                                      ? [...selected, PROMPT_OTHER_VALUE]
                                      : selected.filter((item) => item !== PROMPT_OTHER_VALUE),
                                  };
                                })
                              }
                              disabled={interactionSubmitting || !attachmentId}
                            />
                            <span>Other answer</span>
                          </label>
                        ) : null}
                      </div>
                    ) : (
                      <input
                        value={typeof value === "string" ? value : ""}
                        onChange={(event) =>
                          setPromptAnswers((current) => ({ ...current, [key]: event.target.value }))
                        }
                        placeholder={question.multi_select ? "Enter answers separated by commas" : "Type your answer"}
                        className="mt-2 block w-full rounded-control border border-border bg-surface px-3 py-2 text-sm text-text outline-none focus:border-accent"
                        disabled={interactionSubmitting || !attachmentId}
                      />
                    )}
                    {showOther ? (
                      <input
                        value={typeof promptAnswers[freeformKey] === "string" ? promptAnswers[freeformKey] as string : ""}
                        onChange={(event) =>
                          setPromptAnswers((current) => ({ ...current, [freeformKey]: event.target.value }))
                        }
                        placeholder="Type your answer"
                        className="mt-2 block w-full rounded-control border border-border bg-surface px-3 py-2 text-sm text-text outline-none focus:border-accent"
                        disabled={interactionSubmitting || !attachmentId}
                      />
                    ) : null}
                  </fieldset>
                );
              })}
              <div className="flex flex-wrap gap-2">
                <Button
                  size="sm"
                  onClick={submitPrompt}
                  disabled={interactionSubmitting || !attachmentId || promptQuestions(pendingInteraction).length === 0}
                >
                  Submit answer
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => void answerInteraction({ kind: "user_prompt", cancelled: true })}
                  disabled={interactionSubmitting || !attachmentId}
                >
                  Cancel request
                </Button>
              </div>
            </div>
          )}
          {!attachmentId ? (
            <p className="mt-3 text-xs text-warning">This view cannot answer yet because its attachment expired. Refresh the Work.</p>
          ) : null}
        </div>
      ) : null}

      {error ? (
        <div
          role="alert"
          className={`flex flex-wrap items-center gap-3 border-b px-4 py-3 text-sm ${
            controlConflict
              ? "border-warning/25 bg-warning/5 text-text"
              : "border-danger/20 bg-danger/5 text-danger"
          }`}
        >
          <span className="min-w-0 flex-1">{error}</span>
          {controlConflict && branchRevision !== undefined && currentControlBasis ? (
            confirmingTakeover ? (
              <div className="basis-full rounded-control border border-warning/20 bg-surface px-3 py-3">
                {controlOperation ? (
                  <div className="flex flex-wrap items-center gap-3">
                    <p className="min-w-0 flex-1 text-xs leading-5 text-text-secondary">
                      {takeoverPhaseLabel(controlOperation)}
                    </p>
                    {!takingControl ? (
                      <Button size="sm" onClick={() => void checkTakeover()}>
                        Check again
                      </Button>
                    ) : null}
                    {controlOperation.progress?.abortable ? (
                      <Button
                        size="sm"
                        variant="secondary"
                        onClick={() => void abortTakeover()}
                        disabled={abortingControl}
                      >
                        {abortingControl ? "Stopping…" : "Stop taking control"}
                      </Button>
                    ) : null}
                  </div>
                ) : (
                  <>
                    <p className="text-xs leading-5 text-text-secondary">
                      Continuing here stops work on the other device. Any uncertain external
                      effects are kept for review and are not repeated automatically.
                    </p>
                    <div className="mt-3 flex flex-wrap items-center gap-2">
                      {takeoverMethod.method === "memoria" ? (
                        <a className="basis-full text-sm text-accent underline" href={`${takeoverMethod.verification_url}?purpose=session_forced_takeover`} target="_blank" rel="noopener noreferrer">
                          Verify your identity by email, then paste the one-time proof below
                        </a>
                      ) : null}
                      <input
                        type="password"
                        value={takeoverPassword}
                        onChange={(event) => setTakeoverPassword(event.target.value)}
                        onKeyDown={(event) => {
                          if (event.key === "Enter") {
                            event.preventDefault();
                            void continueHere();
                          }
                        }}
                        autoComplete={takeoverMethod.method === "memoria" ? "off" : "current-password"}
                        placeholder={takeoverMethod.method === "memoria" ? "One-time verification proof" : "Confirm with your password"}
                        aria-label={takeoverMethod.method === "memoria" ? "One-time verification proof" : "Password"}
                        className="min-w-56 flex-1 rounded-control border border-border bg-surface px-3 py-2 text-sm text-text outline-none focus:border-accent"
                      />
                      <Button
                        size="sm"
                        onClick={() => void continueHere()}
                        disabled={takingControl || takeoverPassword.length === 0}
                      >
                        {takingControl ? "Taking control…" : "Confirm"}
                      </Button>
                      <Button
                        size="sm"
                        variant="secondary"
                        onClick={() => {
                          setConfirmingTakeover(false);
                          setTakeoverPassword("");
                        }}
                        disabled={takingControl}
                      >
                        Cancel
                      </Button>
                    </div>
                  </>
                )}
              </div>
            ) : (
              <div className="flex items-center gap-2">
                <Button size="sm" variant="secondary" onClick={viewLive}>
                  Keep viewing
                </Button>
                <Button size="sm" disabled={takingControl} onClick={async () => {
                  setTakingControl(true);
                  try {
                    setTakeoverMethod(await getWorkReauthenticationOptionsAction());
                    setTakeoverPassword("");
                    setConfirmingTakeover(true);
                  } catch {
                    setError("Unable to load identity verification. Please try again.");
                  } finally { setTakingControl(false); }
                }}>
                  Continue here
                </Button>
              </div>
            )
          ) : null}
          {canReconnect && pending.current ? (
            <Button
              size="sm"
              variant="secondary"
              leadingIcon={RotateCcw}
              onClick={() => pending.current && connect(pending.current)}
            >
              Reconnect
            </Button>
          ) : null}
        </div>
      ) : null}

      <div className="p-3">
        <textarea
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              submit();
            }
          }}
          disabled={busy || !attachmentId}
          rows={3}
          className="block min-h-24 w-full resize-y bg-transparent px-2 py-2 text-sm leading-6 text-text outline-none placeholder:text-text-muted disabled:opacity-60"
          placeholder={attachmentId ? "Guide this Work…" : "Reconnect to continue this Work…"}
          aria-label="Guide this Work"
        />
        <div className="mt-2 flex items-center justify-between gap-3 px-1">
          <p className="text-xs text-text-muted">
            {state === "connecting"
              ? "Connecting…"
              : state === "working"
                ? "Working…"
                : state === "waiting"
                  ? "Waiting for a required input or capability"
                  : "Enter to send · Shift+Enter for a new line"}
          </p>
          <button
            type="button"
            onClick={submit}
            disabled={busy || !attachmentId || draft.trim().length === 0}
            className="inline-flex size-9 shrink-0 items-center justify-center rounded-full bg-text text-white transition hover:bg-text/90 disabled:opacity-35"
            aria-label="Send guidance"
          >
            <ArrowUp className="size-4" />
          </button>
        </div>
      </div>
    </section>
  );
}
