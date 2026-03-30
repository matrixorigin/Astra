import type { EventSummary } from '@/lib/models/platform';

type GraphNode = {
  id: string;
  label: string;
  subtitle: string;
  x: number;
  y: number;
  tone: 'session' | 'event' | 'agent';
};

type GraphEdge = {
  from: string;
  to: string;
};

function toneClasses(tone: GraphNode['tone']): string {
  switch (tone) {
    case 'session':
      return 'border-sky-400/40 bg-sky-400/10 text-sky-100';
    case 'agent':
      return 'border-violet-400/40 bg-violet-400/10 text-violet-100';
    default:
      return 'border-slate-700 bg-slate-950/90 text-slate-100';
  }
}

function buildGraph(
  sessionId: string,
  sessionTitle: string,
  events: EventSummary[],
): { nodes: GraphNode[]; edges: GraphEdge[]; height: number } {
  const scopedEvents = events.slice(0, 6);
  const agentIds = Array.from(
    new Set(scopedEvents.map((event) => event.agentId).filter((value): value is string => Boolean(value))),
  );

  const nodes: GraphNode[] = [
    {
      id: `session:${sessionId}`,
      label: sessionTitle,
      subtitle: sessionId,
      x: 24,
      y: 140,
      tone: 'session',
    },
  ];

  const edges: GraphEdge[] = [];
  const sessionNodeId = `session:${sessionId}`;

  scopedEvents.forEach((event, index) => {
    const eventNodeId = `event:${event.id}`;
    nodes.push({
      id: eventNodeId,
      label: event.type,
      subtitle: event.createdAt,
      x: 300,
      y: 24 + index * 104,
      tone: 'event',
    });
    edges.push({
      from: index === 0 ? sessionNodeId : `event:${scopedEvents[index - 1].id}`,
      to: eventNodeId,
    });
  });

  agentIds.forEach((agentId, index) => {
    nodes.push({
      id: `agent:${agentId}`,
      label: agentId,
      subtitle: 'agent',
      x: 576,
      y: 48 + index * 132,
      tone: 'agent',
    });
  });

  scopedEvents.forEach((event) => {
    if (event.agentId) {
      edges.push({
        from: `event:${event.id}`,
        to: `agent:${event.agentId}`,
      });
    }
  });

  return {
    nodes,
    edges,
    height: Math.max(280, scopedEvents.length * 104 + 40),
  };
}

export function SessionFlowGraph({
  sessionId,
  sessionTitle,
  events,
}: {
  sessionId: string;
  sessionTitle: string;
  events: EventSummary[];
}) {
  if (events.length === 0) {
    return (
      <div className="rounded-2xl border border-dashed border-slate-700 px-4 py-10 text-sm text-slate-400">
        No graphable events yet for this session.
      </div>
    );
  }

  const { nodes, edges, height } = buildGraph(sessionId, sessionTitle, events);
  const nodeMap = new Map(nodes.map((node) => [node.id, node]));

  return (
    <div className="overflow-x-auto">
      <div
        className="relative min-w-[860px] rounded-3xl border border-slate-800 bg-slate-950/70"
        style={{ height }}
      >
        <svg className="absolute inset-0 h-full w-full" aria-hidden="true">
          {edges.map((edge, index) => {
            const from = nodeMap.get(edge.from);
            const to = nodeMap.get(edge.to);

            if (!from || !to) {
              return null;
            }

            const x1 = from.x + 216;
            const y1 = from.y + 34;
            const x2 = to.x;
            const y2 = to.y + 34;
            const controlX = (x1 + x2) / 2;

            return (
              <path
                key={`${edge.from}-${edge.to}-${index}`}
                d={`M ${x1} ${y1} C ${controlX} ${y1}, ${controlX} ${y2}, ${x2} ${y2}`}
                fill="none"
                stroke="rgba(148, 163, 184, 0.45)"
                strokeWidth="2"
              />
            );
          })}
        </svg>

        {nodes.map((node) => (
          <div
            key={node.id}
            className={`absolute w-[216px] rounded-2xl border px-4 py-3 shadow-lg ${toneClasses(node.tone)}`}
            style={{ left: node.x, top: node.y }}
          >
            <p className="text-sm font-medium">{node.label}</p>
            <p className="mt-1 text-xs opacity-75">{node.subtitle}</p>
          </div>
        ))}
      </div>
    </div>
  );
}
