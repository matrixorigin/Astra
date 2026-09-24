import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { AuthoringPage } from '@/components/app/authoring-page';
import { SkillUseAction } from '@/components/app/skill-use-action';
import { createAuthoringIntent, listAuthoringTargets, loadAuthoringResult, decideSkillDraft, publishSkillDraft, activatePersonalSkill } from '@/lib/api/harnesses';
import { createSession, listSessions } from '@/lib/api/sessions';
import { runPreparedEvaluation, getEvaluationExperiment } from '@/lib/api/evaluations';
import type { AuthoringIntentRecord } from '@/lib/api/types';

const navigation = vi.hoisted(() => ({ query: 'sessionId=session', defer: false }));
vi.mock('next/navigation', () => ({ useSearchParams: () => new URLSearchParams(navigation.query) }));
vi.mock('@/lib/api/harnesses', () => ({ createAuthoringIntent: vi.fn(), listAuthoringTargets: vi.fn(), loadAuthoringResult: vi.fn(), decideSkillDraft: vi.fn(), publishSkillDraft: vi.fn(), activatePersonalSkill: vi.fn() }));
vi.mock('@/lib/api/sessions', () => ({ createSession: vi.fn(), listSessions: vi.fn() }));
vi.mock('@/lib/api/evaluations', () => ({ runPreparedEvaluation: vi.fn(), getEvaluationExperiment: vi.fn() }));

const record = {
  goal: 'Create a review skill',
  harness_run: { harness_run_id: 'frozen', input_json: { session_ids: ['session'], source_packets: [
    { source_id: 'task', event_type: 'user_query', title: 'Original task', content: 'Return a JSON verdict.' },
    { source_id: 'guidance', event_type: 'user_message', title: 'Guidance', content: 'Please hurry.' },
  ] }, output_json: { authoring: { request: { goal: 'Create a review skill', create_new: false, idempotency_key: '00000000-0000-4000-8000-000000000001' } } } },
  skill_drafts: [{ skill_draft_id: 'draft', revision: 1, candidate_name: 'Review', description: 'Review carefully',
    content_markdown: '# Review\nPreserve examples.', rules: [{ skill_rule_id: 'rule', statement: 'Return JSON',
      rationale: 'The user requested a structured verdict.', citations: [{ citation_id: 'quote', source_id: 'task',
        source_locator_json: { validation: 'exact_source_match', start_byte: 0, end_byte: 22 },
        source_metadata_json: { evidence_kind: 'user_statement' }, evidence_text_preview: 'Return a JSON verdict.',
      }] }] }],
  evaluation: { status: 'unavailable', reason: 'No case' },
  inference: { providers: [], usage_status: 'unavailable', estimated_cost_usd: null },
} as unknown as AuthoringIntentRecord;

beforeEach(() => {
  vi.restoreAllMocks(); vi.resetAllMocks(); window.localStorage.clear();
  navigation.query = 'sessionId=session'; navigation.defer = false;
  window.history.replaceState(null, '', '/authoring?sessionId=session');
  const replaceState = window.history.replaceState.bind(window.history);
  vi.spyOn(window.history, 'replaceState').mockImplementation((data, unused, url) => {
    replaceState(data, unused, url);
    if (!navigation.defer) navigation.query = window.location.search.slice(1);
  });
  vi.spyOn(crypto, 'randomUUID').mockReturnValue('00000000-0000-4000-8000-000000000001');
  vi.mocked(listAuthoringTargets).mockResolvedValue([]);
  vi.mocked(getEvaluationExperiment).mockResolvedValue({ experiment: { experiment_id: 'comparison', spec_fingerprint: 'frozen' }, trials: [] } as unknown as Awaited<ReturnType<typeof getEvaluationExperiment>>);
});

it('opens the standard tool result link through authorized durable reads without generating again', async () => {
  navigation.query = 'runId=tool-run';
  vi.mocked(loadAuthoringResult).mockResolvedValue(record);
  render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  await screen.findByText('Review carefully');
  expect(loadAuthoringResult).toHaveBeenCalledWith('tool-run');
  expect(createAuthoringIntent).not.toHaveBeenCalled();
  expect(runPreparedEvaluation).not.toHaveBeenCalled();
  vi.mocked(createAuthoringIntent).mockResolvedValue(record);
  fireEvent.change(screen.getByLabelText('验证任务'), { target: { value: 'task' } });
  fireEvent.change(screen.getByLabelText('预期 JSON 结果'), { target: { value: '{"ok":true}' } });
  fireEvent.click(screen.getByRole('button', { name: '用这个任务验证' }));
  await waitFor(() => expect(createAuthoringIntent).toHaveBeenCalledWith(expect.objectContaining({
    idempotency_key: '00000000-0000-4000-8000-000000000001', validation_task: { source_id: 'task', expected_result: { ok: true } },
  }), 'session'));
});

