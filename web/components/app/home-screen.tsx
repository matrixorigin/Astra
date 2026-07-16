'use client';

import {
  ArrowRight,
  Bot,
  GitBranch,
  ListTodo,
  Network,
  RefreshCw,
  ScanSearch,
  Sparkles,
  Workflow,
  type LucideIcon,
} from 'lucide-react';
import Link from 'next/link';
import { useRouter } from 'next/navigation';
import { useCallback, useEffect, useState } from 'react';
import { Composer } from '@/components/app/composer';
import { createChat, getEdgeStatus } from '@/lib/api/chats';
import { isAuthRequiredError } from '@/lib/api/errors';
import type { EdgeStatusResponse, WorkspaceSelection } from '@/lib/api/types';
import { cn } from '@/lib/utils/cn';

type Journey = {
  icon: LucideIcon;
  eyebrow: string;
  title: string;
  description: string;
  prompt: string;
  tone: 'blue' | 'violet' | 'amber' | 'green';
};

const journeys: Journey[] = [
  {
    icon: Network,
    eyebrow: 'Multi-agent',
    title: 'Review from multiple angles',
    description:
      'Delegate bounded reviews, observe each run, and synthesize verified findings.',
    prompt:
      'Review the current workspace with multiple agents covering correctness, unhappy paths, architecture, tests, and product quality. Verify and synthesize the findings.',
    tone: 'blue',
  },
  {
    icon: ListTodo,
    eyebrow: 'Tasks',
    title: 'Turn a goal into durable work',
    description:
      'Create an explicit task board, track blockers, and keep long-running work resumable.',
    prompt:
      'Turn this goal into a concrete task board, execute it systematically, surface blockers early, and keep the run resumable: ',
    tone: 'green',
  },
  {
    icon: ScanSearch,
    eyebrow: 'Introspect + reflect',
    title: 'Understand what happened',
    description:
      'Inspect decisions, tool routing, context pressure, and the best next action from evidence.',
    prompt:
      'Introspect this session and reflect on progress, important decisions, risks, context usage, and the highest-value next action.',
    tone: 'violet',
  },
  {
    icon: Workflow,
    eyebrow: 'Harness',
    title: 'Build a repeatable workflow',
    description:
      'Compose sources, agent roles, review gates, and audited outputs into a reusable harness.',
    prompt:
      'Help me design a practical reusable harness for this workflow, including sources, agent roles, review gates, outputs, and failure recovery: ',
    tone: 'amber',
  },
];

const toneClass: Record<Journey['tone'], string> = {
  blue: 'border-blue-200/70 bg-blue-50/45 text-blue-700',
  violet: 'border-violet-200/70 bg-violet-50/45 text-violet-700',
  amber: 'border-amber-200/70 bg-amber-50/45 text-amber-700',
  green: 'border-emerald-200/70 bg-emerald-50/45 text-emerald-700',
};

