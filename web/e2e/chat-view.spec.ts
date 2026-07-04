import { expect, test, type Page } from '@playwright/test';

async function mockChatApis(page: Page, options: {
  queueDelayMs?: number;
  stopDelayMs?: number;
  streamDelayMs?: number;
  workSurface?: Record<string, unknown>;
  agentRunProjection?: Record<string, unknown>;
  activeRunStatus?: string | null;
} = {}) {
  await page.route('**/api/models', async (route) => {
    await route.fulfill({
      json: {
        items: [
          {
            id: 'sonnet-4.6-adaptive',
            name: 'Sonnet 4.6',
            subtitle: 'Responsive everyday work',
            tier: 'included',
          },
        ],
      },
    });
  });
  await page.route('**/api/skills**', async (route) => {
    await route.fulfill({ json: { skills: [], next_cursor: null } });
  });
  await page.route('**/api/edges/status', async (route) => {
    await route.fulfill({ json: { edges: [] } });
  });
  await page.route('**/api/chats/chat-e2e/work-surface', async (route) => {
    await route.fulfill({
      json: options.workSurface ?? {
        sessionId: 'chat-e2e',
        runId: 'run-e2e',
        tasks: [],
        events: [],
        generatedAt: '2026-06-07T00:00:00.000Z',
      },
    });
  });
  await page.route('**/api/chats/chat-e2e/work-surface/runs/**', async (route) => {
    await route.fulfill({
      json: options.agentRunProjection ?? {
        runId: 'child-run-e2e',
        sessionId: 'chat-e2e',
        status: 'completed',
        events: [],
        generatedAt: '2026-06-07T00:00:00.000Z',
      },
    });
  });
  await page.route('**/api/chats/chat-e2e/input', async (route) => {
    if (options.queueDelayMs) {
      await new Promise((resolve) => {
        setTimeout(resolve, options.queueDelayMs);
      });
    }
    await route.fulfill({
      json: {
        userMessage: {
          id: 'queued-user-e2e',
          role: 'user',
          content: 'queued follow-up',
          createdAt: '2026-06-07T00:00:01.000Z',
          status: 'complete',
        },
        assistantMessage: {
          id: 'queued-assistant-e2e',
          role: 'assistant',
          content: '',
          createdAt: '2026-06-07T00:00:01.001Z',
          status: 'streaming',
          reasoning: '',
          reasoningStatus: 'streaming',
        },
        activeRun: {
          runId: 'run-e2e',
          status: 'input-queued',
          waitingFor: 'user_input',
          assistantMessageId: 'queued-assistant-e2e',
        },
      },
    });
  });
  await page.route('**/api/chats/chat-e2e/stop', async (route) => {
    if (options.stopDelayMs) {
      await new Promise((resolve) => {
        setTimeout(resolve, options.stopDelayMs);
      });
    }
    await route.fulfill({
      json: {},
    });
  });
  await page.route('**/api/chats/chat-e2e/resume', async (route) => {
    await route.fulfill({
      json: {
        activeRun: {
          runId: 'run-e2e',
          status: 'running',
          waitingFor: null,
        },
      },
    });
  });
  await page.route('**/api/chats/chat-e2e/stream**', async (route) => {
    if (options.streamDelayMs) {
      await new Promise((resolve) => {
        setTimeout(resolve, options.streamDelayMs);
      });
    }
    await route.fulfill({
      contentType: 'text/event-stream',
      body: 'data: {"type":"text_done","full_text":"streamed fallback"}\n\n',
    });
  });
  await page.route('**/api/chats/chat-e2e', async (route) => {
    const activeRun = options.activeRunStatus && options.activeRunStatus !== 'idle'
      ? {
          runId: 'run-e2e',
          status: options.activeRunStatus,
          waitingFor: options.activeRunStatus === 'paused' ? 'user_resume' : null,
        }
      : undefined;
    await route.fulfill({
      json: {
        chat: {
          id: 'chat-e2e',
          title: 'E2E Chat',
          projectId: null,
          createdAt: '2026-06-07T00:00:00.000Z',
          updatedAt: '2026-06-07T00:00:00.000Z',
          archivedAt: null,
          model: 'sonnet-4.6-adaptive',
        },
        messages: [],
        activeRun,
      },
    });
  });
}

