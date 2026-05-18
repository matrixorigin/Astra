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
  Pencil,
  Play,
  Plus,
  RefreshCw,
  Save,
  Sparkles,
  UploadCloud,
  X,
} from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { listChats } from '@/lib/api/chats';
import {
  createSkillifyRun,
  decideSkillDraft,
  decideSkillRule,
  listHarnessNodeCatalog,
  listHarnessTemplates,
  listSkillDrafts,
  publishSkillDraft,
} from '@/lib/api/harnesses';
import type {
  ChatSummary,
  HarnessNodeCatalogItem,
  HarnessRun,
  HarnessCitation,
  HarnessSkillDraft,
  HarnessSkillRule,
  HarnessTemplate,
  SkillifyPublishRecord,
} from '@/lib/api/types';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { PageHeader } from '@/components/ui/page-header';
import { Textarea } from '@/components/ui/textarea';
import { cn } from '@/lib/utils/cn';

type HarnessView = 'catalog' | 'skillify' | 'custom';
type SkillifySourceFileInput = {
  file_name: string;
  mime_type?: string | null;
  content: string;
};
type RuleEditState = {
  statement: string;
  rationale: string;
};

function statusClass(status: string) {
  if (
    status === 'approved' ||
    status === 'edited' ||
    status === 'completed' ||
    status === 'ready' ||
    status === 'ready_to_publish' ||
    status === 'published'
  ) {
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

function stringFromUnknown(value: unknown) {
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : null;
}

function citationSourceLabel(citation: HarnessCitation) {
  const locator = citation.source_locator_json ?? {};
  return (
    stringFromUnknown(locator.title) ??
    stringFromUnknown(locator.file_name) ??
    stringFromUnknown(locator.session_id) ??
    stringFromUnknown(locator.source_id) ??
    citation.source_id ??
    'Unknown source'
  );
}

function sourceCitationLabel(count: number) {
  return `${count} source citation${count === 1 ? '' : 's'}`;
}

export function HarnessesPage() {
  const [view, setView] = useState<HarnessView>('catalog');
  const [templates, setTemplates] = useState<HarnessTemplate[]>([]);
  const [nodeCatalog, setNodeCatalog] = useState<HarnessNodeCatalogItem[]>([]);
  const [sessions, setSessions] = useState<ChatSummary[]>([]);
  const [selected, setSelected] = useState<string[]>([]);
  const [sourceFiles, setSourceFiles] = useState<SkillifySourceFileInput[]>([]);
  const [skillName, setSkillName] = useState('');
  const [topic, setTopic] = useState('');
  const [run, setRun] = useState<HarnessRun | null>(null);
  const [skillDrafts, setSkillDrafts] = useState<HarnessSkillDraft[]>([]);
  const [activeDraftId, setActiveDraftId] = useState<string | null>(null);
  const [published, setPublished] = useState<SkillifyPublishRecord[]>([]);
  const [customName, setCustomName] = useState('New harness');
  const [customPurpose, setCustomPurpose] = useState('');
  const [customSources, setCustomSources] = useState('sessions, uploaded files, external data source');
  const [customWorkflow, setCustomWorkflow] = useState(defaultCustomWorkflow());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const skillifyTemplate = templates.find((template) => template.template_id === 'skillify.v1');
  const activeDraft = useMemo(
    () => skillDrafts.find((draft) => draft.skill_draft_id === activeDraftId) ?? skillDrafts[0] ?? null,
    [activeDraftId, skillDrafts],
  );
  const approvedCount = useMemo(
    () => skillDrafts.reduce((sum, draft) => sum + draft.rules.filter((rule) => rule.status === 'approved' || rule.status === 'edited').length, 0),
    [skillDrafts],
  );
  const pendingCount = useMemo(
    () => skillDrafts.reduce((sum, draft) => sum + draft.rules.filter((rule) => rule.status === 'proposed' || rule.status === 'conflicted' || rule.status === 'needs_revision').length, 0),
    [skillDrafts],
  );

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

  const refreshDrafts = useCallback(async (runId: string) => {
    const drafts = await listSkillDrafts(runId);
    setSkillDrafts(drafts);
    setActiveDraftId((current) => (current && drafts.some((draft) => draft.skill_draft_id === current)
      ? current
      : drafts[0]?.skill_draft_id ?? null));
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
    setPublished([]);
    if (selected.length === 0 && sourceFiles.length === 0) {
      setError('Select at least one session or text file.');
      return;
    }
    setBusy(true);
    try {
      const created = await createSkillifyRun({
        session_ids: selected,
        source_files: sourceFiles,
        skill_name: skillName.trim() || null,
        topic: topic.trim() || null,
        target_scope: 'personal',
      });
      setRun(created);
      await refreshDrafts(created.harness_run_id);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start Skillify.');
    } finally {
      setBusy(false);
    }
  }, [refreshDrafts, selected, skillName, sourceFiles, topic]);

  const decideRule = useCallback(async (
    draft: HarnessSkillDraft,
    rule: HarnessSkillRule,
    decision: 'approve' | 'reject',
  ) => {
    if (!run) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const updated = await decideSkillRule(run.harness_run_id, draft.skill_draft_id, rule.skill_rule_id, {
        decision,
        reason: decision === 'approve' ? 'Approved rule from Skillify review UI.' : 'Rejected rule from Skillify review UI.',
      });
      setSkillDrafts((current) => current.map((entry) => (entry.skill_draft_id === updated.skill_draft_id ? updated : entry)));
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update rule.');
    } finally {
      setBusy(false);
    }
  }, [run]);

  const editRule = useCallback(async (
    draft: HarnessSkillDraft,
    rule: HarnessSkillRule,
    edit: RuleEditState,
  ) => {
    if (!run) {
      return false;
    }
    const statement = edit.statement.trim();
    const rationale = edit.rationale.trim();
    if (!statement) {
      setError('Rule statement must not be empty.');
      return false;
    }
    setBusy(true);
    setError(null);
    try {
      const updated = await decideSkillRule(run.harness_run_id, draft.skill_draft_id, rule.skill_rule_id, {
        decision: 'edit',
        after_json: { statement, rationale },
        reason: 'Edited rule from Skillify review UI.',
      });
      setSkillDrafts((current) => current.map((entry) => (entry.skill_draft_id === updated.skill_draft_id ? updated : entry)));
      return true;
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to edit rule.');
      return false;
    } finally {
      setBusy(false);
    }
  }, [run]);

  const decideDraft = useCallback(async (draft: HarnessSkillDraft, decision: 'approve' | 'reject') => {
    if (!run) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const updated = await decideSkillDraft(run.harness_run_id, draft.skill_draft_id, {
        decision,
        reason: decision === 'approve' ? 'Approved skill from Skillify review UI.' : 'Rejected skill from Skillify review UI.',
      });
      setSkillDrafts((current) => current.map((entry) => (entry.skill_draft_id === updated.skill_draft_id ? updated : entry)));
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update skill draft.');
    } finally {
      setBusy(false);
    }
  }, [run]);

  const editDraft = useCallback(async (draft: HarnessSkillDraft, contentMarkdown: string) => {
    if (!run) {
      return false;
    }
    const content_markdown = contentMarkdown.trim();
    if (!content_markdown) {
      setError('Skill content must not be empty.');
      return false;
    }
    setBusy(true);
    setError(null);
    try {
      const updated = await decideSkillDraft(run.harness_run_id, draft.skill_draft_id, {
        decision: 'edit',
        after_json: { content_markdown },
        reason: 'Edited skill content from Skillify review UI.',
      });
      setSkillDrafts((current) => current.map((entry) => (entry.skill_draft_id === updated.skill_draft_id ? updated : entry)));
      return true;
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to edit skill draft.');
      return false;
    } finally {
      setBusy(false);
    }
  }, [run]);

  const publishDraft = useCallback(async (draft: HarnessSkillDraft, visibility: 'private' | 'public') => {
    if (!run) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const record = await publishSkillDraft(run.harness_run_id, draft.skill_draft_id, {
        visibility,
        version: '0.1.0',
      });
      setPublished((current) => [...current, record]);
      await refreshDrafts(run.harness_run_id);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to publish skill draft.');
    } finally {
      setBusy(false);
    }
  }, [refreshDrafts, run]);

  const addSourceFiles = useCallback(async (files: FileList | null) => {
    if (!files?.length) {
      return;
    }
    setError(null);
    try {
      const next = await Promise.all(Array.from(files).map(async (file) => ({
        file_name: file.name,
        mime_type: file.type || 'text/plain',
        content: await file.text(),
      })));
      setSourceFiles((current) => [...current, ...next]);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to read source file.');
    }
  }, []);

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
            sourceFiles={sourceFiles}
            skillName={skillName}
            topic={topic}
            run={run}
            skillDrafts={skillDrafts}
            activeDraft={activeDraft}
            published={published}
            approvedCount={approvedCount}
            pendingCount={pendingCount}
            template={skillifyTemplate}
            onSkillNameChange={setSkillName}
            onTopicChange={setTopic}
            onToggleSession={toggleSession}
            onAddSourceFiles={addSourceFiles}
            onRemoveSourceFile={(index) => setSourceFiles((current) => current.filter((_, i) => i !== index))}
            onSelectDraft={setActiveDraftId}
            onStart={startSkillify}
            onDecideRule={decideRule}
            onEditRule={editRule}
            onDecideDraft={decideDraft}
            onEditDraft={editDraft}
            onPublishDraft={publishDraft}
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
  sourceFiles,
  skillName,
  topic,
  run,
  skillDrafts,
  activeDraft,
  published,
  approvedCount,
  pendingCount,
  template,
  onSkillNameChange,
  onTopicChange,
  onToggleSession,
  onAddSourceFiles,
  onRemoveSourceFile,
  onSelectDraft,
  onStart,
  onDecideRule,
  onEditRule,
  onDecideDraft,
  onEditDraft,
  onPublishDraft,
}: {
  busy: boolean;
  sessions: ChatSummary[];
  selected: string[];
  sourceFiles: SkillifySourceFileInput[];
  skillName: string;
  topic: string;
  run: HarnessRun | null;
  skillDrafts: HarnessSkillDraft[];
  activeDraft: HarnessSkillDraft | null;
  published: SkillifyPublishRecord[];
  approvedCount: number;
  pendingCount: number;
  template?: HarnessTemplate;
  onSkillNameChange: (value: string) => void;
  onTopicChange: (value: string) => void;
  onToggleSession: (sessionId: string) => void;
  onAddSourceFiles: (files: FileList | null) => void;
  onRemoveSourceFile: (index: number) => void;
  onSelectDraft: (draftId: string) => void;
  onStart: () => void;
  onDecideRule: (draft: HarnessSkillDraft, rule: HarnessSkillRule, decision: 'approve' | 'reject') => void;
  onEditRule: (draft: HarnessSkillDraft, rule: HarnessSkillRule, edit: RuleEditState) => Promise<boolean>;
  onDecideDraft: (draft: HarnessSkillDraft, decision: 'approve' | 'reject') => void;
  onEditDraft: (draft: HarnessSkillDraft, contentMarkdown: string) => Promise<boolean>;
  onPublishDraft: (draft: HarnessSkillDraft, visibility: 'private' | 'public') => void;
}) {
  const [editingDraftId, setEditingDraftId] = useState<string | null>(null);
  const [draftMarkdown, setDraftMarkdown] = useState('');
  const [ruleEdits, setRuleEdits] = useState<Record<string, RuleEditState>>({});
  const activeDraftKey = activeDraft?.skill_draft_id ?? null;
  const isEditingDraft = activeDraftKey !== null && editingDraftId === activeDraftKey;
  const readyDrafts = skillDrafts.filter((draft) => draft.status === 'ready_to_publish');

  useEffect(() => {
    setEditingDraftId(null);
    setDraftMarkdown('');
    setRuleEdits({});
  }, [activeDraftKey]);

  const startDraftEdit = useCallback(() => {
    if (!activeDraft) {
      return;
    }
    setEditingDraftId(activeDraft.skill_draft_id);
    setDraftMarkdown(activeDraft.content_markdown);
  }, [activeDraft]);

  const saveDraftEdit = useCallback(async () => {
    if (!activeDraft) {
      return;
    }
    const saved = await onEditDraft(activeDraft, draftMarkdown);
    if (saved) {
      setEditingDraftId(null);
      setDraftMarkdown('');
    }
  }, [activeDraft, draftMarkdown, onEditDraft]);

  const startRuleEdit = useCallback((rule: HarnessSkillRule) => {
    setRuleEdits((current) => ({
      ...current,
      [rule.skill_rule_id]: {
        statement: rule.statement,
        rationale: rule.rationale,
      },
    }));
  }, []);

  const updateRuleEdit = useCallback((ruleId: string, field: keyof RuleEditState, value: string) => {
    setRuleEdits((current) => ({
      ...current,
      [ruleId]: {
        ...(current[ruleId] ?? { statement: '', rationale: '' }),
        [field]: value,
      },
    }));
  }, []);

  const cancelRuleEdit = useCallback((ruleId: string) => {
    setRuleEdits((current) => {
      const next = { ...current };
      delete next[ruleId];
      return next;
    });
  }, []);

  const saveRuleEdit = useCallback(async (draft: HarnessSkillDraft, rule: HarnessSkillRule) => {
    const edit = ruleEdits[rule.skill_rule_id];
    if (!edit) {
      return;
    }
    const saved = await onEditRule(draft, rule, edit);
    if (saved) {
      cancelRuleEdit(rule.skill_rule_id);
    }
  }, [cancelRuleEdit, onEditRule, ruleEdits]);

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
                {template?.description ?? 'Create reviewed draft skills from selected sessions and files.'}
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
              disabled={busy || (selected.length === 0 && sourceFiles.length === 0)}
              className="w-full"
            >
              Run Skillify
            </Button>
          </div>
        </Card>

        <Card>
          <div className="flex items-center justify-between gap-3">
            <h2 className="text-base font-semibold">File Sources</h2>
            <span className="text-xs text-text-muted">{sourceFiles.length} selected</span>
          </div>
          <label className="mt-3 block rounded-control border border-dashed border-border bg-surface-muted px-3 py-3 text-sm text-text-secondary">
            <input
              type="file"
              multiple
              className="hidden"
              onChange={(event) => {
                onAddSourceFiles(event.target.files);
                event.currentTarget.value = '';
              }}
            />
            Add text files
          </label>
          {sourceFiles.length ? (
            <div className="mt-3 space-y-2">
              {sourceFiles.map((file, index) => (
                <div key={`${file.file_name}-${index}`} className="flex items-center justify-between gap-2 rounded-control border border-border px-3 py-2 text-sm">
                  <span className="min-w-0 truncate">{file.file_name}</span>
                  <Button size="sm" variant="ghost" leadingIcon={X} onClick={() => onRemoveSourceFile(index)}>
                    Remove
                  </Button>
                </div>
              ))}
            </div>
          ) : null}
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
                Review each draft skill as a whole, then inspect its cited rules.
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

	        {skillDrafts.length ? (
	          <div className="grid gap-4 2xl:grid-cols-[minmax(0,1fr)_420px]">
	            <Card>
	              <div className="flex flex-wrap items-center justify-between gap-3">
	                <div>
	                  <h2 className="text-base font-semibold">{activeDraft?.candidate_name ?? 'Draft Skill'}</h2>
	                  <p className="mt-1 text-sm text-text-secondary">{activeDraft?.description}</p>
	                </div>
	                {activeDraft ? (
	                  <div className="flex flex-wrap items-center gap-2">
	                    <span className={cn('rounded-full border px-2 py-1 text-xs', statusClass(activeDraft.status))}>
	                      {activeDraft.status}
	                    </span>
	                    {isEditingDraft ? (
	                      <>
	                        <Button size="sm" leadingIcon={Save} onClick={() => void saveDraftEdit()} disabled={busy}>
	                          Save
	                        </Button>
	                        <Button
	                          size="sm"
	                          variant="ghost"
	                          leadingIcon={X}
	                          onClick={() => {
	                            setEditingDraftId(null);
	                            setDraftMarkdown('');
	                          }}
	                          disabled={busy}
	                        >
	                          Cancel
	                        </Button>
	                      </>
	                    ) : (
	                      <Button
	                        size="sm"
	                        variant="ghost"
	                        leadingIcon={Pencil}
	                        onClick={startDraftEdit}
	                        disabled={busy || activeDraft.status === 'published'}
	                      >
	                        Edit
	                      </Button>
	                    )}
	                  </div>
	                ) : null}
	              </div>
              {skillDrafts.length > 1 ? (
                <div className="mt-3 flex flex-wrap gap-2">
                  {skillDrafts.map((draft) => (
                    <Button
                      key={draft.skill_draft_id}
                      size="sm"
                      variant={activeDraft?.skill_draft_id === draft.skill_draft_id ? 'primary' : 'ghost'}
                      onClick={() => onSelectDraft(draft.skill_draft_id)}
                    >
                      {draft.candidate_name}
                    </Button>
                  ))}
                </div>
              ) : null}
	              {isEditingDraft ? (
	                <Textarea
	                  value={draftMarkdown}
	                  onChange={(event) => setDraftMarkdown(event.target.value)}
	                  className="mt-4 min-h-[620px] font-mono text-xs"
	                />
	              ) : (
	                <pre className="mt-4 max-h-[620px] overflow-auto whitespace-pre-wrap rounded-control border border-border bg-bg p-4 text-xs text-text-secondary">
	                  {activeDraft?.content_markdown ?? ''}
	                </pre>
	              )}
            </Card>

            <div className="space-y-4">
              {activeDraft ? (
                <Card>
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <h2 className="text-base font-semibold">Rules</h2>
                    <div className="flex gap-2">
                      <Button
                        size="sm"
                        leadingIcon={Check}
                        onClick={() => onDecideDraft(activeDraft, 'approve')}
                        disabled={busy || activeDraft.status === 'ready_to_publish' || activeDraft.status === 'published'}
                      >
                        Approve skill
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        leadingIcon={X}
                        onClick={() => onDecideDraft(activeDraft, 'reject')}
                        disabled={busy || activeDraft.status === 'rejected' || activeDraft.status === 'published'}
                      >
                        Reject
                      </Button>
                    </div>
                  </div>
	                  <div className="mt-3 space-y-3">
	                    {activeDraft.rules.map((rule) => {
	                      const ruleEdit = ruleEdits[rule.skill_rule_id];
	                      return (
	                        <div key={rule.skill_rule_id} className="rounded-control border border-border bg-surface-muted p-3">
	                          <div className="flex flex-wrap items-center gap-2">
	                            <span className={cn('rounded-full border px-2 py-0.5 text-xs', statusClass(rule.status))}>
	                              {rule.status}
	                            </span>
	                          <span className="rounded-full border border-border bg-surface px-2 py-0.5 text-xs text-text-secondary">
	                            {rule.rule_type}
	                          </span>
                          {rule.confidence !== null ? (
                            <span className="text-xs text-text-muted">
                              confidence {Math.round(rule.confidence * 100)}%
                            </span>
	                            ) : null}
	                          </div>
	                          {ruleEdit ? (
	                            <div className="mt-3 space-y-2">
	                              <Textarea
	                                value={ruleEdit.statement}
	                                onChange={(event) => updateRuleEdit(rule.skill_rule_id, 'statement', event.target.value)}
	                                className="min-h-24 text-sm"
	                                aria-label="Rule statement"
	                              />
	                              <Textarea
	                                value={ruleEdit.rationale}
	                                onChange={(event) => updateRuleEdit(rule.skill_rule_id, 'rationale', event.target.value)}
	                                className="min-h-20 text-xs"
	                                aria-label="Rule rationale"
	                              />
	                            </div>
	                          ) : (
	                            <>
	                              <p className="mt-3 text-sm font-medium">{rule.statement}</p>
	                              <p className="mt-2 text-xs text-text-secondary">{rule.rationale}</p>
	                            </>
	                          )}
	                          <div className="mt-3 space-y-2">
	                            <div className="text-xs font-medium text-text-muted">
	                              {sourceCitationLabel(rule.source_count)}
	                            </div>
                          {rule.citations.length ? (
                            <div className="space-y-2">
                              {rule.citations.map((citation) => (
                                <div
                                  key={citation.citation_id}
                                  className="rounded-control border border-border bg-surface px-2.5 py-2"
                                >
                                  <div className="text-xs font-medium text-text-secondary">
                                    {citationSourceLabel(citation)}
                                  </div>
                                  {citation.evidence_text_preview ? (
                                    <p className="mt-1 text-xs leading-relaxed text-text-muted">
                                      {citation.evidence_text_preview}
                                    </p>
                                  ) : null}
                                </div>
                              ))}
                            </div>
	                          ) : (
	                            <p className="text-xs text-text-muted">No citation details returned.</p>
	                          )}
	                        </div>
	                        <div className="mt-3 flex gap-2">
	                          {ruleEdit ? (
	                            <>
	                              <Button
	                                size="sm"
	                                leadingIcon={Save}
	                                onClick={() => void saveRuleEdit(activeDraft, rule)}
	                                disabled={busy}
	                              >
	                                Save
	                              </Button>
	                              <Button
	                                size="sm"
	                                variant="ghost"
	                                leadingIcon={X}
	                                onClick={() => cancelRuleEdit(rule.skill_rule_id)}
	                                disabled={busy}
	                              >
	                                Cancel
	                              </Button>
	                            </>
	                          ) : (
	                            <>
	                              <Button
	                                size="sm"
	                                leadingIcon={Check}
	                                onClick={() => onDecideRule(activeDraft, rule, 'approve')}
	                                disabled={busy || rule.status === 'approved' || activeDraft.status === 'published'}
	                              >
	                                Approve
	                              </Button>
	                              <Button
	                                size="sm"
	                                variant="ghost"
	                                leadingIcon={X}
	                                onClick={() => onDecideRule(activeDraft, rule, 'reject')}
	                                disabled={busy || rule.status === 'rejected' || activeDraft.status === 'published'}
	                              >
	                                Reject
	                              </Button>
	                              <Button
	                                size="sm"
	                                variant="ghost"
	                                leadingIcon={Pencil}
	                                onClick={() => startRuleEdit(rule)}
	                                disabled={busy || activeDraft.status === 'published'}
	                              >
	                                Edit
	                              </Button>
	                            </>
	                          )}
	                        </div>
	                      </div>
	                    );
	                  })}
                  </div>
                </Card>
              ) : null}

	              {activeDraft ? (
	                <Card>
	                  <div>
	                    <h2 className="text-base font-semibold">Publish Queue</h2>
	                    <p className="mt-1 text-sm text-text-secondary">Approved skills can be published privately or publicly.</p>
	                  </div>
	                  {readyDrafts.length ? (
	                    <div className="mt-3 space-y-2">
	                      {readyDrafts.map((draft) => (
	                        <div
	                          key={draft.skill_draft_id}
	                          className="flex flex-wrap items-center justify-between gap-2 rounded-control border border-border bg-surface-muted px-3 py-2"
	                        >
	                          <div className="min-w-0">
	                            <div className="truncate text-sm font-medium">{draft.candidate_name}</div>
	                            <div className="mt-0.5 text-xs text-text-muted">{draft.rules.length} reviewed rules</div>
	                          </div>
	                          <div className="flex gap-2">
	                            <Button
	                              size="sm"
	                              leadingIcon={FileCode2}
	                              onClick={() => onPublishDraft(draft, 'private')}
	                              disabled={busy}
	                            >
	                              Private
	                            </Button>
	                            <Button
	                              size="sm"
	                              variant="ghost"
	                              leadingIcon={UploadCloud}
	                              onClick={() => onPublishDraft(draft, 'public')}
	                              disabled={busy}
	                            >
	                              Public
	                            </Button>
	                          </div>
	                        </div>
	                      ))}
	                    </div>
	                  ) : (
	                    <p className="mt-3 text-sm text-text-secondary">No approved skills are waiting to publish.</p>
	                  )}
	                  {published.length ? (
	                    <div className="mt-3 space-y-2">
                      {published.map((record) => (
                        <div key={record.version_id} className="rounded-control border border-success/30 bg-success/10 px-3 py-2 text-sm text-success">
                          {record.skill_name} published {record.visibility}
                        </div>
                      ))}
                    </div>
                  ) : null}
                </Card>
              ) : null}
            </div>
          </div>
        ) : (
          <Card>
            <p className="text-sm text-text-secondary">
              Select sessions or text files and run Skillify to generate reviewable draft skills.
            </p>
          </Card>
        )}
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
