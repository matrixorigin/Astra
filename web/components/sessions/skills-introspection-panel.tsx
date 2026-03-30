import type { SkillsIntrospectionData, SkillInfo } from '@/lib/models/platform';

function SkillCard({ skill, source }: { skill: SkillInfo; source: 'installed' | 'cloud' }) {
  return (
    <div className="rounded-xl border border-slate-800 bg-slate-950/70 p-4">
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <h3 className="text-sm font-semibold text-white">{skill.name}</h3>
          <p className="mt-1 text-xs text-slate-400">{skill.description}</p>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1">
          <span className="rounded-full bg-slate-800 px-2 py-0.5 text-xs text-slate-400">
            v{skill.version}
          </span>
          <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${
            source === 'installed'
              ? 'bg-green-500/10 text-green-400 border border-green-500/30'
              : 'bg-sky-500/10 text-sky-400 border border-sky-500/30'
          }`}>
            {source}
          </span>
        </div>
      </div>
      {skill.category && (
        <p className="mt-2 text-xs text-slate-500">Category: {skill.category}</p>
      )}
    </div>
  );
}

export function SkillsIntrospectionPanel({ data }: { data: SkillsIntrospectionData }) {
  const total = data.installed.length + data.cloud.length;

  if (total === 0) {
    return (
      <p className="py-8 text-center text-sm text-slate-500">
        No skills available.
      </p>
    );
  }

  return (
    <div className="space-y-5">
      {data.installed.length > 0 && (
        <div>
          <h3 className="mb-3 text-sm font-medium text-slate-300">
            Installed ({data.installed.length})
          </h3>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {data.installed.map((s) => (
              <SkillCard key={s.name} skill={s} source="installed" />
            ))}
          </div>
        </div>
      )}

      {data.cloud.length > 0 && (
        <div>
          <h3 className="mb-3 text-sm font-medium text-slate-300">
            Cloud available ({data.cloud.length})
          </h3>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {data.cloud.map((s) => (
              <SkillCard key={s.name} skill={s} source="cloud" />
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
