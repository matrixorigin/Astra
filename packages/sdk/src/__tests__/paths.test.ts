import {
  buildQueryString,
  chatRunDelegatePath,
  eventsSessionPath,
  joinApiPath,
  sessionActivityPath,
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
});
