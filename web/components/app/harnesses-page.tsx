'use client';

import {
  ArrowLeft,
  Bot,
  Braces,
  Check,
  Database,
  FileCode2,
  GitBranch,
  Layers3,
  Play,
  Plus,
  RefreshCw,
  Sparkles,
  UploadCloud,
  X,
} from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { listChats } from '@/lib/api/chats';
import {
  createSkillifyDraft,
  createSkillifyRun,
  decideHarnessItem,
  listHarnessNodeCatalog,
  listHarnessRunItems,
  listHarnessTemplates,
} from '@/lib/api/harnesses';
import type {
  ChatSummary,
  HarnessItem,
  HarnessNodeCatalogItem,
  HarnessRun,
  HarnessTemplate,
  SkillifyDraft,
} from '@/lib/api/types';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { PageHeader } from '@/components/ui/page-header';
import { Textarea } from '@/components/ui/textarea';
import { cn } from '@/lib/utils/cn';

type HarnessView = 'catalog' | 'skillify' | 'custom';

function outputString(item: HarnessItem, key: string) {
  const value = item.final_output_json[key] ?? item.proposed_output_json[key];
  return typeof value === 'string' ? value : '';
}

function statusClass(status: string) {
  if (status === 'approved' || status === 'completed' || status === 'ready') {
    return 'border-success/30 bg-success/10 text-success';
  }
  if (status === 'rejected' || status === 'blocked') {
    return 'border-danger/30 bg-danger/10 text-danger';
  }
  return 'border-border bg-surface-muted text-text-secondary';
}

function workflowNodeCount(template: HarnessTemplate) {
  const workflow = template.workflow_json;
  if (!workflow || typeof workflow !== 'object' || !('nodes' in workflow)) {
    return null;
  }
  const nodes = (workflow as { nodes?: unknown }).nodes;
  return Array.isArray(nodes) ? nodes.length : null;
}

function defaultCustomWorkflow() {
  return [
    '1. Ingest sources and freeze source snapshots.',
    '2. Split work into reviewable items.',
    '3. Assign agent/subagent roles for extraction, verification, and drafting.',
    '4. Hold uncertain items in human review.',
    '5. Emit audited artifacts with citations and decisions.',
  ].join('\n');
}