it('requires explicit confirmation after an activation conflict and uses CAS when returning to the baseline', async () => {
  const run = { ...record.harness_run, input_json: { session_ids: ['session'] },
    output_json: { authoring: { baseline: { skill_name: 'review', version_id: 'old' } } } };
  vi.mocked(activatePersonalSkill).mockRejectedValueOnce(new Error('Active version changed'))
    .mockResolvedValueOnce({ version_id: 'published', content_hash: 'hash' })
    .mockResolvedValueOnce({ version_id: 'old', content_hash: 'old-hash' });
  vi.mocked(listAuthoringTargets).mockResolvedValue([{ skill_name: 'review', version_id: 'concurrent' }]);
  render(<SkillUseAction run={run} skillName="review" versionId="published" />);
  fireEvent.click(screen.getByRole('button', { name: '在此会话使用此版本' }));
  await screen.findByRole('alert');
  expect(activatePersonalSkill).toHaveBeenLastCalledWith('review', 'session', 'published', 'old');
  fireEvent.click(screen.getByRole('button', { name: '读取当前版本后重选' }));
  await screen.findByText(/当前版本：concurrent/);
  expect(activatePersonalSkill).toHaveBeenCalledTimes(1);
  fireEvent.click(screen.getByRole('button', { name: '在此会话使用此版本' }));
  fireEvent.click(await screen.findByRole('button', { name: '切回原版本' }));
  await waitFor(() => expect(activatePersonalSkill).toHaveBeenLastCalledWith('review', 'session', 'old', 'published'));
  expect(activatePersonalSkill).toHaveBeenNthCalledWith(2, 'review', 'session', 'published', 'concurrent');
});

