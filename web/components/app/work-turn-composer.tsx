"use client";

import {
  SSEClient,
  decodeWorkTurnStreamEventV1,
  type StreamEvent,
  type WorkApiErrorV1,
  type WorkBranchControlBasisV1,
  type WorkBranchControlOperationV2,
  type WorkTurnStreamEvent,
} from "@astra/sdk";
import { ArrowUp, RotateCcw } from "lucide-react";
import { useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  abortWorkBranchControlAction,
  forceTakeoverWorkBranchAction,
  observeWorkBranchControlAction,
} from "@/app/(workspace)/works/[workId]/actions";

type LocalMessage = { id: string; role: "user" | "assistant"; text: string };
type PendingTurn = { requestId: string; message: string };
type TurnState = "idle" | "connecting" | "working" | "waiting" | "disconnected";

function localTurnPath(workId: string, branchId: string) {
  return `/api/works/${encodeURIComponent(workId)}/branches/${encodeURIComponent(branchId)}/turns`;
}

function takeoverPhaseLabel(operation: WorkBranchControlOperationV2) {
  switch (operation.progress?.phase) {
    case "awaiting_reauthentication":
      return "Confirming your identity";
    case "preparing":
      return "Preparing a safe handoff";
    case "fencing":
      return "Stopping new work on the other device";
    case "sealing_effects":
      return "Preserving uncertain external effects for review";
    case "activating":
      return "Opening the Work here";
    default:
      return "Moving this Work here";
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
  onActivityChange,
}: {
  workId: string;
  branchId: string;
  attachmentId?: string;
  branchRevision?: number;
  controlBasis?: WorkBranchControlBasisV1;
  initialDraft?: string;
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
  const [controlOperation, setControlOperation] =
    useState<WorkBranchControlOperationV2 | null>(null);
  const [abortingControl, setAbortingControl] = useState(false);
  const [currentControlBasis, setCurrentControlBasis] = useState(controlBasis);
  const pending = useRef<PendingTurn | null>(null);
  const controlBlocked = useRef(false);
  const controlRequestId = useRef<string | null>(null);
  const activeControlOperationId = useRef<string | null>(null);
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
      case "run_started":
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
        setState("waiting");
        break;
      case "work_task_graph_changed":
        if (event.graph_revision > refreshedGraphRevision.current) {
          refreshedGraphRevision.current = event.graph_revision;
          // React's server refresh preserves the active composer while
          // replacing the Work projections with the committed graph revision.
          router.refresh();
        }
        break;
      case "turn_complete":
        if (event.assistant_text) updateAssistant(event.assistant_text, true);
        pending.current = null;
        setState("idle");
        setCanReconnect(false);
        setError(null);
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
        pending.current = null;
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
      setError("The move was stopped. This Work is still active on the other device.");
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
        setError("The move is still recorded, but its progress could not be refreshed.");
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
        setError("The move was stopped. This Work is still active on the other device.");
      } else {
        setError(
          result.code === "control_operation_not_abortable"
            ? "The safe handoff has already started and can no longer be stopped."
            : "The move could not be stopped. Its durable status is unchanged.",
        );
      }
    } catch {
      if (mounted.current && activeControlOperationId.current === operationId) {
        setError("The stop request could not be confirmed. The durable move is unchanged.");
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
        setError("The move is still recorded, but its progress could not be refreshed.");
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
      const result = await forceTakeoverWorkBranchAction({
        workId,
        branchId,
        attachmentId,
        requestId,
        expectedBranchRevision: branchRevision,
        expectedControlBasis: currentControlBasis,
        password: takeoverPassword,
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
            ? "Your password was not accepted or your sign-in expired. Nothing was moved."
            : result.code === "reauthentication_required" || result.status === 403
              ? "Your password was not accepted. Nothing was moved."
            : result.retryable
              ? "This Work could not move here yet. You can safely try again."
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
        setError("Moving this Work here…");
        await observeTakeover(operation.operation_id, turn);
        return;
      }
      finishTakeover(operation, turn);
    } catch {
      if (mounted.current) {
        setError("The move could not be confirmed. You can safely try the same action again.");
      }
    } finally {
      if (mounted.current) setTakingControl(false);
    }
  }

  const busy = pending.current !== null;
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
              <p className="mt-1 whitespace-pre-wrap text-sm leading-6 text-text">
                {message.text || (busy ? "Working…" : "")}
              </p>
            </div>
          ))}
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
                        {abortingControl ? "Stopping…" : "Stop moving"}
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
                        autoComplete="current-password"
                        placeholder="Confirm with your password"
                        aria-label="Password"
                        className="min-w-56 flex-1 rounded-control border border-border bg-surface px-3 py-2 text-sm text-text outline-none focus:border-accent"
                      />
                      <Button
                        size="sm"
                        onClick={() => void continueHere()}
                        disabled={takingControl || takeoverPassword.length === 0}
                      >
                        {takingControl ? "Moving…" : "Confirm"}
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
                <Button size="sm" onClick={() => setConfirmingTakeover(true)}>
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
