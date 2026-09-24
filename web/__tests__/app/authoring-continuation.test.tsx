import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { HarnessesPage } from '@/components/app/harnesses-page';
import { listChats } from '@/lib/api/chats';
import * as harness from '@/lib/api/harnesses';

const navigation = vi.hoisted(() => ({ query: 'runId=existing&draftId=chosen' }));
vi.mock('next/navigation', () => ({ useSearchParams: () => new URLSearchParams(navigation.query) }));
vi.mock('@/lib/api/chats', () => ({ listChats: vi.fn().mockResolvedValue({ items: [] }) }));
vi.mock('@/lib/api/harnesses', () => ({
  listHarnessTemplates: vi.fn().mockResolvedValue([]),
  listHarnessNodeCatalog: vi.fn().mockResolvedValue([]),
  getHarnessRun: vi.fn().mockResolvedValue({ harness_run_id: 'existing', status: 'waiting_for_review', output_json: {} }),
  listSkillDrafts: vi.fn().mockResolvedValue([{
    skill_draft_id: 'chosen', candidate_name: 'Persisted candidate', description: 'Existing work',
    content_markdown: 'Keep conclusions concise.', status: 'proposed', rules: [],
  }]),
  createSkillifyRun: vi.fn(), decideSkillDraft: vi.fn(), decideSkillRule: vi.fn(), publishSkillDraft: vi.fn(),
}));

it('opens the persisted authoring candidate without creating or publishing another run', async () => {
  render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  expect((await screen.findAllByText('Persisted candidate')).length).toBeGreaterThan(0);
  expect(harness.getHarnessRun).toHaveBeenCalledWith('existing');
  expect(harness.listSkillDrafts).toHaveBeenCalledWith('existing');
  expect(harness.createSkillifyRun).not.toHaveBeenCalled();
  expect(harness.publishSkillDraft).not.toHaveBeenCalled();
});


it('retains the advanced Harness request before dispatch and reuses it after reload', async () => {
  window.localStorage.clear(); vi.clearAllMocks(); navigation.query = '';
  vi.mocked(harness.listHarnessTemplates).mockResolvedValue([{ template_id: 'skillify.v1', name: 'Skillify' }] as Awaited<ReturnType<typeof harness.listHarnessTemplates>>);
  vi.mocked(listChats).mockResolvedValue({ items: [{ id: 'source-session', title: 'Source conversation' }] } as Awaited<ReturnType<typeof listChats>>);
  vi.mocked(harness.createSkillifyRun).mockImplementationOnce(async (request) => {
    const saved = JSON.parse(window.localStorage.getItem('astra:harness:v1:owner:runtime:')!);
    expect(saved.pending).toEqual(request);
    expect(request.idempotency_key).toBeTruthy();
    throw new Error('Generation response lost');
  }).mockResolvedValueOnce({ harness_run_id: 'recovered', status: 'running', output_json: {} } as Awaited<ReturnType<typeof harness.createSkillifyRun>>)
    .mockResolvedValue({ harness_run_id: 'recovered', status: 'waiting_for_review', output_json: {} } as Awaited<ReturnType<typeof harness.createSkillifyRun>>);
  const view = render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
  fireEvent.click(await screen.findByText('Source conversation'));
  fireEvent.click(screen.getByRole('button', { name: 'Run Skillify' }));
  await screen.findByText('Generation response lost');
  const request = vi.mocked(harness.createSkillifyRun).mock.calls[0][0];
  view.unmount();
  render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByRole('button', { name: '恢复原生成任务' }));
  await screen.findByText('原生成任务仍在执行；稍后恢复同一任务以读取结果。');
  expect(JSON.parse(window.localStorage.getItem('astra:harness:v1:owner:runtime:')!).pending).toEqual(request);
  expect(screen.getByRole('button', { name: 'Run Skillify' })).toBeDisabled();
  fireEvent.click(screen.getByRole('button', { name: '恢复原生成任务' }));
  await waitFor(() => expect(harness.listSkillDrafts).toHaveBeenCalledWith('recovered'));
  expect(vi.mocked(harness.createSkillifyRun).mock.calls.map(([body]) => body)).toEqual([request, request, request]);
  expect(JSON.parse(window.localStorage.getItem('astra:harness:v1:owner:runtime:')!)).toEqual({ runId: 'recovered' });
});


it('updates the canonical Harness result link when generation started from an older run', async () => {
  window.localStorage.clear(); vi.clearAllMocks();
  navigation.query = 'runId=old&draftId=old-draft';
  window.history.replaceState(null, '', '/harnesses?runId=old&draftId=old-draft');
  const request = { session_ids: ['session'], idempotency_key: 'retained-key' };
  window.localStorage.setItem('astra:harness:v1:owner:runtime:old', JSON.stringify({ pending: request }));
  vi.mocked(harness.createSkillifyRun).mockResolvedValue({ harness_run_id: 'new', status: 'waiting_for_review', output_json: {} } as Awaited<ReturnType<typeof harness.createSkillifyRun>>);
  const view = render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByRole('button', { name: '恢复原生成任务' }));
  await waitFor(() => expect(harness.listSkillDrafts).toHaveBeenCalledWith('new'));
  expect(window.location.search).toBe('?runId=new');
  view.unmount(); navigation.query = window.location.search.slice(1);
  render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  await waitFor(() => expect(harness.getHarnessRun).toHaveBeenLastCalledWith('new'));
  expect(harness.createSkillifyRun).toHaveBeenCalledTimes(1);
});
