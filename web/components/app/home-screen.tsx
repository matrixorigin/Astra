'use client';

import {
  ArrowRight,
  ListTodo,
  Network,
  RefreshCw,
  ScanSearch,
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
  title: string;
  description: string;
  prompt: string;
};

const journeys: Journey[] = [
  {
    icon: Network,
    title: 'Multi-agent review',
    description: 'Delegate focused reviews and synthesize verified findings.',
    prompt:
      'Review the current workspace with multiple agents covering correctness, unhappy paths, architecture, tests, and product quality. Verify and synthesize the findings.',
  },
  {
    icon: ListTodo,
    title: 'Durable task work',
    description: 'Plan, execute, track blockers, and keep long work resumable.',
    prompt:
      'Turn this goal into a concrete task board, execute it systematically, surface blockers early, and keep the run resumable: ',
  },
  {
    icon: ScanSearch,
    title: 'Introspect a session',
    description: 'Inspect decisions, context pressure, evidence, and next actions.',
    prompt:
      'Introspect this session and reflect on progress, important decisions, risks, context usage, and the highest-value next action.',
  },
  {
    icon: Workflow,
    title: 'Build a harness',
    description: 'Turn a proven workflow into a reusable, reviewed system.',
    prompt:
      'Help me design a practical reusable harness for this workflow, including sources, agent roles, review gates, outputs, and failure recovery: ',
  },
];

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
      <div className="mx-auto flex min-h-full w-full max-w-[980px] flex-col px-5 py-6 sm:px-8 lg:px-10">
        <header className="flex items-center justify-between gap-4">
          <div>
            <p className="text-xs font-semibold uppercase tracking-[0.12em] text-text-muted">
              New workspace
            </p>
            <p className="mt-1 text-sm text-text-secondary">
              One session, explicit execution, durable evidence.
            </p>
          </div>
          <RuntimeStatus
            edges={edgeWorkspaces}
            loading={edgeWorkspacesLoading}
            error={edgeWorkspacesError}
            onRefresh={refreshEdgeWorkspaces}
          />
        </header>

        <main className="flex flex-1 flex-col justify-center py-10 lg:py-14">
          <section>
            <h1 className="max-w-3xl text-[clamp(2.1rem,5vw,3.8rem)] font-semibold leading-[1.04] tracking-[-0.045em] text-text">
              What should Astra
              <span className="text-text-muted"> move forward?</span>
            </h1>
            <p className="mt-4 max-w-2xl text-sm leading-6 text-text-secondary sm:text-base">
              Describe the outcome. Choose the runtime, skills, or connectors
              only when the work needs them.
            </p>
          </section>

          <section className="mt-7">
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

          <section className="mt-7">
            <div className="flex items-center justify-between gap-4">
              <p className="text-xs font-semibold uppercase tracking-[0.1em] text-text-muted">
                Starting points
              </p>
              <Link
                href="/harnesses"
                className="inline-flex items-center gap-1 text-xs font-medium text-text-muted hover:text-text"
              >
                Browse harnesses
                <ArrowRight className="size-3.5" />
              </Link>
            </div>

            <div className="mt-3 grid gap-2 sm:grid-cols-2">
              {journeys.map((journey) => (
                <button
                  key={journey.title}
                  type="button"
                  onClick={() => setInitialValue(journey.prompt)}
                  className="group flex min-h-[88px] items-start gap-3 rounded-card border border-border/80 bg-surface px-4 py-3.5 text-left shadow-[0_1px_2px_rgba(15,23,42,0.025)] transition hover:border-border-strong hover:bg-surface-raised hover:shadow-[0_8px_22px_rgba(15,23,42,0.055)]"
                >
                  <span className="mt-0.5 inline-flex size-8 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent">
                    <journey.icon className="size-4" />
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="block text-sm font-semibold text-text">
                      {journey.title}
                    </span>
                    <span className="mt-1 block text-xs leading-5 text-text-muted">
                      {journey.description}
                    </span>
                  </span>
                  <ArrowRight className="mt-1 size-3.5 shrink-0 text-text-muted transition group-hover:translate-x-0.5 group-hover:text-accent" />
                </button>
              ))}
            </div>
          </section>
        </main>

        <footer className="flex flex-wrap items-center gap-x-5 gap-y-2 border-t border-border/70 pt-4 text-[11px] text-text-muted">
          <span>Agents stay observable</span>
          <span>Tasks stay durable</span>
          <span>Decisions stay inspectable</span>
        </footer>
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
    <div className="flex max-w-[260px] items-center gap-2 rounded-full border border-border bg-surface px-2.5 py-1.5 shadow-sm">
      <span
        className={cn(
          'size-2 rounded-full',
          error ? 'bg-warning' : loading ? 'animate-pulse bg-accent' : 'bg-success',
        )}
      />
      <span className="min-w-0 truncate text-xs text-text-secondary">
        {error
          ? 'Runtime available'
          : edgeCount
            ? `${edgeCount} edge${edgeCount === 1 ? '' : 's'} connected`
            : 'Server runtime'}
      </span>
      <button
        type="button"
        onClick={onRefresh}
        disabled={loading}
        className="inline-flex size-6 shrink-0 items-center justify-center rounded-full text-text-muted hover:bg-surface-muted hover:text-text disabled:opacity-50"
        aria-label="Refresh runtime status"
      >
        <RefreshCw className={cn('size-3', loading && 'animate-spin')} />
      </button>
    </div>
  );
}