export function HarnessesPage() {
  const [view, setView] = useState<HarnessView>('catalog');
  const [templates, setTemplates] = useState<HarnessTemplate[]>([]);
  const [nodeCatalog, setNodeCatalog] = useState<HarnessNodeCatalogItem[]>([]);
  const [sessions, setSessions] = useState<ChatSummary[]>([]);
  const [selected, setSelected] = useState<string[]>([]);
  const [skillName, setSkillName] = useState('');
  const [topic, setTopic] = useState('');
  const [run, setRun] = useState<HarnessRun | null>(null);
  const [items, setItems] = useState<HarnessItem[]>([]);
  const [draft, setDraft] = useState<SkillifyDraft | null>(null);
  const [customName, setCustomName] = useState('New harness');
  const [customPurpose, setCustomPurpose] = useState('');
  const [customSources, setCustomSources] = useState('sessions, uploaded files, external data source');
  const [customWorkflow, setCustomWorkflow] = useState(defaultCustomWorkflow());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const skillifyTemplate = templates.find((template) => template.template_id === 'skillify.v1');
  const approvedCount = useMemo(() => items.filter((item) => item.status === 'approved').length, [items]);
  const pendingCount = useMemo(() => items.filter((item) => item.status === 'pending_review').length, [items]);

  const loadInitial = useCallback(async () => {
    setError(null);
    try {
      const [templatePayload, nodePayload, chatPayload] = await Promise.all([
        listHarnessTemplates(),
        listHarnessNodeCatalog(),
        listChats({ limit: 50, archived: false }),
      ]);
      setTemplates(templatePayload);
      setNodeCatalog(nodePayload);
      setSessions(chatPayload.items);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load harness data.');
    }
  }, []);

  useEffect(() => {
    void loadInitial();
  }, [loadInitial]);

  const refreshItems = useCallback(async (runId: string) => {
    setItems(await listHarnessRunItems(runId));
  }, []);

  const toggleSession = useCallback((sessionId: string) => {
    setSelected((current) => (
      current.includes(sessionId)
        ? current.filter((id) => id !== sessionId)
        : [...current, sessionId]
    ));
  }, []);

  const openTemplate = useCallback((template: HarnessTemplate) => {
    setError(null);
    if (template.template_id === 'skillify.v1') {
      setView('skillify');
      return;
    }
    setError(`${template.name} does not have a run UI yet.`);
  }, []);

  const startSkillify = useCallback(async () => {
    setError(null);
    setDraft(null);
    if (selected.length === 0) {
      setError('Select at least one session.');
      return;
    }
    setBusy(true);
    try {
      const created = await createSkillifyRun({
        session_ids: selected,
        skill_name: skillName.trim() || null,
        topic: topic.trim() || null,
        target_scope: 'personal',
      });
      setRun(created);
      await refreshItems(created.harness_run_id);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start Skillify.');
    } finally {
      setBusy(false);
    }
  }, [refreshItems, selected, skillName, topic]);

  const decide = useCallback(async (item: HarnessItem, decision: 'approve' | 'reject') => {
    if (!run) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const updated = await decideHarnessItem(run.harness_run_id, item.item_id, {
        decision,
        reason: decision === 'approve' ? 'Approved from harness review UI.' : 'Rejected from harness review UI.',
      });
      setItems((current) => current.map((entry) => (entry.item_id === updated.item_id ? updated : entry)));
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update candidate.');
    } finally {
      setBusy(false);
    }
  }, [run]);

  const createDraft = useCallback(async () => {
    if (!run) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      setDraft(await createSkillifyDraft(run.harness_run_id, {
        skill_name: skillName.trim() || null,
        version: '0.1.0',
      }));
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create draft skill.');
    } finally {
      setBusy(false);
    }
  }, [run, skillName]);

  const goCatalog = useCallback(() => {
    setError(null);
    setView('catalog');
  }, []);

  const headerAction = (
    <div className="flex flex-wrap gap-2">
      {view !== 'catalog' ? (
        <Button leadingIcon={ArrowLeft} onClick={goCatalog}>
          Catalog
        </Button>
      ) : (
        <Button leadingIcon={Plus} variant="primary" onClick={() => setView('custom')}>
          Custom Harness
        </Button>
      )}
      <Button leadingIcon={RefreshCw} onClick={loadInitial} disabled={busy}>
        Refresh
      </Button>
    </div>
  );

  return (
    <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
      <div className="mx-auto flex max-w-7xl flex-col gap-6">
        <PageHeader
          title="Harnesses"
          description="Reusable agent workflows with explicit sources, review gates, and audit state."
          action={headerAction}
        />

        {error ? (
          <div className="rounded-card border border-danger/30 bg-danger/10 px-4 py-3 text-sm text-danger">
            {error}
          </div>
        ) : null}

        {view === 'catalog' ? (
          <CatalogView templates={templates} onOpenTemplate={openTemplate} onOpenCustom={() => setView('custom')} />
        ) : null}

        {view === 'skillify' ? (
          <SkillifyView
            busy={busy}
            sessions={sessions}
            selected={selected}
            skillName={skillName}
            topic={topic}
            run={run}
            items={items}
            draft={draft}
            approvedCount={approvedCount}
            pendingCount={pendingCount}
            template={skillifyTemplate}
            onSkillNameChange={setSkillName}
            onTopicChange={setTopic}
            onToggleSession={toggleSession}
            onStart={startSkillify}
            onDecide={decide}
            onCreateDraft={createDraft}
          />
        ) : null}

        {view === 'custom' ? (
          <CustomHarnessView
            name={customName}
            purpose={customPurpose}
            sources={customSources}
            workflow={customWorkflow}
            nodeCatalog={nodeCatalog}
            onNameChange={setCustomName}
            onPurposeChange={setCustomPurpose}
            onSourcesChange={setCustomSources}
            onWorkflowChange={setCustomWorkflow}
            onOpenSkillify={() => setView('skillify')}
          />
        ) : null}
      </div>
    </div>
  );
}

