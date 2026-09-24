'use client';

import { AlertTriangle, CheckCircle2, Play, RefreshCw, Scale } from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { EmptyState } from '@/components/ui/empty-state';
import { Input } from '@/components/ui/input';
import { PageHeader } from '@/components/ui/page-header';
import { Textarea } from '@/components/ui/textarea';
import {
  getEvaluationExperiment,
  getEvaluationExperimentBySubmission,
  getEvaluationReport,
  listEvaluationModels,
  listPersonalSkillSources,
  listPersonalSkillVersions,
  prepareEvaluation,
  runPreparedEvaluation,
  type EvaluationPrepareResponse,
  type EvaluationModel,
  type EvaluationProjection,
  type EvaluationReport,
  type PersonalSkillSource,
  type PersonalSkillVersion,
} from '@/lib/api/evaluations';
import { listModels } from '@/lib/api/models';
import { WebApiError } from '@/lib/api/errors';

type PrimaryModel = Awaited<ReturnType<typeof listModels>>['items'][number];
const defaultExpected = '{\n  "ok": true\n}';
type ComparisonKind = 'skill' | 'skill_routing_judgment';
type SavedEvaluationReference = {
  version: 2;
  ownerId: string;
  runtimeKey: string;
  experimentId: string;
};

type PendingEvaluationSubmission = {
  version: 1;
  ownerId: string;
  runtimeKey: string;
  submissionIdempotencyKey: string;
};

type EvaluationPageProps = {
  ownerId: string;
  runtimeKey: string;
};

function statusTone(value: string) {
  if (value === 'observed' || value === 'recorded' || value === 'pass') {
    return 'border-success/30 bg-success/10 text-success';
  }
  if (value === 'unavailable' || value === 'fail' || value === 'failed') {
    return 'border-danger/30 bg-danger/10 text-danger';
  }
  return 'border-border bg-surface-muted text-text-secondary';
}