async function submit() {
  render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Create a review skill' } });
  await waitFor(() => expect(screen.getByRole('button', { name: '生成结果' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  await screen.findByText('Review carefully');
}

it('shows candidate and source evidence while evaluation is still running', async () => {
  vi.mocked(createAuthoringIntent).mockResolvedValue({ ...record,
    evaluation_plan: { experiment: { experiment_id: 'comparison' }, trials: [] } as unknown as NonNullable<AuthoringIntentRecord['evaluation_plan']> });
  let reject!: (reason: Error) => void;
  vi.mocked(runPreparedEvaluation).mockReturnValue(new Promise((_, fail) => { reject = fail; }));
  await submit();
  expect(screen.getByText('正在验证：候选与依据已可查看')).toBeInTheDocument();
  expect(screen.getByText('已匹配冻结原文')).toBeInTheDocument();
  expect(screen.getByText(/用户陈述或偏好/)).toBeInTheDocument();
  expect(screen.getByText('查看原文')).toBeInTheDocument();
  expect(screen.getByRole('link', { name: '审阅并发布此 Skill' })).toHaveAttribute('href', '/harnesses?runId=frozen&draftId=draft');
  await act(async () => reject(new Error('Trial unavailable')));
  expect(screen.getByText(/Trial unavailable/)).toBeInTheDocument();
  expect(screen.getByText('Review carefully')).toBeInTheDocument();
});

it('does not launch evaluation from an authoring response after the owner changes', async () => {
  let resolve!: (record: AuthoringIntentRecord) => void;
  vi.mocked(createAuthoringIntent).mockReturnValue(new Promise((done) => { resolve = done; }));
  const view = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Create a review skill' } });
  await waitFor(() => expect(screen.getByRole('button', { name: '生成结果' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  view.rerender(<AuthoringPage ownerId="other-owner" runtimeKey="runtime" />);
  await act(async () => resolve({ ...record, evaluation_plan: {
    experiment: { experiment_id: 'comparison' }, trials: [],
  } as unknown as NonNullable<AuthoringIntentRecord['evaluation_plan']> }));
  expect(runPreparedEvaluation).not.toHaveBeenCalled();
  expect(getEvaluationExperiment).not.toHaveBeenCalled();
  expect(screen.queryByText('Review carefully')).not.toBeInTheDocument();
  expect(window.localStorage.length).toBe(1);
  expect(window.localStorage.key(0)).toContain('owner:runtime');
});

it('reuses the candidate when selecting an original task and explicit expected result', async () => {
  vi.mocked(createAuthoringIntent).mockResolvedValue(record);
  await submit();
  expect(screen.queryByRole('option', { name: 'Please hurry.' })).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText('验证任务'), { target: { value: 'task' } });
  fireEvent.change(screen.getByLabelText('预期 JSON 结果'), { target: { value: '{"ok":true}' } });
  await act(async () => fireEvent.click(screen.getByRole('button', { name: '用这个任务验证' })));
  const requests = vi.mocked(createAuthoringIntent).mock.calls;
  expect(requests).toHaveLength(2);
  expect(requests[1][0]).toEqual({ ...requests[0][0], validation_task: { source_id: 'task', expected_result: { ok: true } } });
});

it('asks for an ambiguous active Skill and sends its frozen identity', async () => {
  const targets = [{ skill_name: 'review', version_id: 'v-review' }, { skill_name: 'deploy', version_id: 'v-deploy' }];
  vi.mocked(listAuthoringTargets).mockResolvedValue(targets);
  vi.mocked(createAuthoringIntent).mockResolvedValue(record);
  render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Improve my skill' } });
  const selection = await screen.findByLabelText('要优化的 Skill');
  expect(screen.getByRole('button', { name: '生成结果' })).toBeDisabled();
  expect(createAuthoringIntent).not.toHaveBeenCalled();
  fireEvent.change(selection, { target: { value: 'v-review' } });
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  await screen.findByText('Review carefully');
  expect(createAuthoringIntent).toHaveBeenCalledWith(expect.objectContaining({ target_skill: targets[0] }), 'session');
});

it('resumes the persisted comparison after a lost response and reload without regenerating', async () => {
  const prepared = { ...record, evaluation_plan: { experiment: { experiment_id: 'comparison', spec_fingerprint: 'frozen' },
    trials: [], adapter_profile_version: 'test' } };
  vi.mocked(createAuthoringIntent).mockResolvedValue(prepared);
  vi.mocked(loadAuthoringResult).mockResolvedValue(prepared);
  vi.mocked(runPreparedEvaluation).mockRejectedValueOnce(new Error('Polling response lost'));
  const view = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Improve review' } });
  await waitFor(() => expect(screen.getByRole('button', { name: '生成结果' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  await screen.findByText(/Polling response lost/);
  view.unmount();
  const recovered = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  await screen.findByText('Review carefully');
  expect(loadAuthoringResult).toHaveBeenCalledWith('frozen');
  expect(createAuthoringIntent).toHaveBeenCalledTimes(1);
  expect(runPreparedEvaluation).toHaveBeenCalledTimes(1);
  const binding = { trial_id: 'trial', binding_status: 'bound', session_id: 'trial-session', run_id: 'trial-run',
    trial: { sequence: 0, case_id: 'case', arm: 'candidate', repetition: 0 } };
  vi.mocked(getEvaluationExperiment).mockResolvedValue({ experiment: prepared.evaluation_plan.experiment,
    trials: [{ binding, lifecycle: 'observed', task_assessment: { outcome: { status: 'pass' } } }] } as unknown as Awaited<ReturnType<typeof getEvaluationExperiment>>);
  vi.mocked(runPreparedEvaluation).mockResolvedValue({ report: { conclusion: 'One task passed', causal_strength: 'unknown' },
    manifest: { coverage: { evidence_incomplete: false } }, markdown: 'bounded observation' } as unknown as Awaited<ReturnType<typeof runPreparedEvaluation>>);
  fireEvent.click(screen.getByRole('button', { name: '恢复已有结果与评估' }));
  await screen.findByText('One task passed');
  expect(runPreparedEvaluation).toHaveBeenLastCalledWith(expect.objectContaining({ trials: [binding] }), expect.anything());
  expect(createAuthoringIntent).toHaveBeenCalledTimes(1);
  recovered.unmount();
  navigation.query = 'sessionId=session';
  render(<AuthoringPage ownerId="another-owner" runtimeKey="runtime" />);
  await waitFor(() => expect(screen.queryByText('正在恢复已保存的结果…')).not.toBeInTheDocument());
  expect(screen.queryByText('Review carefully')).not.toBeInTheDocument();
  expect(loadAuthoringResult).toHaveBeenCalledTimes(2);
});

it('reviews and privately publishes the exact candidate before explicitly adopting it', async () => {
  const owned = { ...record, harness_run: { ...record.harness_run,
    input_json: { ...record.harness_run.input_json, session_ids: ['session'], source_coverage: { selected_event_count: 2000, older_events_omitted: true } } } };
  vi.mocked(createAuthoringIntent).mockResolvedValue(owned);
  vi.mocked(decideSkillDraft).mockResolvedValue(owned.skill_drafts[0]);
  vi.mocked(publishSkillDraft).mockResolvedValue({ version_id: 'published' } as unknown as Awaited<ReturnType<typeof publishSkillDraft>>);
  vi.mocked(loadAuthoringResult).mockResolvedValue({ ...owned, skill_drafts: [{ ...owned.skill_drafts[0], published_version_id: 'published' }] });
  vi.mocked(activatePersonalSkill).mockResolvedValue({ version_id: 'published', content_hash: 'hash' });
  await submit();
  expect(screen.getByText(/较早记录已省略/)).toBeInTheDocument();
  fireEvent.click(screen.getByRole('button', { name: '我已审核，保存为私有 Skill' }));
  const use = await screen.findByRole('button', { name: '在此会话使用此版本' });
  expect(decideSkillDraft).toHaveBeenCalledWith('frozen', 'draft', expect.objectContaining({ decision: 'approve' }));
  expect(publishSkillDraft).toHaveBeenCalledWith('frozen', 'draft', { expected_revision: 1, visibility: 'private' });
  expect(activatePersonalSkill).not.toHaveBeenCalled();
  fireEvent.click(use);
  await screen.findByText(/已启用 Review/);
  expect(activatePersonalSkill).toHaveBeenCalledWith('Review', 'session', 'published', null);
});

it('does not publish after the owner changes while approval is pending', async () => {
  vi.mocked(createAuthoringIntent).mockResolvedValue(record);
  let approve!: (draft: AuthoringIntentRecord['skill_drafts'][number]) => void;
  vi.mocked(decideSkillDraft).mockReturnValue(new Promise((done) => { approve = done; }));
  const view = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Create a review skill' } });
  await waitFor(() => expect(screen.getByRole('button', { name: '生成结果' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  fireEvent.click(await screen.findByRole('button', { name: '我已审核，保存为私有 Skill' }));
  view.rerender(<AuthoringPage ownerId="other-owner" runtimeKey="runtime" />);
  await act(async () => approve(record.skill_drafts[0]));
  expect(publishSkillDraft).not.toHaveBeenCalled();
});


it('persists the original submission before dispatch and retries a lost initial response with the same key after reload', async () => {
  vi.mocked(createAuthoringIntent).mockImplementationOnce(async (request) => {
    expect(JSON.parse(window.localStorage.getItem(window.localStorage.key(0)!)!).pending.request).toEqual(request);
    throw new Error('Initial response lost');
  }).mockRejectedValueOnce(new Error('Original run is still running')).mockResolvedValue(record);
  const view = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Create a review skill' } });
  await waitFor(() => expect(screen.getByRole('button', { name: '生成结果' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  await screen.findByText('Initial response lost');
  const original = vi.mocked(createAuthoringIntent).mock.calls[0];
  view.unmount();
  render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  const retry = await screen.findByRole('button', { name: '重试原任务' });
  expect(screen.getByLabelText('Authoring goal')).toBeDisabled();
  expect(createAuthoringIntent).toHaveBeenCalledTimes(1);
  fireEvent.click(retry);
  await screen.findByText('Original run is still running');
  fireEvent.click(screen.getByRole('button', { name: '恢复已有结果与评估' }));
  await screen.findByText('Review carefully');
  expect(vi.mocked(createAuthoringIntent).mock.calls).toEqual([original, original, original]);
  expect(crypto.randomUUID).toHaveBeenCalledTimes(1);
});

it('recovers the original request when the page leaves before generation completes', async () => {
  let finish!: (value: AuthoringIntentRecord) => void;
  vi.mocked(createAuthoringIntent).mockReturnValueOnce(new Promise((resolve) => { finish = resolve; })).mockResolvedValue(record);
  const view = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Create a review skill' } });
  await waitFor(() => expect(screen.getByRole('button', { name: '生成结果' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  view.unmount();
  await act(async () => finish(record));
  expect(runPreparedEvaluation).not.toHaveBeenCalled();
  render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByRole('button', { name: '重试原任务' }));
  await screen.findByText('Review carefully');
  expect(vi.mocked(createAuthoringIntent).mock.calls[1]).toEqual(vi.mocked(createAuthoringIntent).mock.calls[0]);
});

it('does not dispatch generation when the browser cannot retain the recovery request', async () => {
  const storage = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('Storage full'); });
  render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Create a review skill' } });
  await waitFor(() => expect(screen.getByRole('button', { name: '生成结果' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  await screen.findByText('Storage full');
  expect(createAuthoringIntent).not.toHaveBeenCalled();
  storage.mockRestore();
});

it('uses a standalone published Skill in an explicitly selected session with CAS', async () => {
  vi.mocked(listSessions).mockResolvedValue({ sessions: [
    { session_id: 'other', title: 'My task', status: 'active', created_at: '', metadata: { source: 'web_v1', web_chat_id: 'web-other' } },
    { session_id: 'closed', title: 'Ended task', status: 'ended', created_at: '' },
  ], next_cursor: null });
  vi.mocked(activatePersonalSkill).mockResolvedValue({ version_id: 'published', content_hash: 'hash' });
  render(<SkillUseAction skillName="review" versionId="published" />);
  expect(listSessions).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: '选择其他会话' }));
  await screen.findByRole('option', { name: 'My task' });
  expect(screen.queryByRole('option', { name: 'Ended task' })).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText('启用 Skill 的会话'), { target: { value: 'other' } });
  expect(activatePersonalSkill).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: '在此会话使用此版本' }));
  await screen.findByText(/已启用 review/);
  expect(activatePersonalSkill).toHaveBeenCalledWith('review', 'other', 'published', null);
  expect(screen.getByRole('link', { name: '返回会话' })).toHaveAttribute('href', '/chats/web-other');
});

it('creates an empty session before explicit activation and retries activation in that same session', async () => {
  vi.mocked(createSession).mockResolvedValue({ session_id: 'new-session', status: 'active', created_at: '' });
  vi.mocked(activatePersonalSkill).mockRejectedValueOnce(new Error('Activation response lost')).mockResolvedValue({ version_id: 'published', content_hash: 'hash' });
  render(<SkillUseAction skillName="review" versionId="published" />);
  fireEvent.click(screen.getByRole('button', { name: '在新会话使用此版本' }));
  await screen.findByRole('alert');
  expect(createSession).toHaveBeenCalledTimes(1);
  fireEvent.click(screen.getByRole('button', { name: '在此会话使用此版本' }));
  await screen.findByText(/已启用 review/);
  expect(createSession).toHaveBeenCalledTimes(1);
  expect(activatePersonalSkill).toHaveBeenLastCalledWith('review', 'new-session', 'published', null);
});

it('does not activate a newly created session after the owner leaves the page', async () => {
  let finish!: (session: Awaited<ReturnType<typeof createSession>>) => void;
  vi.mocked(createSession).mockReturnValue(new Promise((resolve) => { finish = resolve; }));
  const view = render(<SkillUseAction skillName="review" versionId="published" />);
  fireEvent.click(screen.getByRole('button', { name: '在新会话使用此版本' }));
  view.unmount();
  await act(async () => finish({ session_id: 'new-session', created_at: '' }));
  expect(activatePersonalSkill).not.toHaveBeenCalled();
});


it.each([false, true])('preserves the source and pinned improvement target from a result link (explicit=%s)', async (explicit) => {
  navigation.query = 'runId=old-run';
  window.history.replaceState(null, '', '/authoring?runId=old-run');
  const target = { skill_name: 'review', version_id: 'old-version' };
  const previous = { ...record, harness_run: { ...record.harness_run, harness_run_id: 'old-run',
    output_json: { authoring: { baseline: target, request: { goal: 'Improve review', create_new: false,
      idempotency_key: 'old-key', ...(explicit ? { target_skill: target } : {}) } } } } };
  const next = { ...previous, harness_run: { ...previous.harness_run, harness_run_id: 'new-run' },
    skill_drafts: [{ ...record.skill_drafts[0], description: 'New candidate evidence' }] };
  vi.mocked(loadAuthoringResult).mockImplementation(async (id) => id === 'new-run' ? next : previous);
  vi.mocked(createAuthoringIntent).mockResolvedValue(next);
  const view = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  await screen.findByText('Review carefully');
  fireEvent.click(screen.getByRole('button', { name: '另生成一个候选' }));
  await screen.findByText('New candidate evidence');
  expect(createAuthoringIntent).toHaveBeenCalledWith(expect.objectContaining({
    create_new: false, target_skill: target,
  }), 'session');
  expect(new URL(window.location.href).searchParams.get('runId')).toBe('new-run');
  view.unmount();
  render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  await screen.findByText('New candidate evidence');
  expect(loadAuthoringResult).toHaveBeenLastCalledWith('new-run');
  expect(createAuthoringIntent).toHaveBeenCalledTimes(1);
});


it('keeps evaluation mounted while Next defers the canonical query update', async () => {
  navigation.defer = true;
  vi.mocked(createAuthoringIntent).mockResolvedValue({ ...record,
    evaluation_plan: { experiment: { experiment_id: 'comparison' }, trials: [] } as unknown as NonNullable<AuthoringIntentRecord['evaluation_plan']> });
  vi.mocked(runPreparedEvaluation).mockReturnValue(new Promise(() => {}));
  const view = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Improve review' } });
  await waitFor(() => expect(screen.getByRole('button', { name: '生成结果' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  await screen.findByText('正在验证：候选与依据已可查看');
  const options = vi.mocked(runPreparedEvaluation).mock.calls[0][1];
  expect(options?.signal?.aborted).toBe(false);
  navigation.query = window.location.search.slice(1);
  view.rerender(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  expect(screen.getByText('正在验证：候选与依据已可查看')).toBeInTheDocument();
  expect(options?.signal?.aborted).toBe(false);
  expect(loadAuthoringResult).not.toHaveBeenCalled();
});

it('preserves the original operation identity when validating an auto-targeted candidate', async () => {
  navigation.query = 'runId=auto-run';
  const original = record.harness_run.output_json.authoring as { request: { goal: string; idempotency_key: string } };
  const auto = { ...record, harness_run: { ...record.harness_run, output_json: {
    authoring: { ...original, baseline: { skill_name: 'review', version_id: 'baseline' } },
  } } };
  vi.mocked(loadAuthoringResult).mockResolvedValue(auto);
  vi.mocked(createAuthoringIntent).mockResolvedValue(auto);
  render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  await screen.findByText('Review carefully');
  fireEvent.change(screen.getByLabelText('验证任务'), { target: { value: 'task' } });
  fireEvent.change(screen.getByLabelText('预期 JSON 结果'), { target: { value: '{"ok":true}' } });
  fireEvent.click(screen.getByRole('button', { name: '用这个任务验证' }));
  await waitFor(() => expect(createAuthoringIntent).toHaveBeenCalledWith({ ...original.request,
    validation_task: { source_id: 'task', expected_result: { ok: true } },
  }, 'session'));
});


it('clears the candidate when external navigation removes the result query', async () => {
  navigation.query = 'runId=old-run';
  vi.mocked(loadAuthoringResult).mockResolvedValue(record);
  const view = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  await screen.findByText('Review carefully');
  navigation.query = '';
  view.rerender(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  await waitFor(() => expect(screen.queryByText('正在恢复已保存的结果…')).not.toBeInTheDocument());
  expect(screen.getByLabelText('Authoring goal')).toHaveValue('');
  expect(screen.queryByText('Review carefully')).not.toBeInTheDocument();
});

it('loads the canonical result under the new runtime after generating another candidate', async () => {
  window.history.replaceState(null, '', '/authoring?runId=old-run');
  vi.mocked(loadAuthoringResult).mockResolvedValue(record);
  vi.mocked(createAuthoringIntent).mockResolvedValue({ ...record,
    harness_run: { ...record.harness_run, harness_run_id: 'new-run' } });
  const view = render(<AuthoringPage ownerId="owner" runtimeKey="runtime" />);
  await screen.findByText('Review carefully');
  fireEvent.click(screen.getByRole('button', { name: '另生成一个候选' }));
  await waitFor(() => expect(new URL(window.location.href).searchParams.get('runId')).toBe('new-run'));
  view.rerender(<AuthoringPage ownerId="owner" runtimeKey="other-runtime" />);
  await waitFor(() => expect(loadAuthoringResult).toHaveBeenLastCalledWith('new-run'));
});
