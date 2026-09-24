'use client';

import type { HarnessCitation, HarnessRun, HarnessSkillRule } from '@/lib/api/types';

export type FrozenSkillSource = {
  source_id: string; event_id: string; session_id: string;
  title: string; event_type: string; content: string;
};

export function frozenSkillSources(run: HarnessRun): FrozenSkillSource[] {
  return Array.isArray(run.input_json?.source_packets) ? run.input_json.source_packets as FrozenSkillSource[] : [];
}

export function RuleEvidence({ citations, sources }: { citations: HarnessCitation[]; sources: FrozenSkillSource[] }) {
  return <div className="space-y-2">{citations.map((citation) => {
    const source = sources.find((entry) => entry.source_id === citation.source_id);
    const locator = citation.source_locator_json;
    const verified = locator?.validation === 'exact_source_match' && source;
    const excerpt = citation.evidence_text_preview ?? '';
    const position = source?.content.indexOf(excerpt) ?? -1;
    const kind = citation.source_metadata_json?.evidence_kind;
    const label = kind === 'user_goal' ? '用户目标（不代表效果已验证）'
      : kind === 'user_statement' ? '用户陈述或偏好（不代表效果已验证）'
      : kind === 'execution_result' ? '执行结果（仍需结合任务验证）' : '来源材料';
    return <div key={citation.citation_id} className="rounded-control border border-border p-3 text-xs">
      <p className="font-medium">{source?.title ?? citation.source_id} · {label}</p>
      <p className="mt-1 text-text-muted">{verified ? '已匹配冻结原文' : '引用尚未完成原文验证'}</p>
      <blockquote className="mt-2 whitespace-pre-wrap">{citation.evidence_text_preview}</blockquote>
      {source ? <details className="mt-2">
        <summary className="cursor-pointer">查看原文</summary>
        <pre className="mt-2 max-h-72 overflow-auto whitespace-pre-wrap">{verified && position >= 0 ? <>
          {source.content.slice(0, position)}<mark>{excerpt}</mark>{source.content.slice(position + excerpt.length)}
        </> : source.content}</pre>
      </details> : null}
    </div>;
  })}</div>;
}

export function SkillEvidence({ rules, sources }: { rules: HarnessSkillRule[]; sources: FrozenSkillSource[] }) {
  return <details className="mt-4">
    <summary className="cursor-pointer text-sm">生成依据：能力、原因与原文</summary>
    <div className="mt-3 space-y-4">{rules.map((rule) => <section key={rule.skill_rule_id}>
      <h3 className="text-sm font-medium">{rule.statement}</h3>
      <p className="my-2 text-xs text-text-secondary">{rule.rationale}</p>
      <RuleEvidence citations={rule.citations} sources={sources} />
    </section>)}</div>
  </details>;
}

export function SkillContentComparison({ before, after }: { before: string; after: string }) {
  return <details className="my-3 text-xs" open={before !== after}>
    <summary className="cursor-pointer">{before === after ? '正文未改变' : '正文已改变：查看修改前后'}</summary>
    <div className="mt-2 grid gap-3 md:grid-cols-2">
      <div><p>修改前</p><pre className="max-h-80 overflow-auto whitespace-pre-wrap">{before}</pre></div>
      <div><p>修改后</p><pre className="max-h-80 overflow-auto whitespace-pre-wrap">{after}</pre></div>
    </div>
  </details>;
}

export function SkillSourceCoverage({ run }: { run: HarnessRun }) {
  const coverage = run.input_json?.source_coverage as { selected_event_count: number; older_events_omitted: boolean } | undefined;
  if (!coverage) return null;
  return <p className="mt-3 text-xs text-text-muted" role={coverage.older_events_omitted ? 'status' : undefined}>
    使用最近 {coverage.selected_event_count} 条会话记录。
    {coverage.older_events_omitted ? '较早记录已省略；这是有限证据窗口，不能代表完整会话。如需早期材料，请在详细审核中选择或上传来源后重新生成。' : '所选会话中可读取的记录均已包含。'}
  </p>;
}