export function HomeScreen() {
  const router = useRouter();
  const [initialValue, setInitialValue] = useState('');
  const [busy, setBusy] = useState(false);
  const [workspaceSelection, setWorkspaceSelection] =
    useState<WorkspaceSelection | null>(null);
  const [edgeWorkspaces, setEdgeWorkspaces] =
    useState<EdgeStatusResponse['edges']>([]);
  const [edgeWorkspacesLoading, setEdgeWorkspacesLoading] = useState(false);
  const [edgeWorkspacesError, setEdgeWorkspacesError] = useState<string | null>(
    null,
  );

  const refreshEdgeWorkspaces = useCallback(async () => {
    setEdgeWorkspacesLoading(true);
    setEdgeWorkspacesError(null);
    try {
      const status = await getEdgeStatus();
      setEdgeWorkspaces(status.edges);
    } catch (error) {
      setEdgeWorkspaces([]);
      setEdgeWorkspacesError(
        error instanceof Error ? error.message : 'Failed to load environments.',
      );
    } finally {
      setEdgeWorkspacesLoading(false);
    }
  }, []);

  useEffect(() => {
    void refreshEdgeWorkspaces();
  }, [refreshEdgeWorkspaces]);

  return (
    <div className="h-full overflow-y-auto overscroll-contain">
      <div className="mx-auto flex min-h-full w-full max-w-[1180px] flex-col px-6 pb-16 pt-10 lg:px-10 lg:pt-14">
        <header className="flex flex-wrap items-start justify-between gap-5">
          <div className="max-w-3xl">
            <div className="inline-flex items-center gap-2 rounded-full border border-border bg-surface px-3 py-1 text-xs font-medium text-text-secondary shadow-sm">
              <Sparkles className="size-3.5 text-accent" />
              Astra agent workspace
            </div>
            <h1 className="mt-5 max-w-3xl text-[clamp(2.35rem,5vw,4.75rem)] font-semibold leading-[0.98] tracking-[-0.045em] text-text">
              Turn intent into
              <span className="block text-text-muted">durable work.</span>
            </h1>
            <p className="mt-5 max-w-2xl text-base leading-7 text-text-secondary">
              Work with agents that can plan, delegate, use tools, preserve
              evidence, and resume across long-running tasks—while you stay in
              control.
            </p>
          </div>

          <RuntimeStatus
            edges={edgeWorkspaces}
            loading={edgeWorkspacesLoading}
            error={edgeWorkspacesError}
            onRefresh={refreshEdgeWorkspaces}
          />
        </header>

        <section className="mt-10">
          <Composer
            key={initialValue}
            initialValue={initialValue}
            disabled={busy}
            className="w-full max-w-none"
            workspaceSelection={workspaceSelection}
            edgeWorkspaces={edgeWorkspaces}
            edgeWorkspacesLoading={edgeWorkspacesLoading}
            edgeWorkspacesError={edgeWorkspacesError}
            onWorkspaceSelectionChange={setWorkspaceSelection}
            onRefreshEdgeWorkspaces={refreshEdgeWorkspaces}
            onSubmit={async ({ text, options }) => {
              setBusy(true);
              try {
                const result = await createChat({
                  message: text,
                  model: options.model,
                  options: {
                    webSearch: options.webSearch,
                    thinking: options.thinking,
                    activeSkills: options.activeSkills,
                    activeTools: options.activeTools,
                  },
                  projectId: null,
                  workspaceSelection,
                });
                router.replace(`/chats/${result.chatId}`);
              } catch (error) {
                if (isAuthRequiredError(error)) {
                  router.push('/login?next=/');
                  return;
                }
                throw error;
              } finally {
                setBusy(false);
              }
            }}
          />
        </section>

        <section className="mt-10">
          <div className="flex items-end justify-between gap-4">
            <div>
              <p className="text-xs font-semibold uppercase tracking-[0.1em] text-text-muted">
                Start with an outcome
              </p>
              <h2 className="mt-1 text-xl font-semibold tracking-[-0.02em] text-text">
                Choose a working mode
              </h2>
            </div>
            <Link
              href="/harnesses"
              className="hidden items-center gap-1.5 text-sm font-medium text-text-secondary hover:text-text sm:inline-flex"
            >
              Browse harnesses
              <ArrowRight className="size-4" />
            </Link>
          </div>

          <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-4">
            {journeys.map((journey) => (
              <button
                key={journey.eyebrow}
                type="button"
                onClick={() => setInitialValue(journey.prompt)}
                className="group flex min-h-52 flex-col rounded-card border border-border bg-surface p-5 text-left shadow-[0_1px_2px_rgba(15,23,42,0.03)] transition hover:-translate-y-0.5 hover:border-border-strong hover:shadow-[0_12px_30px_rgba(15,23,42,0.07)]"
              >
                <span
                  className={cn(
                    'inline-flex size-9 items-center justify-center rounded-control border',
                    toneClass[journey.tone],
                  )}
                >
                  <journey.icon className="size-4" />
                </span>
                <span className="mt-5 text-[11px] font-semibold uppercase tracking-[0.1em] text-text-muted">
                  {journey.eyebrow}
                </span>
                <span className="mt-1 text-base font-semibold leading-6 text-text">
                  {journey.title}
                </span>
                <span className="mt-2 text-sm leading-6 text-text-muted">
                  {journey.description}
                </span>
                <span className="mt-auto flex items-center gap-1.5 pt-5 text-xs font-medium text-accent opacity-0 transition group-hover:opacity-100">
                  Use this starting point
                  <ArrowRight className="size-3.5" />
                </span>
              </button>
            ))}
          </div>
        </section>

        <section className="mt-10 grid gap-3 border-t border-border pt-7 md:grid-cols-3">
          <CapabilitySummary
            icon={Bot}
            title="Observable agents"
            description="Each delegated run keeps its own status, events, tools, and transcript identity."
          />
          <CapabilitySummary
            icon={GitBranch}
            title="Explicit execution boundaries"
            description="See where work runs, which tools are selected, and when user action is required."
          />
          <CapabilitySummary
            icon={Workflow}
            title="Reusable harnesses"
            description="Turn proven sessions into reviewed, auditable workflows instead of repeating prompts."
          />
        </section>
      </div>
    </div>
  );
}