async function typeComposerMessage(page: Page, text: string) {
  const composer = page.locator('[data-composer-input="true"]');
  await composer.click();
  await page.keyboard.type(text);
}

function transcriptMessage(page: Page, text: string) {
  return page.getByTestId('chat-scroll-container').getByText(text);
}

function workSurfacePanel(page: Page) {
  return page.getByRole('complementary').last();
}

async function openWorkSurfaceIfNeeded(page: Page) {
  const surface = workSurfacePanel(page);
  const workButton = page.getByRole('button', { name: /^Activity$/ }).first();
  if (await workButton.isVisible().catch(() => false)) {
    await workButton.click();
    await expect(surface.getByRole('heading', { name: 'Activity' })).toBeVisible();
  }
  return surface;
}

async function closeMobileWorkSurfaceIfOpen(page: Page) {
  const closeButton = page.getByRole('button', { name: 'Close activity' }).last();
  if (await closeButton.isVisible().catch(() => false)) {
    await closeButton.click();
    await expect(closeButton).not.toBeVisible();
  }
}

test('run-control buttons are mutexed while queueing deferred input', async ({ page }) => {
  await mockChatApis(page, {
    activeRunStatus: 'running',
    queueDelayMs: 400,
    streamDelayMs: 20_000,
  });
  await page.goto('/e2e/chat-view?status=running');

  await typeComposerMessage(page, 'queued follow-up');
  await page.getByRole('button', { name: 'Send message' }).click();

  await expect(page.getByRole('button', { name: 'Stop' })).toBeDisabled();
  await expect(transcriptMessage(page, 'queued follow-up')).toBeVisible();
});

test('unknown active-run statuses block composer instead of enabling queue mode', async ({ page }) => {
  await mockChatApis(page, { activeRunStatus: 'initializing-provider' });
  await page.goto('/e2e/chat-view?status=initializing-provider');

  const composer = page.locator('[data-composer-input="true"]');
  await expect(composer).toHaveAttribute(
    'data-placeholder',
    'Astra is busy...',
  );
  await expect(composer).toHaveAttribute(
    'aria-label',
    'Astra is busy. Stop it or wait to continue.',
  );
  await expect(composer).toHaveAttribute('contenteditable', 'false');
});

test('empty streaming assistant shows main-chat typing feedback', async ({ page }) => {
  await mockChatApis(page, { streamDelayMs: 1000 });
  await page.goto('/e2e/chat-view?status=running&assistant=empty-streaming');

  await expect(page.getByRole('status', { name: 'Astra is responding' })).toBeVisible();
  await expect(page.getByText('Thinking', { exact: true }).first()).toBeVisible();
});

test('reasoning segment completion stays in thinking state while streaming', async ({ page }) => {
  await mockChatApis(page);
  await page.goto('/e2e/chat-view?status=idle&reasoning=segmentdone');

  await expect(page.getByText(/Thinking \d+s/).first()).toBeVisible();
  await expect(page.getByText(/^Thought/)).not.toBeVisible();
});

test('thinking transcript allows manual scrollback while streaming', async ({ page }) => {
  await mockChatApis(page, {
    activeRunStatus: 'running',
    streamDelayMs: 20_000,
  });
  await page.goto('/e2e/chat-view?status=running&long=1&reasoning=segmentdone');

  const scroller = page.getByTestId('chat-scroll-container');
  await expect(page.getByText(/Thinking \d+s/).first()).toBeVisible();
  await scroller.evaluate((element) => {
    element.scrollTop = element.scrollHeight;
  });
  const bottomScrollTop = await scroller.evaluate((element) => element.scrollTop);

  await scroller.hover();
  await page.mouse.wheel(0, -700);

  await expect.poll(async () => scroller.evaluate((element) => element.scrollTop)).toBeLessThan(
    bottomScrollTop,
  );
});

