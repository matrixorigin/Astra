import {
  buildQueryString,
  chatRunDelegatePath,
  eventsSessionPath,
  joinApiPath,
  modelCheckPath,
  modelPath,
  sessionArtifactDownloadPath,
  sessionArtifactLatestPath,
  sessionArtifactPath,
  sessionArtifactsPath,
  sessionActivityPath,
  sessionTranscriptPath,
  skillPath,
  skillUnpublishPath,
} from '../paths';

describe('paths — buildQueryString', () => {
  test('returns empty string when no params', () => {
    expect(buildQueryString({})).toBe('');
  });

  test('skips undefined and null', () => {
    expect(
      buildQueryString({
        a: 1,
        b: undefined,
        c: null,
        d: 'x',
      }),
    ).toBe('?a=1&d=x');
  });

  test('encodes values', () => {
    const q = buildQueryString({ session_id: 'a b' });
    const params = new URLSearchParams(q.startsWith('?') ? q.slice(1) : q);
    expect(params.get('session_id')).toBe('a b');
  });
});

describe('paths — joinApiPath', () => {
  test('joins prefix and path', () => {
    expect(joinApiPath('/api', '/auth/login')).toBe('/api/auth/login');
  });

  test('empty prefix returns path', () => {
    expect(joinApiPath('', '/runs')).toBe('/runs');
    expect(joinApiPath(undefined, '/runs')).toBe('/runs');
  });
});

describe('paths — helpers encode ids', () => {
  test('sessionActivityPath', () => {
    expect(sessionActivityPath('s/1')).toContain(encodeURIComponent('s/1'));
  });

  test('sessionTranscriptPath', () => {
    expect(sessionTranscriptPath('s/1')).toBe(`/sessions/${encodeURIComponent('s/1')}/transcript`);
  });

  test('sessionArtifactsPath', () => {
    expect(sessionArtifactsPath('s1')).toBe('/sessions/s1/artifacts');
  });

  test('sessionArtifactLatestPath matches Rust safe segment behavior', () => {
    expect(sessionArtifactLatestPath('s1', 'llm_capture')).toBe(
      '/sessions/s1/artifacts/latest/llm_capture',
    );
    expect(sessionArtifactLatestPath('s1', '../../admin')).toBeNull();
    expect(sessionArtifactLatestPath('s1', 'a/b')).toBeNull();
    expect(sessionArtifactLatestPath('s1', '..')).toBeNull();
    expect(sessionArtifactLatestPath('s1', '')).toBeNull();
    expect(sessionArtifactLatestPath('s1', 'a?b')).toBeNull();
    expect(sessionArtifactLatestPath('s1', 'a#b')).toBeNull();
  });

  test('sessionArtifactDownloadPath matches Rust safe segment behavior', () => {
    expect(sessionArtifactPath('s1', 'a1')).toBe('/sessions/s1/artifacts/a1');
    expect(sessionArtifactDownloadPath('s1', 'a1')).toBe('/sessions/s1/artifacts/a1/download');
    expect(sessionArtifactPath('s1', '../secret')).toBeNull();
    expect(sessionArtifactDownloadPath('s1', '../secret')).toBeNull();
    expect(sessionArtifactDownloadPath('s1', 'a%2Fb')).toBeNull();
  });

  test('chatRunDelegatePath', () => {
    expect(chatRunDelegatePath('r-1')).toBe('/chat/runs/r-1/delegate');
  });

  test('eventsSessionPath', () => {
    expect(eventsSessionPath('sid')).toBe('/events/session/sid');
  });

  test('skillPath encodes id', () => {
    expect(skillPath('n@1.0.0')).toBe(`/skills/${encodeURIComponent('n@1.0.0')}`);
  });

  test('skillUnpublishPath encodes name', () => {
    expect(skillUnpublishPath('a/b')).toBe(`/skills/${encodeURIComponent('a/b')}/unpublish`);
  });

  test('model paths encode model names', () => {
    expect(modelPath('bedrock/claude')).toBe(`/models/${encodeURIComponent('bedrock/claude')}`);
    expect(modelCheckPath('gpt-4')).toBe('/models/gpt-4/check');
  });
});