function CatalogView({
  templates,
  onOpenTemplate,
  onOpenCustom,
}: {
  templates: HarnessTemplate[];
  onOpenTemplate: (template: HarnessTemplate) => void;
  onOpenCustom: () => void;
}) {
  return (
    <div className="grid gap-6 xl:grid-cols-[minmax(0,1fr)_360px]">
      <section className="space-y-3">
        <div className="flex items-center justify-between gap-3">
          <h2 className="text-base font-semibold">Existing Harnesses</h2>
          <span className="text-xs text-text-muted">{templates.length} available</span>
        </div>

        {templates.length ? templates.map((template) => {
          const nodeCount = workflowNodeCount(template);
          return (
            <Card key={template.template_id} interactive>
              <div className="flex flex-col gap-4 md:flex-row md:items-start md:justify-between">
                <div className="flex min-w-0 gap-3">
                  <div className="flex size-10 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent">
                    {template.template_id === 'skillify.v1' ? (
                      <Sparkles className="size-5" />
                    ) : (
                      <GitBranch className="size-5" />
                    )}
                  </div>
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <h3 className="font-semibold">{template.name}</h3>
                      <span className={cn('rounded-full border px-2 py-0.5 text-xs', statusClass('ready'))}>
                        ready
                      </span>
                      {template.built_in ? (
                        <span className="rounded-full border border-border bg-surface-muted px-2 py-0.5 text-xs text-text-secondary">
                          built-in
                        </span>
                      ) : null}
                    </div>
                    <p className="mt-1 max-w-3xl text-sm text-text-secondary">{template.description}</p>
                    <div className="mt-3 flex flex-wrap gap-2 text-xs text-text-muted">
                      <span className="rounded-full border border-border px-2 py-1">{template.template_id}</span>
                      {nodeCount !== null ? (
                        <span className="rounded-full border border-border px-2 py-1">{nodeCount} nodes</span>
                      ) : null}
                    </div>
                  </div>
                </div>
                <Button leadingIcon={Play} onClick={() => onOpenTemplate(template)}>
                  Open
                </Button>
              </div>
            </Card>
          );
        }) : (
          <Card>
            <p className="text-sm text-text-secondary">No harnesses are available from the runtime.</p>
          </Card>
        )}
      </section>

      <aside className="space-y-3">
        <h2 className="text-base font-semibold">Create</h2>
        <Card interactive>
          <div className="flex items-start gap-3">
            <div className="flex size-10 shrink-0 items-center justify-center rounded-control bg-surface-muted text-text-secondary">
              <Plus className="size-5" />
            </div>
            <div className="min-w-0 flex-1">
              <h3 className="font-semibold">Custom Harness</h3>
              <p className="mt-1 text-sm text-text-secondary">
                Define sources, graph shape, agent roles, review gates, and audited outputs.
              </p>
              <Button className="mt-4 w-full" variant="primary" leadingIcon={Braces} onClick={onOpenCustom}>
                Define
              </Button>
            </div>
          </div>
        </Card>

        <Card>
          <div className="flex items-start gap-3">
            <Layers3 className="mt-0.5 size-5 shrink-0 text-text-muted" />
            <div>
              <h3 className="text-sm font-medium">Common source types</h3>
              <div className="mt-3 grid grid-cols-2 gap-2 text-xs text-text-secondary">
                <span className="rounded-control border border-border bg-surface-muted px-2 py-1">Sessions</span>
                <span className="rounded-control border border-border bg-surface-muted px-2 py-1">Uploads</span>
                <span className="rounded-control border border-border bg-surface-muted px-2 py-1">Knowledge DS</span>
                <span className="rounded-control border border-border bg-surface-muted px-2 py-1">External APIs</span>
              </div>
            </div>
          </div>
        </Card>
      </aside>
    </div>
  );
}