test('activity panel shows live work without main-chat metric chips', async ({ page }) => {
  await mockChatApis(page, {
    activeRunStatus: 'running',
    streamDelayMs: 20_000,
    workSurface: {
      sessionId: 'chat-e2e',
      runId: 'run-e2e',
      tasks: [
        {
          id: 'task-1',
          title: 'Investigate transport routing',
          status: 'in_progress',
          created_at: '2026-06-07T00:00:00.000Z',
          updated_at: '2026-06-07T00:00:00.000Z',
        },
      ],
      generatedAt: '2026-06-07T00:00:00.000Z',
      events: [
        {
          type: 'tool_call_start',
          call_id: 'call-bash-1',
          tool: 'bash',
          arguments: { command: 'git status --short' },
          timestamp: 1_771_000_000_000,
        },
        {
          type: 'agent_spawned',
          agent_id: 'code-review_123@child-run-e2e',
          run_id: 'child-run-e2e',
          parent_run_id: 'run-e2e',
          agent_type: 'code-review',
          description: 'Review transport routing',
          timestamp: 1_771_000_001_000,
        },
      ],
    },
  });
  await page.goto('/e2e/chat-view?status=running&assistant=empty-streaming');
  const surface = workSurfacePanel(page);

  await expect(
    page.getByRole('button', { name: /Open agents activity/i }),
  ).not.toBeVisible();
  await expect(
    page.getByRole('button', { name: /Open tasks activity/i }),
  ).not.toBeVisible();
  await expect(
    page.getByRole('button', { name: /Open tools activity/i }),
  ).not.toBeVisible();

  await surface.getByRole('button', { name: /Agents/ }).click();
  await expect(
    workSurfacePanel(page).getByText('Review transport routing').first(),
  ).toBeVisible();
  await closeMobileWorkSurfaceIfOpen(page);

  await surface.getByRole('button', { name: /Tools/ }).click();
  await expect(workSurfacePanel(page).getByText('git status --short')).toBeVisible();
  await closeMobileWorkSurfaceIfOpen(page);

  await surface.getByRole('button', { name: /Tasks/ }).click();
  await expect(workSurfacePanel(page).getByText('Investigate transport routing')).toBeVisible();
});

test('stop immediately clears the visible run while cancellation is slow', async ({ page }) => {
  await mockChatApis(page, { stopDelayMs: 1500, streamDelayMs: 3000 });
  await page.goto('/e2e/chat-view?status=running&assistant=empty-streaming');

  await page.getByRole('button', { name: 'Stop run' }).click();

  await expect(transcriptMessage(page, 'Stopped.')).toBeVisible({ timeout: 500 });
  await expect(page.getByText('Thinking', { exact: true })).not.toBeVisible();
  await expect(page.getByRole('button', { name: 'Stop run' })).not.toBeVisible();
});

test('activity panel hides environment internals before work runs', async ({ page }) => {
  await mockChatApis(page, {
    workSurface: {
      sessionId: 'chat-e2e',
      runId: 'run-e2e',
      tasks: [],
      generatedAt: '2026-06-07T00:00:00.000Z',
      events: [
        {
          type: 'workspace_bound',
          workspace: {
            kind: 'server_sandbox',
            display_name: 'Server sandbox',
            cwd: '/tmp/astra-workspaces/chat-e2e',
            authority: 'read_write',
            fallback_policy: 'disabled',
          },
        },
        {
          type: 'executor_bound',
          executor: {
            kind: 'server_local',
            executor_id: 'server-local',
            display_name: 'Server sandbox',
            transport: 'server_local',
            status: 'online',
          },
        },
      ],
    },
  });
  await page.goto('/e2e/chat-view?status=running');
  const surface = await openWorkSurfaceIfNeeded(page);

  await expect(surface.getByRole('heading', { name: 'Activity' })).toBeVisible();
  await expect(surface.getByText('Workspace', { exact: true })).toHaveCount(0);
  await expect(surface.getByText('Executor', { exact: true })).toHaveCount(0);
  await expect(surface.getByText('Server sandbox')).toHaveCount(0);
});

