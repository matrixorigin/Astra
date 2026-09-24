'use client';

import { useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { activatePersonalSkill, listAuthoringTargets } from '@/lib/api/harnesses';
import { createSession, listSessions } from '@/lib/api/sessions';
import type { RuntimeSessionListCursor } from '@astra/sdk';
import type { HarnessRun } from '@/lib/api/types';

export function SkillUseAction({ run, skillName, versionId }: { run?: HarnessRun; skillName: string; versionId: string }) {
  const sessions = (run?.input_json.session_ids ?? []) as string[];
  const baseline = (run?.output_json.authoring as { baseline?: { skill_name: string; version_id: string } } | undefined)?.baseline;
  const oldVersion = baseline?.skill_name === skillName ? baseline.version_id : null;
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const [choices, setChoices] = useState<Array<{ id: string; title: string; chatId?: string }>>(sessions.map((id) => ({ id, title: id })));
  const [choosing, setChoosing] = useState(!sessions.length || sessions.length > 1);
  const [nextCursor, setNextCursor] = useState<RuntimeSessionListCursor | null>(null);
  const [sessionId, setSessionId] = useState(sessions.length === 1 ? sessions[0] : '');
  const [expectedVersion, setExpectedVersion] = useState<string | null>(oldVersion);
  const [activeVersion, setActiveVersion] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState('发布不会自动更改当前会话。');

  async function activateVersion(target: string, destination = sessionId, expected = expectedVersion) {
    setBusy(true); setError(null);
    try {
      const adopted = await activatePersonalSkill(skillName, destination, target, expected);
      setExpectedVersion(adopted.version_id); setActiveVersion(adopted.version_id);
      setNotice(`已启用 ${skillName} · ${adopted.version_id}。下一次请求使用此版本，正在执行的请求不变。`);
    } catch (reason) { setError(reason instanceof Error ? reason.message : '无法启用此版本'); }
    finally { setBusy(false); }
  }

  async function refreshVersion() {
    setBusy(true); setError(null);
    try {
      const current = (await listAuthoringTargets(sessionId)).find((target) => target.skill_name === skillName)?.version_id ?? null;
      setExpectedVersion(current); setActiveVersion(current);
      setNotice(current === versionId ? `已确认 ${skillName} · ${versionId} 已启用。` : `当前版本：${current ?? '未启用'}。再次点击使用会明确替换这个版本。`);
    } catch (reason) { setError(reason instanceof Error ? reason.message : '无法读取当前版本'); }
    finally { setBusy(false); }
  }

  async function loadSessions(more = false) {
    setBusy(true); setError(null); setChoosing(true);
    try {
      const page = await listSessions(more ? nextCursor : null);
      if (!mounted.current) return;
      setChoices((current) => [...new Map([...current, ...page.sessions.filter((session) => session.status === 'active' && session.metadata?.source === 'web_v1')
        .map((session) => ({ id: session.session_id, title: session.title || session.session_id, chatId: typeof session.metadata?.web_chat_id === 'string' ? session.metadata.web_chat_id : session.session_id }))].map((entry) => [entry.id, entry])).values()]);
      setNextCursor(page.next_cursor ?? null);
    } catch (reason) { setError(reason instanceof Error ? reason.message : '无法读取会话'); }
    finally { setBusy(false); }
  }

  async function createAndActivateSession() {
    setBusy(true); setError(null);
    try {
      const session = await createSession(`使用 ${skillName}`);
      if (!mounted.current) return;
      setChoices((current) => [...current, { id: session.session_id, title: session.title || session.session_id, chatId: session.session_id }]);
      setChoosing(true);
      setSessionId(session.session_id); setExpectedVersion(null); setActiveVersion(null);
      await activateVersion(versionId, session.session_id, null);
    } catch (reason) { setError(reason instanceof Error ? reason.message : '无法创建会话'); }
    finally { setBusy(false); }
  }

  const chatId = choices.find((choice) => choice.id === sessionId)?.chatId;
  return <div className="mt-3 space-y-2 text-sm">
    {choosing ? <label>选择使用此 Skill 的会话
      <select aria-label="启用 Skill 的会话" value={sessionId} disabled={busy} onChange={(event) => {
        setSessionId(event.target.value); setExpectedVersion(null); setActiveVersion(null); setError(null); setNotice('尚未在所选会话启用此版本。');
      }}><option value="">请选择</option>{choices.map((entry) => <option key={entry.id} value={entry.id}>{entry.title}</option>)}</select>
    </label> : null}
    <div className="flex gap-2">
      <Button variant="ghost" disabled={busy} onClick={() => void loadSessions()}>选择其他会话</Button>
      {nextCursor ? <Button variant="ghost" disabled={busy} onClick={() => void loadSessions(true)}>更多会话</Button> : null}
      <Button variant="ghost" disabled={busy} onClick={() => void createAndActivateSession()}>在新会话使用此版本</Button>
    </div>
    <p className="text-xs text-text-muted">{notice}</p>
    {error ? <p role="alert" className="text-danger">{error} {sessionId ? <button disabled={busy} onClick={() => void refreshVersion()}>读取当前版本后重选</button> : null}</p> : null}
    <div className="flex flex-wrap gap-2">
      <Button disabled={busy || !sessionId || activeVersion === versionId} onClick={() => void activateVersion(versionId)}>在此会话使用此版本</Button>
      {oldVersion && sessions.includes(sessionId) && activeVersion === versionId ? <Button variant="ghost" disabled={busy} onClick={() => void activateVersion(oldVersion)}>切回原版本</Button> : null}
      {activeVersion && chatId ? <Button variant="ghost" href={`/chats/${encodeURIComponent(chatId)}`}>返回会话</Button> : null}
    </div>
  </div>;
}