function SkillifyView({
  busy,
  sessions,
  selected,
  skillName,
  topic,
  run,
  items,
  draft,
  approvedCount,
  pendingCount,
  template,
  onSkillNameChange,
  onTopicChange,
  onToggleSession,
  onStart,
  onDecide,
  onCreateDraft,
}: {
  busy: boolean;
  sessions: ChatSummary[];
  selected: string[];
  skillName: string;
  topic: string;
  run: HarnessRun | null;
  items: HarnessItem[];
  draft: SkillifyDraft | null;
  approvedCount: number;
  pendingCount: number;
  template?: HarnessTemplate;
  onSkillNameChange: (value: string) => void;
  onTopicChange: (value: string) => void;
  onToggleSession: (sessionId: string) => void;
  onStart: () => void;
  onDecide: (item: HarnessItem, decision: 'approve' | 'reject') => void;
  onCreateDraft: () => void;
}) {
  return (
    <div className="grid gap-6 xl:grid-cols-[360px_minmax(0,1fr)]">
      <section className="space-y-4">
        <Card>
          <div className="flex items-start gap-3">
            <div className="flex size-9 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent">
              <Sparkles className="size-5" />
            </div>
            <div className="min-w-0">
              <h2 className="text-base font-semibold">Skillify</h2>
              <p className="mt-1 text-sm text-text-secondary">
                {template?.description ?? 'Extract reviewed personal skill rules from selected sessions.'}
              </p>
            </div>
          </div>

          <div className="mt-4 space-y-3">
            <label className="block text-sm font-medium">
              Draft skill name
              <Input
                value={skillName}
                onChange={(event) => onSkillNameChange(event.target.value)}
                placeholder="my-workflow-style"
                className="mt-1"
              />
            </label>
            <label className="block text-sm font-medium">
              Topic filter
              <Input
                value={topic}
                onChange={(event) => onTopicChange(event.target.value)}
                placeholder="writing, design review, paper reading"
                className="mt-1"
              />
            </label>
            <Button
              variant="primary"
              leadingIcon={Play}
              onClick={onStart}
              disabled={busy || selected.length === 0}
              className="w-full"
            >
              Run Skillify
            </Button>
          </div>
        </Card>

        <Card>
          <div className="flex items-center justify-between gap-3">
            <h2 className="text-base font-semibold">Session Sources</h2>
            <span className="text-xs text-text-muted">{selected.length} selected</span>
          </div>
          <div className="mt-3 max-h-[420px] space-y-2 overflow-y-auto pr-1">
            {sessions.length ? sessions.map((session) => (
              <button
                key={session.id}
                type="button"
                onClick={() => onToggleSession(session.id)}
                className={cn(
                  'flex w-full items-start gap-3 rounded-control border px-3 py-2 text-left text-sm',
                  selected.includes(session.id)
                    ? 'border-accent bg-accent/10'
                    : 'border-border bg-surface hover:bg-surface-muted',
                )}
              >
                <span
                  className={cn(
                    'mt-0.5 flex size-4 shrink-0 items-center justify-center rounded border',
                    selected.includes(session.id) ? 'border-accent bg-accent text-white' : 'border-border',
                  )}
                >
                  {selected.includes(session.id) ? <Check className="size-3" /> : null}
                </span>
                <span className="min-w-0">
                  <span className="block truncate font-medium">{session.title || 'Untitled session'}</span>
                  <span className="mt-0.5 block truncate text-xs text-text-muted">
                    {session.lastMessagePreview || session.id}
                  </span>
                </span>
              </button>
            )) : (
              <p className="rounded-control border border-border bg-surface-muted px-3 py-2 text-sm text-text-muted">
                No sessions available.
              </p>
            )}
          </div>
        </Card>
      </section>

      <section className="space-y-4">
        <Card>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <h2 className="text-base font-semibold">Run Review</h2>
              <p className="mt-1 text-sm text-text-secondary">
                Candidates stay pending until a human approves or rejects them.
              </p>
            </div>
            <div className="flex gap-2 text-xs">
              <span className={cn('rounded-full border px-2 py-1', statusClass(run?.status ?? 'idle'))}>
                {run?.status ?? 'idle'}
              </span>
              <span className="rounded-full border border-border bg-surface-muted px-2 py-1 text-text-secondary">
                {approvedCount} approved
              </span>
              <span className="rounded-full border border-border bg-surface-muted px-2 py-1 text-text-secondary">
                {pendingCount} pending
              </span>
            </div>
          </div>
        </Card>

        {items.length ? (
          <div className="space-y-3">
            {items.map((item) => (
              <Card key={item.item_id}>
                <div className="flex items-start justify-between gap-4">
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className={cn('rounded-full border px-2 py-1 text-xs', statusClass(item.status))}>
                        {item.status}
                      </span>
                      <span className="rounded-full border border-border bg-surface-muted px-2 py-1 text-xs text-text-secondary">
                        {outputString(item, 'kind') || item.item_type}
                      </span>
                      {item.confidence !== null ? (
                        <span className="text-xs text-text-muted">
                          confidence {Math.round(item.confidence * 100)}%
                        </span>
                      ) : null}
                    </div>
                    <p className="mt-3 text-sm font-medium">{outputString(item, 'statement')}</p>
                    <p className="mt-2 line-clamp-3 text-sm text-text-secondary">
                      {outputString(item, 'source_excerpt')}
                    </p>
                  </div>
                  <div className="flex shrink-0 gap-2">
                    <Button
                      size="sm"
                      leadingIcon={Check}
                      onClick={() => onDecide(item, 'approve')}
                      disabled={busy || item.status === 'approved'}
                    >
                      Approve
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      leadingIcon={X}
                      onClick={() => onDecide(item, 'reject')}
                      disabled={busy || item.status === 'rejected'}
                    >
                      Reject
                    </Button>
                  </div>
                </div>
              </Card>
            ))}
          </div>
        ) : (
          <Card>
            <p className="text-sm text-text-secondary">
              Select sessions and run Skillify to extract reviewable candidate rules.
            </p>
          </Card>
        )}

        <Card>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="min-w-0">
              <h2 className="text-base font-semibold">Draft Skill</h2>
              <p className="mt-1 text-sm text-text-secondary">
                Creates a draft personal skill from approved candidates. Activation remains a separate human action.
              </p>
            </div>
            <Button
              leadingIcon={FileCode2}
              onClick={onCreateDraft}
              disabled={busy || !run || approvedCount === 0}
            >
              Create draft
            </Button>
          </div>
          {draft ? (
            <div className="mt-4 rounded-control border border-border bg-bg p-3">
              <div className="flex flex-wrap items-center justify-between gap-2 text-sm">
                <span className="font-medium">{draft.skill_name}</span>
                <span className="text-text-muted">{draft.approved_item_count} rules</span>
              </div>
              <pre className="mt-3 max-h-72 overflow-auto whitespace-pre-wrap text-xs text-text-secondary">
                {draft.content_markdown}
              </pre>
            </div>
          ) : null}
        </Card>
      </section>
    </div>
  );
}