test('activity tool cards show runtime files and connection', async ({ page }) => {
  await mockChatApis(page, {
    workSurface: {
      sessionId: 'chat-e2e',
      runId: 'run-e2e',
      tasks: [],
      generatedAt: '2026-06-07T00:00:00.000Z',
      events: [
        {
          type: 'workspace_bound',
          workspace: {
            kind: 'edge_workspace',
            display_name: 'MacBook Pro',
            cwd: '/Users/xupeng/github/astra',
            authority: 'read_write',
            fallback_policy: 'disabled',
          },
        },
        {
          type: 'executor_bound',
          executor: {
            kind: 'edge_agent',
            executor_id: 'edge-1',
            display_name: 'MacBook Pro',
            transport: 'edge_ws',
            status: 'online',
          },
        },
        {
          type: 'tool_transport_completed',
          call_id: 'call-bash',
          tool: 'bash',
          success: true,
          duration_ms: 42,
          result: 'ok',
          transport: 'edge_ledger',
          fallback_policy: 'disabled',
          workspace: {
            kind: 'edge_workspace',
            display_name: 'MacBook Pro',
            cwd: '/Users/xupeng/github/astra',
            authority: 'read_write',
            fallback_policy: 'disabled',
          },
          executor: {
            kind: 'edge_agent',
            executor_id: 'edge-1',
            display_name: 'MacBook Pro',
            transport: 'edge_ledger',
            status: 'online',
          },
        },
      ],
    },
  });
  await page.goto('/e2e/chat-view?status=idle');
  const surface = await openWorkSurfaceIfNeeded(page);

  await surface.getByRole('button', { name: /Tools/ }).click();

  await expect(surface.getByRole('heading', { name: 'bash' })).toBeVisible();
  await expect(surface.getByText('MacBook Pro').first()).toBeVisible();
  await expect(surface.getByText('/Users/xupeng/github/astra').first()).toBeVisible();
  await expect(surface.getByText('Connection', { exact: true })).toBeVisible();
  await expect(surface.getByText('edge ledger', { exact: true })).toBeVisible();
  await expect(surface.getByText('Policy', { exact: true })).toBeVisible();
  await expect(surface.getByText('disabled', { exact: true })).toBeVisible();
});

test('activity shows actionable execution-environment blocked state', async ({ page }) => {
  await mockChatApis(page, {
    workSurface: {
      sessionId: 'chat-e2e',
      runId: 'run-e2e',
      tasks: [],
      generatedAt: '2026-06-07T00:00:00.000Z',
      events: [
        {
          type: 'workspace_bound',
          workspace: {
            kind: 'edge_workspace',
            display_name: 'MacBook Pro',
            cwd: '/Users/xupeng/github/astra',
            authority: 'read_write',
            fallback_policy: 'disabled',
          },
        },
        {
          type: 'executor_bound',
          executor: {
            kind: 'edge_agent',
            executor_id: 'edge-1',
            display_name: 'MacBook Pro',
            transport: 'edge_ws',
            status: 'online',
          },
        },
        {
          type: 'run_blocked',
          call_id: 'call-bash',
          tool: 'bash',
          reason: 'executor_offline',
          message:
            "Error: executor 'MacBook Pro' is offline. Server fallback is disabled.",
          transport: 'edge_ws',
          fallback_policy: 'disabled',
          workspace: {
            kind: 'edge_workspace',
            display_name: 'MacBook Pro',
            cwd: '/Users/xupeng/github/astra',
            authority: 'read_write',
            fallback_policy: 'disabled',
          },
          executor: {
            kind: 'edge_agent',
            executor_id: 'edge-1',
            display_name: 'MacBook Pro',
            transport: 'edge_ws',
            status: 'offline',
          },
        },
      ],
    },
  });
  await page.goto('/e2e/chat-view?status=running');
  const surface = await openWorkSurfaceIfNeeded(page);

  await expect(surface.getByText('Run blocked')).toBeVisible();
  await expect(
    surface.getByText('Execution environment is offline. Reconnect it or choose another environment.'),
  ).toBeVisible();
  await expect(surface.getByText('/Users/xupeng/github/astra').first()).toBeVisible();
  await expect(surface.getByText('policy disabled')).toBeVisible();
});

