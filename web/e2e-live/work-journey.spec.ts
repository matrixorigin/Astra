import { expect, test } from '@playwright/test';
import { existsSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { visibleMarkdownSnippet } from './visible-markdown';
import { configureRuntimeSession } from './runtime-session';

type LiveState = {
  schema_version: 1;
  phase: string;
  work_id?: string;
  branch_id?: string;
  run_id?: string;
  goal?: string;
  milestones?: Record<string, Record<string, unknown>>;
  [key: string]: unknown;
};

type LiveTranscript = {
  schema_version?: number;
  work_id?: string;
  branch_id?: string;
  sync?: string;
  items?: { role?: string; content?: string }[];
};

const statePath = process.env.ASTRA_WORK_LIVE_STATE;
const controlPath = process.env.ASTRA_WORK_LIVE_CONTROL;
const accessToken = process.env.ASTRA_WORK_LIVE_ACCESS_TOKEN;
const apiUrl = process.env.ASTRA_API_URL ?? 'http://127.0.0.1:17001';

function readState(): LiveState | null {
  if (!statePath || !existsSync(statePath)) return null;
  try {
    const parsed: unknown = JSON.parse(readFileSync(statePath, 'utf8'));
    if (!parsed || typeof parsed !== 'object') return null;
    const state = parsed as LiveState;
    return state.schema_version === 1 ? state : null;
  } catch {
    // The controller publishes with rename; a concurrent read can only see
    // the previous complete record, never a partially written JSON file.
    return null;
  }
}

function writeControl(command: string): void {
  if (!controlPath) throw new Error('ASTRA_WORK_LIVE_CONTROL is not set');
  const temporary = `${controlPath}.${process.pid}.${randomUUID()}.tmp`;
  writeFileSync(
    temporary,
    `${JSON.stringify({ schema_version: 1, command, control_id: randomUUID() })}\n`,
    { encoding: 'utf8', mode: 0o600 },
  );
  renameSync(temporary, controlPath);
}

async function waitForPhase(phase: string): Promise<LiveState> {
  await expect
    .poll(
      () => {
        const state = readState();
        return state?.phase === phase || Boolean(state?.milestones?.[phase]);
      },
      { timeout: 100_000, message: `live controller phase ${phase}` },
    )
    .toBe(true);
  const state = readState();
  if (!state || (state.phase !== phase && !state.milestones?.[phase])) {
    throw new Error(`live controller phase ${phase} disappeared after observation`);
  }
  return state;
}

async function waitForStartedWork(): Promise<LiveState> {
  // The controller records Web readiness after creating the Work and before
  // launching this browser.  Accept either complete phase while requiring the
  // Work identity to be present; a stale state file is removed by the runner.
  await expect
    .poll(() => {
      const state = readState();
      if (state?.milestones?.started || state?.phase === 'started') return 'started';
      return state?.phase === 'web_ready' ? 'web_ready' : '';
    }, {
      timeout: 100_000,
      message: 'live controller has created a Work',
    })
    .toMatch(/^(started|web_ready)$/);
  const state = readState();
  if (!state || !state.work_id || !state.branch_id) {
    throw new Error('live controller published an incomplete Work identity');
  }
  return state;
}

test.beforeEach(async ({ context }) => {
  if (!statePath || !controlPath || !accessToken) {
    throw new Error(
      'live Work harness requires ASTRA_WORK_LIVE_STATE, ASTRA_WORK_LIVE_CONTROL, and ASTRA_WORK_LIVE_ACCESS_TOKEN',
    );
  }
  const webUrl = process.env.ASTRA_WORK_LIVE_WEB_URL ?? 'http://127.0.0.1:3537';
  await configureRuntimeSession(context.request, webUrl, apiUrl, accessToken);
  await context.addCookies([
    { name: 'astra_web_client_id', value: `live-harness-${process.pid}`, url: webUrl },
  ]);
});

test('one Work is discoverable and observable across TUI and Web', async ({ page, request }) => {
  const started = await waitForStartedWork();
  const workId = started.work_id;
  const goal = started.goal;
  expect(workId).toBeTruthy();
  expect(started.branch_id).toBeTruthy();
  expect(goal).toBeTruthy();

  await page.goto('/now', { waitUntil: 'domcontentloaded' });
  await expect(page.getByRole('heading', { name: 'What needs your attention' })).toBeVisible();
  const workLink = page.locator(`a[href="/works/${encodeURIComponent(workId as string)}"]`);
  await expect(workLink).toContainText(goal as string);
  await expect(workLink).toBeVisible();
  await workLink.click();
  await expect(page).toHaveURL(new RegExp(`/works/${workId}$`));
  await expect(page.getByRole('heading', { name: goal as string })).toBeVisible();
  await expect(page.locator('#work-plan')).toBeVisible();
  await expect(page.locator('#work-conversation')).toBeVisible();
  writeControl('web_observed');

  const admitted = await waitForPhase('run_admitted');
  expect(admitted.work_id).toBe(workId);
  expect(admitted.branch_id).toBe(started.branch_id);
  expect(admitted.run_id).toBeTruthy();
  const runId = admitted.run_id as string;

  // This is a real page refresh/poll, not a mocked route. It proves Web saw
  // the same Work while its root Run was active before the TUI left.
  await expect
    .poll(
      async () => {
        await page.reload({ waitUntil: 'domcontentloaded' });
        return (await page.getByRole('status').allTextContents()).join(' ');
      },
      { timeout: 100_000, message: 'Web observes the admitted Work as active' },
    )
    .toMatch(/Astra is working/);
  writeControl('web_working_observed');

  const exited = await waitForPhase('tui_exited');
  expect(exited.run_id).toBe(runId);
  const settled = await waitForPhase('run_settled');
  expect(settled.run_id).toBe(runId);
  expect(settled.proof).toMatchObject({
    same_run_id: true,
    events_increased_after_tui_exit: true,
    terminal_after_tui_exit: true,
  });

  // Read the canonical Work projections directly as an independent oracle.
  // A mounted conversation card or a boolean in the handshake is not proof
  // that the completed Run produced a visible result.
  let terminalEvent: Record<string, unknown> | null = null;
  await expect
    .poll(
      async () => {
        try {
          const response = await request.get(
            `${apiUrl}/v1/works/${encodeURIComponent(workId as string)}/events?limit=100`,
            {
              headers: {
                Authorization: `Bearer ${accessToken}`,
                'x-astra-work-api-major': '1',
              },
            },
          );
          if (!response.ok()) return '';
          const body = (await response.json()) as {
            page?: { events?: Record<string, unknown>[] };
          };
          terminalEvent =
            body.page?.events?.find(
              (event) =>
                event.kind === 'run_completed' && event.source_ref === `run:${runId}`,
            ) ?? null;
          return terminalEvent?.source_ref ?? '';
        } catch {
          // A transient API failure is retried by expect.poll.  Do not expose
          // the Authorization header through Playwright's failure call log.
          return '';
        }
      },
      { timeout: 100_000, message: 'canonical Work completion event' },
    )
    .toBe(`run:${runId}`);
  expect(terminalEvent).toMatchObject({
    kind: 'run_completed',
    source_ref: `run:${runId}`,
  });

  let transcript: LiveTranscript | null = null;
  let assistantText = '';
  await expect
    .poll(
      async () => {
        try {
          const response = await request.get(
            `${apiUrl}/v1/works/${encodeURIComponent(workId as string)}/branches/${encodeURIComponent(started.branch_id as string)}/transcript?limit=50`,
            {
              headers: {
                Authorization: `Bearer ${accessToken}`,
                'x-astra-work-api-major': '1',
              },
            },
          );
          if (!response.ok()) return '';
          const body = (await response.json()) as LiveTranscript;
          transcript = body;
          assistantText =
            body.items
              ?.filter((item) => item.role === 'assistant' && item.content?.trim())
              .map((item) => item.content!.trim())
              .at(-1) ?? '';
          return assistantText;
        } catch {
          return '';
        }
      },
      { timeout: 100_000, message: 'canonical assistant result in Work transcript' },
    )
    .toMatch(/\S/);
  const finalTranscript = transcript as unknown as LiveTranscript;
  expect(finalTranscript).toMatchObject({
    schema_version: 1,
    work_id: workId,
    branch_id: started.branch_id,
  });
  expect(finalTranscript.sync).not.toBe('corrupt');
  expect(finalTranscript.sync).not.toBe('offline');
  const visibleSnippet = visibleMarkdownSnippet(assistantText);
  expect(visibleSnippet).toMatch(/\S/);

  // Re-open the same canonical Work after the client process is gone. The
  // page must retain the durable result and present no fake second identity.
  await page.reload({ waitUntil: 'domcontentloaded' });
  await expect(page.getByRole('heading', { name: goal as string })).toBeVisible();
  await expect(page.locator('#work-plan')).toBeVisible();
  await expect(page.locator('#work-conversation')).toBeVisible();
  await expect(page.locator('#work-conversation')).toContainText(visibleSnippet);
  await expect(page.getByRole('alert')).toHaveCount(0);
  expect(readState()?.run_id).toBe(runId);
  writeControl('web_settled_observed');
});
