import { createPortal } from 'react-dom';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { SkillPickerPanel } from '@/components/app/skill-picker-panel';
import { listPersonalSkillSources, listPersonalSkillVersions } from '@/lib/api/evaluations';
import { activatePersonalSkill } from '@/lib/api/harnesses';
import { listSessions } from '@/lib/api/sessions';

vi.mock('@/hooks/use-skill-catalog', () => ({ useSkillCatalog: () => ({ items: [], nextCursor: null, loading: false, error: null, loadInitial: () => {}, loadNextPage: () => {} }) }));
vi.mock('@/lib/api/evaluations', () => ({ listPersonalSkillSources: vi.fn(), listPersonalSkillVersions: vi.fn() }));
vi.mock('@/lib/api/harnesses', () => ({ activatePersonalSkill: vi.fn(), listAuthoringTargets: vi.fn() }));
vi.mock('@/lib/api/sessions', () => ({ listSessions: vi.fn(), createSession: vi.fn() }));

it('discovers a private published revision from the ordinary picker and explicitly activates it without per-turn name selection', async () => {
  vi.mocked(listPersonalSkillSources).mockResolvedValue([{ source_id: 'source', skill_name: 'review', status: 'active', visibility: 'private' }]);
  vi.mocked(listPersonalSkillVersions).mockResolvedValue([
    { version_id: 'published', skill_name: 'review', version: 'v1', content_hash: 'hash', status: 'published', token_estimate: 12, created_at: '' },
    { version_id: 'draft', skill_name: 'review', version: 'v2', content_hash: 'hash2', status: 'draft', token_estimate: 12, created_at: '' },
  ]);
  vi.mocked(listSessions).mockResolvedValue({ sessions: [{ session_id: 'session', title: 'My task', status: 'active', created_at: '', metadata: { source: 'web_v1' } }] });
  vi.mocked(activatePersonalSkill).mockResolvedValue({ version_id: 'published', content_hash: 'hash' });
  const onChange = vi.fn();
  const onChatSubmit = vi.fn();
  render(<form onSubmit={(event) => { event.preventDefault(); onChatSubmit(); }}>
    {createPortal(<SkillPickerPanel selected={[]} onChange={onChange} onBack={() => {}} />, document.body)}
  </form>);
  expect(listPersonalSkillSources).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: '我的 Skill：选择已发布版本并启用到会话' }));
  await screen.findByRole('option', { name: 'review' });
  expect(listPersonalSkillVersions).not.toHaveBeenCalled();
  fireEvent.change(screen.getByLabelText('按名称前缀查找我的 Skill'), { target: { value: 'review' } });
  fireEvent.click(screen.getByRole('button', { name: '查找' }));
  await waitFor(() => expect(listPersonalSkillSources).toHaveBeenCalledTimes(2));
  await waitFor(() => expect(screen.getByRole('button', { name: '查找' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: '查找' }));
  await waitFor(() => expect(listPersonalSkillSources).toHaveBeenCalledTimes(3));
  await waitFor(() => expect(screen.getByLabelText('我的 Skill')).toBeEnabled());
  expect(onChatSubmit).not.toHaveBeenCalled();
  fireEvent.change(screen.getByLabelText('我的 Skill'), { target: { value: 'review' } });
  await screen.findByRole('option', { name: 'v1 · published' });
  expect(screen.queryByRole('option', { name: 'v2 · draft' })).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText('已发布的 Skill 版本'), { target: { value: 'published' } });
  fireEvent.click(screen.getByRole('button', { name: '选择其他会话' }));
  await screen.findByRole('option', { name: 'My task' });
  fireEvent.change(screen.getByLabelText('启用 Skill 的会话'), { target: { value: 'session' } });
  expect(activatePersonalSkill).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: '在此会话使用此版本' }));
  await waitFor(() => expect(activatePersonalSkill).toHaveBeenCalledWith('review', 'session', 'published', null));
  expect(onChange).not.toHaveBeenCalled();
  expect(listPersonalSkillVersions).toHaveBeenCalledTimes(1);
});