test('activity distinguishes transport disconnect from executor offline', async ({ page }) => {
  await mockChatApis(page, {
    workSurface: {
      sessionId: 'chat-e2e',
      runId: 'run-e2e',
      tasks: [],
      generatedAt: '2026-06-07T00:00:00.000Z',
      events: [
        {
          type: 'workspace_bound',
          workspace: {
            kind: 'edge_workspace',
            display_name: 'MacBook Pro',
            cwd: '/Users/xupeng/github/astra',
            authority: 'read_write',
            fallback_policy: 'disabled',
          },
        },
        {
          type: 'executor_bound',
          executor: {
            kind: 'edge_agent',
            executor_id: 'edge-1',
            display_name: 'MacBook Pro',
            transport: 'edge_ws',
            status: 'online',
          },
        },
        {
          type: 'run_blocked',
          call_id: 'call-bash',
          tool: 'bash',
          reason: 'transport_disconnected',
          message:
            "Error: transport 'edge_ws' disconnected or timed out while executing tool 'bash' on executor 'MacBook Pro'.",
          transport: 'edge_ws',
          fallback_policy: 'disabled',
          workspace: {
            kind: 'edge_workspace',
            display_name: 'MacBook Pro',
            cwd: '/Users/xupeng/github/astra',
            authority: 'read_write',
            fallback_policy: 'disabled',
          },
          executor: {
            kind: 'edge_agent',
            executor_id: 'edge-1',
            display_name: 'MacBook Pro',
            transport: 'edge_ws',
            status: 'degraded',
          },
        },
      ],
    },
  });
  await page.goto('/e2e/chat-view?status=running');
  const surface = await openWorkSurfaceIfNeeded(page);

  await expect(surface.getByText('Run blocked')).toBeVisible();
  await expect(
    surface.getByText('Execution connection disconnected. Reconnect it or retry after it recovers.'),
  ).toBeVisible();
  await expect(surface.getByText('connection edge ws')).toBeVisible();
  await expect(surface.getByText('policy disabled')).toBeVisible();
});