function CustomHarnessView({
  name,
  purpose,
  sources,
  workflow,
  nodeCatalog,
  onNameChange,
  onPurposeChange,
  onSourcesChange,
  onWorkflowChange,
  onOpenSkillify,
}: {
  name: string;
  purpose: string;
  sources: string;
  workflow: string;
  nodeCatalog: HarnessNodeCatalogItem[];
  onNameChange: (value: string) => void;
  onPurposeChange: (value: string) => void;
  onSourcesChange: (value: string) => void;
  onWorkflowChange: (value: string) => void;
  onOpenSkillify: () => void;
}) {
  return (
    <div className="grid gap-6 xl:grid-cols-[minmax(0,1fr)_380px]">
      <section className="space-y-4">
        <Card>
          <div className="flex items-start gap-3">
            <div className="flex size-10 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent">
              <Braces className="size-5" />
            </div>
            <div className="min-w-0">
              <h2 className="text-base font-semibold">Custom Harness Draft</h2>
              <p className="mt-1 text-sm text-text-secondary">
                This is the definition entry point. Persisting custom harness definitions needs the next runtime API.
              </p>
            </div>
          </div>

          <div className="mt-5 grid gap-4 md:grid-cols-2">
            <label className="block text-sm font-medium">
              Name
              <Input value={name} onChange={(event) => onNameChange(event.target.value)} className="mt-1" />
            </label>
            <label className="block text-sm font-medium">
              Sources
              <Input value={sources} onChange={(event) => onSourcesChange(event.target.value)} className="mt-1" />
            </label>
          </div>

          <label className="mt-4 block text-sm font-medium">
            Business goal
            <Textarea
              value={purpose}
              onChange={(event) => onPurposeChange(event.target.value)}
              placeholder="Summarize the repetitive business workflow this harness should run."
              className="mt-1 min-h-28"
            />
          </label>

          <label className="mt-4 block text-sm font-medium">
            Workflow outline
            <Textarea
              value={workflow}
              onChange={(event) => onWorkflowChange(event.target.value)}
              className="mt-1 min-h-52 font-mono"
            />
          </label>
        </Card>

        <Card>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <h2 className="text-base font-semibold">Examples</h2>
              <p className="mt-1 text-sm text-text-secondary">
                Start from a built-in harness, then adapt its sources, review gates, and outputs.
              </p>
            </div>
            <Button leadingIcon={Sparkles} onClick={onOpenSkillify}>
              Open Skillify
            </Button>
          </div>
          <div className="mt-4 grid gap-3 md:grid-cols-2">
            <div className="rounded-card border border-border bg-surface-muted p-3">
              <div className="flex items-center gap-2 text-sm font-medium">
                <UploadCloud className="size-4 text-text-muted" />
                Bidding response
              </div>
              <p className="mt-2 text-sm text-text-secondary">
                DS + requirement spreadsheet + cited answer cells + human review queue.
              </p>
            </div>
            <div className="rounded-card border border-border bg-surface-muted p-3">
              <div className="flex items-center gap-2 text-sm font-medium">
                <Bot className="size-4 text-text-muted" />
                Skillify
              </div>
              <p className="mt-2 text-sm text-text-secondary">
                Sessions + preference extraction + approved draft skill.
              </p>
            </div>
          </div>
        </Card>
      </section>

      <aside className="space-y-4">
        <Card>
          <div className="flex items-center gap-2">
            <GitBranch className="size-5 text-text-muted" />
            <h2 className="text-base font-semibold">Node Catalog</h2>
          </div>
          <div className="mt-3 max-h-[360px] space-y-2 overflow-y-auto pr-1">
            {nodeCatalog.length ? nodeCatalog.map((node) => (
              <div key={node.node_type} className="rounded-control border border-border bg-surface-muted px-3 py-2">
                <div className="text-sm font-medium">{node.node_type}</div>
                <p className="mt-1 text-xs text-text-secondary">{node.description}</p>
              </div>
            )) : (
              <p className="text-sm text-text-secondary">No nodes loaded.</p>
            )}
          </div>
        </Card>

        <Card>
          <div className="flex items-center gap-2">
            <Database className="size-5 text-text-muted" />
            <h2 className="text-base font-semibold">Definition Preview</h2>
          </div>
          <pre className="mt-3 max-h-[360px] overflow-auto whitespace-pre-wrap rounded-control border border-border bg-bg p-3 text-xs text-text-secondary">
            {JSON.stringify({
              name,
              purpose,
              sources: sources.split(',').map((source) => source.trim()).filter(Boolean),
              workflow: workflow.split('\n').map((step) => step.trim()).filter(Boolean),
            }, null, 2)}
          </pre>
        </Card>
      </aside>
    </div>
  );
}
