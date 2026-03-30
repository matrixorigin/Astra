const nodes = [
  { title: 'Plan created', note: 'Define run goal and initial steps.' },
  { title: 'Agent delegated', note: 'Spawn code-review specialist.' },
  { title: 'Waiting', note: 'Pause on CI or human approval.' },
  { title: 'Completed', note: 'Persist summary and final event.' },
];

export function PlanFlowPreview() {
  return (
    <div className="space-y-4">
      {nodes.map((node, index) => (
        <div key={node.title} className="relative pl-10">
          {index < nodes.length - 1 ? (
            <div className="absolute left-4 top-10 h-16 w-px bg-slate-700" />
          ) : null}
          <div className="absolute left-0 top-2 flex h-8 w-8 items-center justify-center rounded-full border border-sky-400/40 bg-sky-400/10 text-xs text-sky-300">
            {index + 1}
          </div>
          <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
            <p className="font-medium text-white">{node.title}</p>
            <p className="mt-1 text-sm text-slate-400">{node.note}</p>
          </div>
        </div>
      ))}
    </div>
  );
}