test('agent cards expand into live child run details with runtime metadata', async ({ page }) => {
  const workspace = {
    kind: 'edge_workspace',
    display_name: 'MacBook Pro',
    cwd: '/Users/xupeng/github/astra',
    authority: 'read_write',
    fallback_policy: 'disabled',
  };
  const executor = {
    kind: 'edge_agent',
    executor_id: 'edge-1',
    display_name: 'MacBook Pro',
    transport: 'edge_ws',
    status: 'online',
  };
  await mockChatApis(page, {
    workSurface: {
      sessionId: 'chat-e2e',
      runId: 'run-e2e',
      tasks: [],
      generatedAt: '2026-06-07T00:00:00.000Z',
      events: [
        {
          type: 'workspace_bound',
          workspace,
        },
        {
          type: 'executor_bound',
          executor,
        },
        {
          type: 'agent_spawned',
          agent_id: 'code-review_123@child-run-e2e',
          run_id: 'child-run-e2e',
          parent_run_id: 'run-e2e',
          agent_type: 'code-review',
          description: 'Review the branch for regressions',
          workspace,
          executor,
          transport: 'edge_ws',
          fallback_policy: 'disabled',
          timestamp: 1_771_000_000_000,
        },
        {
          type: 'agent_progress',
          agent_id: 'code-review_123@child-run-e2e',
          status: 'tool_executing',
          tool_name: 'bash',
          workspace,
          executor,
          transport: 'edge_ws',
          fallback_policy: 'disabled',
          timestamp: 1_771_000_001_000,
        },
      ],
    },
    agentRunProjection: {
      runId: 'child-run-e2e',
      sessionId: 'chat-e2e',
      status: 'running',
      workspace,
      executor,
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
      generatedAt: '2026-06-07T00:00:00.000Z',
      events: [
        {
          type: 'tool_call_end',
          call_id: 'child-call-1',
          tool: 'bash',
          success: true,
          result: 'child bash output: inspected src/lib.rs',
        },
        {
          type: 'text_delta',
          content: 'live child review finding',
        },
      ],
    },
  });
  await page.goto('/e2e/chat-view?status=running');
  const surface = await openWorkSurfaceIfNeeded(page);

  await surface.getByRole('button', { name: /Agents/ }).click();
  await surface.getByRole('button', { name: /code-review/ }).click();

  await expect(surface.getByText('Live activity')).toBeVisible();
  await expect(surface.getByText('Child run events')).toBeVisible();
  await expect(surface.getByText('Spawned')).toBeVisible();
  await expect(surface.getByText('Running bash').first()).toBeVisible();
  await expect(surface.getByText('child bash output: inspected src/lib.rs')).toBeVisible();
  await expect(surface.getByText('live child review finding')).toBeVisible();
  await expect(surface.getByText('/Users/xupeng/github/astra').first()).toBeVisible();
  await expect(surface.getByText('MacBook Pro').first()).toBeVisible();
  await expect(surface.getByText('Connection', { exact: true }).first()).toBeVisible();
  await expect(surface.getByText('edge ws').first()).toBeVisible();
  await expect(surface.getByText('Policy', { exact: true }).first()).toBeVisible();
  await expect(surface.getByText('disabled').first()).toBeVisible();
});

test('manual scrollback is preserved when a deferred message is appended', async ({ page }) => {
  await mockChatApis(page);
  await page.goto('/e2e/chat-view?status=running&long=1');

  const scroller = page.getByTestId('chat-scroll-container');
  await scroller.evaluate((element) => {
    element.scrollTop = 120;
    element.dispatchEvent(new Event('scroll', { bubbles: true }));
  });
  const before = await scroller.evaluate((element) => element.scrollTop);

  await typeComposerMessage(page, 'queued follow-up');
  await page.getByRole('button', { name: 'Send message' }).click();
  await expect(transcriptMessage(page, 'queued follow-up')).toBeVisible();

  const after = await scroller.evaluate((element) => element.scrollTop);
  expect(after).toBe(before);
});

test('pinned chat scrolls to the newest deferred message', async ({ page }) => {
  await mockChatApis(page);
  await page.goto('/e2e/chat-view?status=running&long=1');

  const scroller = page.getByTestId('chat-scroll-container');
  await scroller.evaluate((element) => {
    element.scrollTop = element.scrollHeight;
    element.dispatchEvent(new Event('scroll', { bubbles: true }));
  });

  await typeComposerMessage(page, 'queued follow-up');
  await page.getByRole('button', { name: 'Send message' }).click();
  await expect(transcriptMessage(page, 'queued follow-up')).toBeVisible();

  const distanceFromBottom = await scroller.evaluate((element) => (
    element.scrollHeight - element.scrollTop - element.clientHeight
  ));
  expect(distanceFromBottom).toBeLessThan(4);
});

test('mobile queue placeholder is compact and visible', async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== 'mobile-chromium', 'mobile-only placeholder assertion');
  await mockChatApis(page);
  await page.goto('/e2e/chat-view?status=running');

  await expect(page.locator('[data-composer-input="true"]')).toHaveAttribute(
    'data-placeholder',
    'Queue follow-up...',
  );
});