export function EvaluationPage({ ownerId, runtimeKey }: EvaluationPageProps) {
  const [sources, setSources] = useState<PersonalSkillSource[]>([]);
  const [versions, setVersions] = useState<PersonalSkillVersion[]>([]);
  const [primaryModels, setPrimaryModels] = useState<PrimaryModel[]>([]);
  const [judgmentModels, setJudgmentModels] = useState<EvaluationModel[]>([]);
  const [skillName, setSkillName] = useState('');
  const [comparisonKind, setComparisonKind] = useState<ComparisonKind>('skill');
  const [baselineVersionId, setBaselineVersionId] = useState('');
  const [candidateVersionId, setCandidateVersionId] = useState('');
  const [primaryOfferingId, setPrimaryOfferingId] = useState('');
  const [judgmentOfferingId, setJudgmentOfferingId] = useState('');
  const [workspaceEnabled, setWorkspaceEnabled] = useState(false);
  const [edgeExecutorId, setEdgeExecutorId] = useState('');
  const [sourceCommit, setSourceCommit] = useState('');
  const [workspaceTools, setWorkspaceTools] = useState('read_file,write_file,bash');
  const [verifierCommand, setVerifierCommand] = useState('make check');
  const [caseId, setCaseId] = useState('routing-case');
  const [message, setMessage] = useState('Apply the pinned Skill and return exactly the expected JSON.');
  const [expectedJson, setExpectedJson] = useState(defaultExpected);
  const [wallTimeSecs, setWallTimeSecs] = useState('120');
  const [projection, setProjection] = useState<EvaluationProjection | null>(null);
  const [report, setReport] = useState<EvaluationReport | null>(null);
  const [experimentId, setExperimentId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState('Choose a published Skill revision to begin.');
  const [savedIntent, setSavedIntent] = useState<SavedEvaluationReference | null>(null);
  const [pendingSubmission, setPendingSubmission] = useState<PendingEvaluationSubmission | null>(null);
  const [savedIntentLoaded, setSavedIntentLoaded] = useState(false);
  const savedIntentKey = `astra:evaluation:skill-routing:v2:${encodeURIComponent(ownerId)}:${encodeURIComponent(runtimeKey)}`;
  const pendingSubmissionKey = `${savedIntentKey}:pending`;

  const publishedVersions = useMemo(
    () => versions.filter((version) => version.status === 'published'),
    [versions],
  );

  const loadCatalog = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [skillResult, primaryResult, judgmentResult] = await Promise.allSettled([
        listPersonalSkillSources(),
        listModels(),
        listEvaluationModels(),
      ]);
      const errors: string[] = [];
      if (skillResult.status === 'fulfilled') {
        const availableSources = skillResult.value.filter((source) => source.status !== 'deleted');
        setSources(availableSources);
        setSkillName((current) => current || availableSources[0]?.skill_name || '');
      } else {
        errors.push(skillResult.reason instanceof Error ? skillResult.reason.message : 'Skill catalog unavailable.');
      }
      if (primaryResult.status === 'fulfilled') {
        setPrimaryModels(primaryResult.value.items);
        setPrimaryOfferingId((current) => current || primaryResult.value.items[0]?.id || '');
      } else {
        errors.push(primaryResult.reason instanceof Error ? primaryResult.reason.message : 'Primary model catalog unavailable.');
      }
      if (judgmentResult.status === 'fulfilled') {
        setJudgmentModels(judgmentResult.value.items.filter((model) => model.is_active));
      } else {
        errors.push(judgmentResult.reason instanceof Error ? judgmentResult.reason.message : 'Judgment model catalog unavailable.');
      }
      if (errors.length > 0) {
        setError(`Some Evaluation catalog data is unavailable: ${errors.join(' ')}`);
      }
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadCatalog();
  }, [loadCatalog]);

  useEffect(() => {
    let active = true;
    const recoverSavedSubmission = async () => {
      try {
        window.localStorage.removeItem('astra:evaluation:skill-routing:v1');
        const savedRaw = window.localStorage.getItem(savedIntentKey);
        if (savedRaw) {
          const parsed: unknown = JSON.parse(savedRaw);
          if (
            parsed &&
            typeof parsed === 'object' &&
            (parsed as { version?: unknown }).version === 2 &&
            (parsed as { ownerId?: unknown }).ownerId === ownerId &&
            (parsed as { runtimeKey?: unknown }).runtimeKey === runtimeKey &&
            typeof (parsed as { experimentId?: unknown }).experimentId === 'string'
          ) {
            if (active) {
              setSavedIntent(parsed as SavedEvaluationReference);
              setStatus('A saved comparison is available to resume.');
            }
          } else {
            window.localStorage.removeItem(savedIntentKey);
          }
        }
        if (active) setSavedIntentLoaded(true);

        const pendingRaw = window.localStorage.getItem(pendingSubmissionKey);
        if (!pendingRaw) return;
        const pending: unknown = JSON.parse(pendingRaw);
        if (
          !pending ||
          typeof pending !== 'object' ||
          (pending as { version?: unknown }).version !== 1 ||
          (pending as { ownerId?: unknown }).ownerId !== ownerId ||
          (pending as { runtimeKey?: unknown }).runtimeKey !== runtimeKey ||
          typeof (pending as { submissionIdempotencyKey?: unknown }).submissionIdempotencyKey !== 'string'
        ) {
          window.localStorage.removeItem(pendingSubmissionKey);
          return;
        }
        if (active) setPendingSubmission(pending as PendingEvaluationSubmission);
        try {
          const record = await getEvaluationExperimentBySubmission(
            (pending as PendingEvaluationSubmission).submissionIdempotencyKey,
          );
          if (!active) return;
          if (window.localStorage.getItem(pendingSubmissionKey) !== pendingRaw) return;
          const saved: SavedEvaluationReference = {
            version: 2,
            ownerId,
            runtimeKey,
            experimentId: record.experiment_id,
          };
          setSavedIntent(saved);
          window.localStorage.setItem(savedIntentKey, JSON.stringify(saved));
          window.localStorage.removeItem(pendingSubmissionKey);
          setPendingSubmission(null);
          setStatus('A submitted comparison was recovered and is ready to resume.');
        } catch {
          if (active) {
            setStatus('A comparison submission is awaiting confirmation; retrying will reuse its submission identity.');
          }
        }
      } catch {
        window.localStorage.removeItem(savedIntentKey);
        window.localStorage.removeItem(pendingSubmissionKey);
      } finally {
        if (active) setSavedIntentLoaded(true);
      }
    };
    void recoverSavedSubmission();
    return () => {
      active = false;
    };
  }, [ownerId, pendingSubmissionKey, runtimeKey, savedIntentKey]);

  useEffect(() => {
    if (!skillName) {
      setVersions([]);
      setBaselineVersionId('');
      setCandidateVersionId('');
      return;
    }
    let active = true;
    void listPersonalSkillVersions(skillName)
      .then((payload) => {
        if (!active) return;
        setVersions(payload);
        const published = payload.filter((version) => version.status === 'published');
        setBaselineVersionId((current) => current || published[0]?.version_id || '');
        setCandidateVersionId((current) => current || published[1]?.version_id || published[0]?.version_id || '');
      })
      .catch((reason: unknown) => {
        if (active) setError(reason instanceof Error ? reason.message : 'Failed to load Skill revisions.');
      });
    return () => {
      active = false;
    };
  }, [skillName]);

  const resetResult = useCallback(() => {
    setExperimentId(null);
    setProjection(null);
    setReport(null);
    setError(null);
    setStatus('Choose a published Skill revision to begin.');
  }, []);

  const recoverReview = useCallback(async (id: string) => {
    const [projectionResult, reportResult] = await Promise.allSettled([
      getEvaluationExperiment(id),
      getEvaluationReport(id),
    ]);
    let recovered = false;
    if (projectionResult.status === 'fulfilled') {
      setProjection(projectionResult.value);
      recovered = true;
    }
    if (reportResult.status === 'fulfilled') {
      setReport(reportResult.value);
      recovered = true;
    }
    if (recovered) {
      setExperimentId(id);
      setStatus(`Experiment ${id} is available; the report shows any remaining evidence gaps.`);
    }
    return recovered;
  }, []);

  const retryPendingSubmission = useCallback(async () => {
    if (!pendingSubmission) return;
    setError(null);
    setBusy(true);
    try {
      const record = await getEvaluationExperimentBySubmission(pendingSubmission.submissionIdempotencyKey);
      const saved: SavedEvaluationReference = {
        version: 2,
        ownerId,
        runtimeKey,
        experimentId: record.experiment_id,
      };
      setSavedIntent(saved);
      setPendingSubmission(null);
      window.localStorage.setItem(savedIntentKey, JSON.stringify(saved));
      window.localStorage.removeItem(pendingSubmissionKey);
      setStatus('A submitted comparison was recovered and is ready to resume.');
    } catch (reason) {
      setStatus(reason instanceof Error ? reason.message : 'The pending submission is not confirmed yet; retry the lookup.');
    } finally {
      setBusy(false);
    }
  }, [ownerId, pendingSubmission, pendingSubmissionKey, runtimeKey, savedIntentKey]);

  const discardPendingSubmission = useCallback(() => {
    setPendingSubmission(null);
    try {
      window.localStorage.removeItem(pendingSubmissionKey);
    } catch {
      // The in-memory state still prevents this page from reusing the key.
    }
    setStatus('Pending submission discarded; the next comparison will use a new submission identity.');
  }, [pendingSubmissionKey]);

  const executeIntent = useCallback(async (payload: Record<string, unknown>) => {
    setError(null);
    setReport(null);
    setBusy(true);
    resetResult();
    let currentExperimentId: string | null = null;
    try {
      const submissionIdempotencyKey = payload.submission_idempotency_key;
      if (typeof submissionIdempotencyKey === 'string') {
        const pending = {
          version: 1,
          ownerId,
          runtimeKey,
          submissionIdempotencyKey,
        } satisfies PendingEvaluationSubmission;
        setPendingSubmission(pending);
        try {
          window.localStorage.setItem(pendingSubmissionKey, JSON.stringify(pending));
        } catch {
          // The server-side idempotency key remains authoritative.
        }
      }
      setStatus('Freezing Skill, model, judgment policy, and task criterion…');
      const prepared = await prepareEvaluation(payload);
      currentExperimentId = prepared.experiment.experiment_id;
      setExperimentId(currentExperimentId);
      const saved: SavedEvaluationReference = {
        version: 2,
        ownerId,
        runtimeKey,
        experimentId: currentExperimentId,
      };
      setSavedIntent(saved);
      setPendingSubmission(null);
      try {
        window.localStorage.setItem(savedIntentKey, JSON.stringify(saved));
        window.localStorage.removeItem(pendingSubmissionKey);
      } catch {
        // The server-side experiment identity remains the recovery authority.
      }
      setStatus(`Prepared ${prepared.trials.length} trials; running the baseline arm first…`);
      const wall = Number(payload.max_wall_time_secs);
      const finalReport = await runPreparedEvaluation(prepared, {
        waitSecs: wall * prepared.trials.length + 60,
        onProjection: (next) => {
          setProjection(next);
          const current = next.trials.find((trial) => trial.lifecycle !== 'observed');
          setStatus(current ? `${current.binding.trial_id}: ${current.lifecycle}` : 'All trials observed; assessing report…');
        },
      });
      setReport(finalReport);
      setProjection(await getEvaluationExperiment(currentExperimentId));
      setStatus('Evaluation report is ready.');
    } catch (reason) {
      const recovered = currentExperimentId ? await recoverReview(currentExperimentId) : false;
      if (reason instanceof WebApiError && [400, 409, 422, 501].includes(reason.status)) {
        setPendingSubmission(null);
        try {
          window.localStorage.removeItem(pendingSubmissionKey);
        } catch {
          // The rejected request cannot be retried safely with this key.
        }
      }
      setError(reason instanceof Error ? reason.message : 'Evaluation failed.');
      if (currentExperimentId && !recovered) setStatus(`Experiment ${currentExperimentId} remains available for review.`);
    } finally {
      setBusy(false);
    }
  }, [ownerId, pendingSubmissionKey, recoverReview, resetResult, runtimeKey, savedIntentKey]);

  const runComparison = useCallback(async () => {
    setError(null);
    if (pendingSubmission) {
      await retryPendingSubmission();
      return;
    }
    if (!skillName || !baselineVersionId || (comparisonKind === 'skill' && !candidateVersionId) || !primaryOfferingId) {
      setError('Select the pinned published Skill revisions and a primary Offering first.');
      return;
    }
    let expected: unknown = null;
    if (!workspaceEnabled) {
      try {
        expected = JSON.parse(expectedJson);
      } catch (reason) {
        setError(`Expected JSON is invalid: ${reason instanceof Error ? reason.message : String(reason)}`);
        return;
      }
    }
    const wall = Number(wallTimeSecs);
    if (!Number.isSafeInteger(wall) || wall < 1) {
      setError('Wall time must be a positive whole number of seconds.');
      return;
    }
    const workspace = workspaceEnabled
      ? {
          edge_executor_id: edgeExecutorId.trim(),
          source_commit: sourceCommit.trim(),
          tool_names: workspaceTools.split(',').map((tool) => tool.trim()).filter(Boolean),
        }
      : undefined;
    if (workspaceEnabled && (!workspace?.edge_executor_id || !workspace.source_commit || workspace.tool_names.length === 0)) {
      setError('Fill in the Edge executor, full source commit, and at least one workspace tool.');
      return;
    }
    if (workspaceEnabled && !verifierCommand.trim()) {
      setError('Enter a frozen verifier command for the workspace comparison.');
      return;
    }
    const payload: Record<string, unknown> = {
      submission_idempotency_key: `web-skill-routing-${crypto.randomUUID()}`,
      target: {
        kind: comparisonKind,
        skill_name: skillName,
        baseline: { revision_id: baselineVersionId },
        candidate: { revision_id: comparisonKind === 'skill' ? candidateVersionId : baselineVersionId },
      },
      case: {
        case_id: caseId.trim() || 'routing-case',
        message: message.trim(),
        verifier_config: workspaceEnabled
          ? { kind: 'workspace_command', command: verifierCommand.trim(), expected_exit_code: 0, timeout_secs: wall }
          : { kind: 'json_value_equals', expected },
      },
      model_offering_id: primaryOfferingId,
      ...(comparisonKind === 'skill_routing_judgment' && judgmentOfferingId
        ? { judgment_model_offering_id: judgmentOfferingId }
        : {}),
      max_concurrency: 1,
      max_wall_time_secs: wall,
      ...(workspace ? { workspace } : {}),
    };
    await executeIntent(payload);
  }, [baselineVersionId, candidateVersionId, caseId, comparisonKind, edgeExecutorId, executeIntent, expectedJson, judgmentOfferingId, message, pendingSubmission, primaryOfferingId, retryPendingSubmission, skillName, sourceCommit, verifierCommand, wallTimeSecs, workspaceEnabled, workspaceTools]);

  const resumeSavedComparison = useCallback(async () => {
    if (!savedIntent) return;
    setError(null);
    setBusy(true);
    setExperimentId(savedIntent.experimentId);
    try {
      setStatus(`Resuming experiment ${savedIntent.experimentId} from its durable plan…`);
      const current = await getEvaluationExperiment(savedIntent.experimentId);
      setProjection(current);
      const prepared: EvaluationPrepareResponse = {
        experiment: current.experiment,
        trials: current.trials.map((trial) => trial.binding),
        adapter_profile_version: 'persisted-evaluation-plan',
      };
      const finalReport = await runPreparedEvaluation(prepared, {
        waitSecs: 600,
        onProjection: setProjection,
      });
      setReport(finalReport);
      setProjection(await getEvaluationExperiment(savedIntent.experimentId));
      setStatus('Evaluation report is ready.');
    } catch (reason) {
      const recovered = await recoverReview(savedIntent.experimentId);
      setError(reason instanceof Error ? reason.message : 'Saved Evaluation resume failed.');
      if (!recovered) setStatus(`Experiment ${savedIntent.experimentId} remains available for review.`);
    } finally {
      setBusy(false);
    }
  }, [recoverReview, savedIntent]);

  if (!savedIntentLoaded || (loading && !savedIntent && !pendingSubmission)) {
    return <div className="flex h-full items-center justify-center text-sm text-text-secondary">Loading Evaluation catalog…</div>;
  }

  if (sources.length === 0 && !savedIntent && !pendingSubmission) {
    return (
      <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
        <div className="mx-auto max-w-5xl">
          <PageHeader title="Evaluation" description="Compare pinned Skill revisions or measure a frozen routing judgment." />
          <div className="mt-8">
            {error ? (
              <Card>
                <div className="flex gap-2 rounded-control border border-danger/30 bg-danger/10 p-3 text-sm text-danger"><AlertTriangle className="mt-0.5 size-4 shrink-0" /><span>{error}</span></div>
                <Button variant="ghost" leadingIcon={RefreshCw} onClick={loadCatalog} className="mt-4">Retry catalog</Button>
              </Card>
            ) : (
              <EmptyState
                icon={Scale}
                title="No personal Skills available"
                description="Publish an instruction-only Skill in Harnesses first, then return here to evaluate its behavior."
              />
            )}
          </div>
        </div>
      </div>
    );
  }

  const selectedBaseline = publishedVersions.find((version) => version.version_id === baselineVersionId);
  const selectedCandidate = publishedVersions.find((version) => version.version_id === candidateVersionId);
  const coverage = report?.manifest.coverage;

  return (
    <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
      <div className="mx-auto max-w-6xl">
        <PageHeader
          title="Evaluation"
          description="Compare two pinned Skill revisions, or isolate the effect of a frozen routing judgment."
          action={<Button variant="ghost" leadingIcon={RefreshCw} onClick={loadCatalog} disabled={busy}>Refresh catalog</Button>}
        />

        <div className="mt-8 grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(320px,0.7fr)]">
          <Card>
            <div className="flex items-start gap-3">
              <span className="flex size-9 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent"><Scale className="size-4" /></span>
              <div>
                <h2 className="text-base font-semibold">Skill comparison</h2>
                <p className="mt-1 text-sm leading-6 text-text-secondary">The server freezes the exact Skill revisions, model, verifier, and runtime conditions before either trial starts.</p>
              </div>
            </div>

            <div className="mt-6 grid gap-4 sm:grid-cols-2">
              <SelectField label="Comparison" value={comparisonKind} onChange={(value) => setComparisonKind(value as ComparisonKind)} disabled={busy}>
                <option value="skill">Skill revisions</option>
                <option value="skill_routing_judgment">Routing judgment</option>
              </SelectField>
              <SelectField label="Skill" value={skillName} onChange={(value) => { setSkillName(value); setBaselineVersionId(''); setCandidateVersionId(''); }} disabled={busy}>
                {sources.map((source) => <option key={source.skill_name} value={source.skill_name}>{source.skill_name}</option>)}
              </SelectField>
              <SelectField label="Baseline revision" value={baselineVersionId} onChange={setBaselineVersionId} disabled={busy || publishedVersions.length === 0}>
                {publishedVersions.map((version) => <option key={version.version_id} value={version.version_id}>{version.version} · {version.version_id}</option>)}
              </SelectField>
              {comparisonKind === 'skill' ? (
                <SelectField label="Candidate revision" value={candidateVersionId} onChange={setCandidateVersionId} disabled={busy || publishedVersions.length === 0}>
                  {publishedVersions.map((version) => <option key={version.version_id} value={version.version_id}>{version.version} · {version.version_id}</option>)}
                </SelectField>
              ) : null}
              <SelectField label="Primary Offering" value={primaryOfferingId} onChange={setPrimaryOfferingId} disabled={busy}>
                {primaryModels.map((model) => <option key={model.id} value={model.id}>{model.name} · {model.id}</option>)}
              </SelectField>
              {comparisonKind === 'skill_routing_judgment' ? (
                <SelectField label={<>Judgment Offering <span className="font-normal text-text-muted">optional</span></>} value={judgmentOfferingId} onChange={setJudgmentOfferingId} disabled={busy}>
                  <option value="">Use configured default</option>
                  {judgmentModels.map((model) => <option key={model.offering_id} value={model.offering_id}>{model.name} · {model.provider}</option>)}
                </SelectField>
              ) : null}
            </div>

            <div className="mt-5 rounded-control border border-border bg-surface-muted p-4">
              <label className="flex items-center gap-2 text-sm font-medium">
                <input type="checkbox" checked={workspaceEnabled} onChange={(event) => setWorkspaceEnabled(event.target.checked)} disabled={busy} />
                Run both arms on a pinned Edge workspace
              </label>
              <p className="mt-2 text-xs leading-5 text-text-muted">The server freezes the checkout commit and tool surface. The authenticated Edge must prove the same source and materialization before each Run.</p>
              {workspaceEnabled ? (
                <div className="mt-3 grid gap-4 sm:grid-cols-2">
                  <label className="text-sm font-medium">Edge executor ID
                    <Input value={edgeExecutorId} onChange={(event) => setEdgeExecutorId(event.target.value)} disabled={busy} placeholder="edge-agent-id" className="mt-1.5" />
                  </label>
                  <label className="text-sm font-medium">Source commit
                    <Input value={sourceCommit} onChange={(event) => setSourceCommit(event.target.value)} disabled={busy} placeholder="40 or 64 hex characters" className="mt-1.5 font-mono text-xs" />
                  </label>
                  <label className="text-sm font-medium sm:col-span-2">Workspace tools
                    <Input value={workspaceTools} onChange={(event) => setWorkspaceTools(event.target.value)} disabled={busy} placeholder="read_file,write_file,bash" className="mt-1.5 font-mono text-xs" />
                    <span className="mt-1.5 block text-xs font-normal text-text-muted">Comma-separated built-in tools. Only the frozen list is visible to the model.</span>
                  </label>
                  <label className="text-sm font-medium sm:col-span-2">Verifier command
                    <Input value={verifierCommand} onChange={(event) => setVerifierCommand(event.target.value)} disabled={busy} placeholder="make check" className="mt-1.5 font-mono text-xs" />
                    <span className="mt-1.5 block text-xs font-normal text-text-muted">Runs once after the agent settles, without network access. The patch, command output, exit code, and workspace identity become durable evidence.</span>
                  </label>
                </div>
              ) : null}
            </div>

            <div className="mt-4 grid gap-4 sm:grid-cols-[minmax(0,1fr)_150px]">
              <label className="text-sm font-medium">Case message
                <Textarea value={message} onChange={(event) => setMessage(event.target.value)} disabled={busy} className="mt-1.5 min-h-24" />
              </label>
              <label className="text-sm font-medium">Case ID
                <Input value={caseId} onChange={(event) => setCaseId(event.target.value)} disabled={busy} className="mt-1.5" />
                <span className="mt-2 block text-xs font-normal text-text-muted">The expected value stays in the verifier, outside the prompt.</span>
              </label>
            </div>

            {!workspaceEnabled ? <label className="mt-4 block text-sm font-medium">Expected JSON
              <Textarea value={expectedJson} onChange={(event) => setExpectedJson(event.target.value)} disabled={busy} className="mt-1.5 min-h-32 font-mono text-xs" />
            </label> : null}

            <div className="mt-4 flex flex-wrap items-end justify-between gap-4">
              <label className="text-sm font-medium">Max wall time per trial
                <Input type="number" min={1} value={wallTimeSecs} onChange={(event) => setWallTimeSecs(event.target.value)} disabled={busy} className="mt-1.5 w-40" />
              </label>
              <Button leadingIcon={Play} onClick={runComparison} disabled={busy || Boolean(pendingSubmission) || !selectedBaseline || (comparisonKind === 'skill' && !selectedCandidate) || !message.trim()}>{busy ? 'Running…' : 'Run comparison'}</Button>
            </div>
            {comparisonKind === 'skill_routing_judgment' ? <p className="mt-4 text-xs leading-5 text-text-muted">Jev is an enhancement when selected or configured. If it is unavailable, the candidate keeps the basic Skill path and the report cannot establish a Jev benefit.</p> : null}
            {pendingSubmission ? (
              <div className="mt-3 rounded-control border border-border bg-surface-muted p-3 text-xs leading-5 text-text-secondary">
                <p>A submitted comparison is awaiting confirmation. The lookup uses its original submission identity and does not resend the current form.</p>
                <div className="mt-2 flex flex-wrap gap-2">
                  <Button variant="ghost" onClick={retryPendingSubmission} disabled={busy}>Check pending submission</Button>
                  <Button variant="ghost" onClick={discardPendingSubmission} disabled={busy}>Start a new comparison</Button>
                </div>
              </div>
            ) : null}
            {savedIntent ? <Button variant="ghost" onClick={resumeSavedComparison} disabled={busy} className="mt-3">Resume saved comparison</Button> : null}
          </Card>

          <div className="space-y-5">
            <Card>
              <div className="flex items-center gap-2 text-sm font-semibold"><span className="size-2 rounded-full bg-accent" />Run status</div>
              <p className="mt-3 text-sm leading-6 text-text-secondary">{status}</p>
              {experimentId ? <p className="mt-3 break-all font-mono text-xs text-text-muted">{experimentId}</p> : null}
              {error ? <div className="mt-4 flex gap-2 rounded-control border border-danger/30 bg-danger/10 p-3 text-sm text-danger"><AlertTriangle className="mt-0.5 size-4 shrink-0" /><span>{error}</span></div> : null}
            </Card>

            {projection ? <TrialEvidence projection={projection} /> : null}

            {report && coverage ? (
              <Card>
                <div className="flex items-center gap-2 text-sm font-semibold"><CheckCircle2 className="size-4 text-success" />Report coverage</div>
                <div className="mt-4 grid grid-cols-2 gap-3 text-sm">
                  <Metric label="Observed" value={`${coverage.observed_trial_count}/${coverage.planned_trial_count}`} />
                  <Metric label="Metric gaps" value={`${coverage.metric_gaps.length}`} />
                </div>
                <p className="mt-4 text-sm leading-6 text-text-secondary">{report.report.conclusion}</p>
                {coverage.evidence_incomplete ? <p className="mt-3 text-xs text-warning">Evidence is incomplete; this comparison does not establish an improvement.</p> : null}
                <details className="mt-4">
                  <summary className="cursor-pointer text-xs font-medium text-text-secondary">View Markdown report</summary>
                  <pre className="mt-3 max-h-96 overflow-auto whitespace-pre-wrap rounded-control bg-surface-muted p-3 text-xs leading-5 text-text-secondary">{report.markdown}</pre>
                </details>
              </Card>
            ) : null}
          </div>
        </div>
      </div>
    </div>
  );
}

