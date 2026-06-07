import { expect, test, type Page } from '@playwright/test';

async function mockChatApis(page: Page, options: {
  queueDelayMs?: number;
  stopDelayMs?: number;
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
    await route.fulfill({ json: { items: [], nextOffset: null } });
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
        activeRun: {
          runId: 'run-e2e',
          status: 'input-queued',
          waitingFor: 'user_input',
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
      json: {
        activeRun: {
          runId: 'run-e2e',
          status: 'cancelling',
          waitingFor: null,
        },
      },
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
    await route.fulfill({
      contentType: 'text/event-stream',
      body: 'data: {"type":"text_done","full_text":"streamed fallback"}\n\n',
    });
  });
  await page.route('**/api/chats/chat-e2e', async (route) => {
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

test('run-control buttons are mutexed while queueing deferred input', async ({ page }) => {
  await mockChatApis(page, { queueDelayMs: 400 });
  await page.goto('/e2e/chat-view?status=running');

  await typeComposerMessage(page, 'queued follow-up');
  await page.getByRole('button', { name: 'Send message' }).click();

  await expect(page.getByRole('button', { name: 'Stop' })).toBeDisabled();
  await page.getByRole('button', { name: 'Stop' }).click({ force: true });
  await expect(transcriptMessage(page, 'queued follow-up')).toBeVisible();
});

test('unknown active-run statuses block composer instead of enabling queue mode', async ({ page }) => {
  await mockChatApis(page);
  await page.goto('/e2e/chat-view?status=initializing-provider');

  await expect(page.getByText('Run status is initializing-provider. Stop it or refresh before sending new input.')).toBeVisible();
  await expect(page.locator('[data-composer-input="true"]')).toHaveAttribute('contenteditable', 'false');
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
