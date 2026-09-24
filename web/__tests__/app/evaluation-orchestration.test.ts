import { requestJson } from '@/lib/api/request';
import { runPreparedEvaluation, type EvaluationPrepareResponse } from '@/lib/api/evaluations';
import { loadAuthoringResult } from '@/lib/api/harnesses';

vi.mock('@/lib/api/request', () => ({ requestJson: vi.fn() }));

const request = vi.mocked(requestJson);
const prepared: EvaluationPrepareResponse = {
  experiment: { experiment_id: 'comparison', spec_fingerprint: 'frozen' },
  adapter_profile_version: 'test',
  trials: [
    { trial_id: 'candidate', binding_status: 'planned', session_id: null, run_id: null,
      trial: { sequence: 1, case_id: 'case', arm: 'candidate', repetition: 0 } },
    { trial_id: 'baseline', binding_status: 'bound', session_id: 'session', run_id: 'run',
      trial: { sequence: 0, case_id: 'case', arm: 'baseline', repetition: 0 } },
  ],
};

describe('Evaluation browser orchestration', () => {
  beforeEach(() => vi.resetAllMocks());

  it('invalidates a recovered comparison when the durable draft revision changed', async () => {
    request.mockImplementation(async (path) => path.endsWith('/skill-drafts')
      ? [{ revision: 2 }]
      : { input_json: {}, output_json: { authoring: {
        inference: { usage_status: 'unavailable' }, evaluation: { status: 'prepared' },
        evaluated_draft_revision: 1, evaluation_plan: prepared,
      } } });
    const recovered = await loadAuthoringResult('frozen');
    expect(recovered.evaluation_plan).toBeNull();
    expect(recovered.evaluation.status).toBe('unavailable');
    expect(request.mock.calls.every(([, init]) => !init?.method || init.method === 'GET')).toBe(true);
  });

  it('resumes the bound baseline and repairs its observation before starting the candidate', async () => {
    let repaired = false;
    let candidateStarted = false;
    const report = { manifest: { coverage: { evidence_incomplete: true } } };
    request.mockImplementation(async (path, init) => {
      if (path.endsWith('/baseline/start')) throw new Error('baseline must not restart');
      if (path.endsWith('/candidate/start')) {
        expect(repaired).toBe(true);
        candidateStarted = true;
        return { run_id: 'candidate-run', session_id: 'candidate-session', status: 'running' };
      }
      if (path.endsWith('/assess')) {
        expect(init?.body).toBeUndefined();
        if (path.endsWith('/baseline/assess')) repaired = true;
        return { status: 'recorded', assessment: { outcome: { status: 'unavailable' } } };
      }
      if (path.endsWith('/report')) return report;
      return { trials: prepared.trials.map((binding) => ({ binding,
        lifecycle: binding.trial_id === 'baseline'
          ? (repaired ? 'observed' : 'terminal_awaiting_observation')
          : (candidateStarted ? 'observed' : 'planned'),
      })) };
    });

    const result = await runPreparedEvaluation(prepared, { waitSecs: 5, pollMs: 0 });
    expect(result).toBe(report);
    expect(candidateStarted).toBe(true);
    expect(request.mock.calls.filter(([path]) => path.endsWith('/start'))).toHaveLength(1);
  });

  it('stops without starting another arm or requesting a report when a trial is unavailable', async () => {
    request.mockResolvedValue({ trials: [{ binding: prepared.trials[1], lifecycle: 'unavailable' }] });

    await expect(runPreparedEvaluation(prepared, { waitSecs: 5 }))
      .rejects.toThrow('Trial baseline became unavailable');
    expect(request).toHaveBeenCalledTimes(1);
    expect(request.mock.calls[0][0]).toBe('/api/evaluations/experiments/comparison');
  });
});