function RuntimeStatus({
  edges,
  loading,
  error,
  onRefresh,
}: {
  edges: EdgeStatusResponse['edges'];
  loading: boolean;
  error: string | null;
  onRefresh: () => void;
}) {
  const edgeCount = edges.length;
  return (
    <div className="w-full max-w-sm rounded-card border border-border bg-surface p-4 shadow-sm lg:w-80">
      <div className="flex items-center gap-3">
        <span
          className={cn(
            'size-2.5 rounded-full',
            error ? 'bg-warning' : loading ? 'animate-pulse bg-accent' : 'bg-success',
          )}
        />
        <div className="min-w-0 flex-1">
          <p className="text-sm font-semibold text-text">Runtime ready</p>
          <p className="mt-0.5 truncate text-xs text-text-muted">
            {error
              ? 'Server available · edge status unavailable'
              : edgeCount
                ? `${edgeCount} connected edge workspace${edgeCount === 1 ? '' : 's'}`
                : 'Server workspace · no connected edges'}
          </p>
        </div>
        <button
          type="button"
          onClick={onRefresh}
          disabled={loading}
          className="inline-flex size-8 items-center justify-center rounded-control text-text-muted hover:bg-surface-muted hover:text-text disabled:opacity-50"
          aria-label="Refresh runtime status"
        >
          <RefreshCw className={cn('size-4', loading && 'animate-spin')} />
        </button>
      </div>
      <div className="mt-4 grid grid-cols-3 gap-2 border-t border-border pt-3 text-center">
        <RuntimeFact label="Agents" value="Ready" />
        <RuntimeFact label="Tasks" value="Durable" />
        <RuntimeFact label="Reflect" value="On demand" />
      </div>
    </div>
  );
}

function RuntimeFact({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <p className="text-[10px] font-semibold uppercase tracking-[0.08em] text-text-muted">
        {label}
      </p>
      <p className="mt-1 text-xs font-medium text-text">{value}</p>
    </div>
  );
}

function CapabilitySummary({
  icon: Icon,
  title,
  description,
}: {
  icon: LucideIcon;
  title: string;
  description: string;
}) {
  return (
    <div className="flex gap-3">
      <span className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-control bg-surface-muted text-text-secondary">
        <Icon className="size-4" />
      </span>
      <div>
        <h3 className="text-sm font-semibold text-text">{title}</h3>
        <p className="mt-1 text-xs leading-5 text-text-muted">{description}</p>
      </div>
    </div>
  );
}
