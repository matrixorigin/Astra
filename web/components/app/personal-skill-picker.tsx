'use client';

import { useEffect, useState } from 'react';
import { listPersonalSkillSources, listPersonalSkillVersions, type PersonalSkillSource, type PersonalSkillVersion } from '@/lib/api/evaluations';
import { SkillUseAction } from '@/components/app/skill-use-action';
import { Button } from '@/components/ui/button';

export function PersonalSkillPicker({ onBack }: { onBack: () => void }) {
  const [prefix, setPrefix] = useState('');
  const [search, setSearch] = useState({ prefix: '' });
  const [sources, setSources] = useState<PersonalSkillSource[]>([]);
  const [skillName, setSkillName] = useState('');
  const [versions, setVersions] = useState<PersonalSkillVersion[]>([]);
  const [versionId, setVersionId] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    setBusy(true); setError(null); setSkillName(''); setVersions([]); setVersionId('');
    listPersonalSkillSources(search.prefix).then((items) => { if (active) setSources(items); })
      .catch((reason) => { if (active) setError(reason instanceof Error ? reason.message : '无法读取我的 Skill'); })
      .finally(() => { if (active) setBusy(false); });
    return () => { active = false; };
  }, [search]);
  useEffect(() => {
    if (!skillName) return;
    let active = true;
    setBusy(true); setError(null); setVersions([]); setVersionId('');
    listPersonalSkillVersions(skillName).then((items) => {
      if (active) setVersions(items.filter((version) => version.status === 'published'));
    }).catch((reason) => { if (active) setError(reason instanceof Error ? reason.message : '无法读取版本'); })
      .finally(() => { if (active) setBusy(false); });
    return () => { active = false; };
  }, [skillName]);
  return <div className="w-96 max-w-[calc(100vw-2rem)] space-y-3 p-2">
    <Button variant="ghost" onClick={onBack}>返回本轮 Skill 选择</Button>
    <p className="font-medium">我的 Skill</p>
    <p className="text-xs text-text-muted">选择已发布版本并明确启用到会话；不会自动替换已有版本。</p>
    <form onSubmit={(event) => { event.preventDefault(); event.stopPropagation(); setSearch({ prefix: prefix.trim() }); }} className="flex gap-2">
      <input aria-label="按名称前缀查找我的 Skill" placeholder="名称前缀" value={prefix} onChange={(event) => setPrefix(event.target.value)} className="min-w-0 border p-2" />
      <Button type="submit" disabled={busy}>查找</Button>
    </form>
    <select aria-label="我的 Skill" value={skillName} disabled={busy} onChange={(event) => setSkillName(event.target.value)} className="w-full border p-2">
      <option value="">选择 Skill</option>
      {sources.map((source) => <option key={source.source_id} value={source.skill_name}>{source.skill_name}</option>)}
    </select>
    {sources.length === 100 ? <p className="text-xs">显示前 100 个结果，可按名称前缀缩小范围。</p> : null}
    {!busy && !sources.length ? <p>没有匹配的私有 Skill。</p> : null}
    {skillName ? <select aria-label="已发布的 Skill 版本" value={versionId} disabled={busy} onChange={(event) => setVersionId(event.target.value)} className="w-full border p-2">
      <option value="">选择已发布版本</option>
      {versions.map((version) => <option key={version.version_id} value={version.version_id}>{version.version} · {version.version_id}</option>)}
    </select> : null}
    {!busy && skillName && !versions.length ? <p>此 Skill 尚无已发布版本。</p> : null}
    {busy ? <p>正在读取…</p> : null}
    {error ? <p role="alert">{error}</p> : null}
    {versionId ? <SkillUseAction key={`${skillName}:${versionId}`} skillName={skillName} versionId={versionId} /> : null}
  </div>;
}