function SelectField({ label, value, onChange, disabled, children }: { label: React.ReactNode; value: string; onChange: (value: string) => void; disabled?: boolean; children: React.ReactNode }) {
  return (
    <label className="text-sm font-medium">{label}
      <select value={value} onChange={(event) => onChange(event.target.value)} disabled={disabled} className="mt-1.5 h-10 w-full rounded-control border border-border bg-surface px-3 text-sm font-normal text-text outline-none focus:border-accent">
        {children}
      </select>
    </label>
  );
}

function TrialEvidence({ projection }: { projection: EvaluationProjection }) {
  return (
    <Card>
      <h2 className="text-sm font-semibold">Trial evidence</h2>
      <div className="mt-3 space-y-2">
        {projection.trials.map((trial) => (
          <div key={trial.binding.trial_id} className="flex items-center justify-between gap-3 rounded-control border border-border bg-surface-muted px-3 py-2 text-xs">
          <div className="min-w-0"><span className="font-medium">{trial.binding.trial.arm}</span><span className="ml-2 truncate text-text-muted">{trial.binding.trial_id}</span></div>
            <span className={`shrink-0 rounded-full border px-2 py-0.5 ${statusTone(trial.task_assessment?.outcome.status ?? trial.lifecycle)}`}>{trial.task_assessment?.outcome.status ?? trial.lifecycle}</span>
          </div>
        ))}
      </div>
    </Card>
  );
}

function Metric({ label, value }: { label: string; value: string }) {
  return <div className="rounded-control border border-border bg-surface-muted px-3 py-2"><div className="text-xs text-text-muted">{label}</div><div className="mt-1 font-semibold">{value}</div></div>;
}
