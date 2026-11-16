import {
  extractJwtSubject,
  headersInitToRecord,
  methodCanHaveJson,
  readAstraErrorDetail,
  utf8HeaderByteString,
} from '../http';
import { createServer } from 'node:http';

describe('http helpers', () => {
  test.each(['用户-张旭', 'café', 'user-🚀'])('preserves UTF-8 JWT subject %s', (subject) => {
    const payload = Buffer.from(JSON.stringify({ sub: subject })).toString('base64url');
    expect(extractJwtSubject(`e30.${payload}.`)).toBe(subject);
  });

  test('decodes UTF-8 without browser atob', () => {
    const payload = Buffer.from(JSON.stringify({ sub: '用户-🚀' })).toString('base64url');
    vi.stubGlobal('atob', undefined);
    try {
      expect(extractJwtSubject(`e30.${payload}.`)).toBe('用户-🚀');
    } finally {
      vi.unstubAllGlobals();
    }
  });

  test('rejects invalid UTF-8 payload bytes', () => {
    const payload = Buffer.concat([
      Buffer.from('{"sub":"'), Buffer.from([0xff]), Buffer.from('"}'),
    ]).toString('base64url');
    expect(extractJwtSubject(`e30.${payload}.`)).toBeNull();
  });

  test('headersInitToRecord merges plain records and Headers instances', () => {
    expect(headersInitToRecord({ a: '1' }, { b: '2' })).toEqual({ a: '1', b: '2' });

    const headers = new Headers();
    headers.set('c', '3');
    expect(headersInitToRecord({ a: '1' }, headers)).toEqual({ a: '1', c: '3' });
  });

  test('methodCanHaveJson rejects GET and HEAD only', () => {
    expect(methodCanHaveJson('GET')).toBe(false);
    expect(methodCanHaveJson('head')).toBe(false);
    expect(methodCanHaveJson('POST')).toBe(true);
  });

  test('readAstraErrorDetail prefers structured JSON fields', async () => {
    const response = new Response(JSON.stringify({ detail: 'bad request' }), {
      status: 400,
      headers: { 'content-type': 'application/json' },
    });

    await expect(readAstraErrorDetail(response)).resolves.toBe('bad request');
  });

  test('readAstraErrorDetail returns text for non-JSON bodies', async () => {
    const response = new Response('plain error', { status: 500 });

    await expect(readAstraErrorDetail(response)).resolves.toBe('plain error');
  });

  test('extractJwtSubject reads JWT subject without validating signature', () => {
    const header = Buffer.from(JSON.stringify({ alg: 'none' })).toString('base64url');
    const payload = Buffer.from(JSON.stringify({ sub: 'user-1' })).toString('base64url');

    expect(extractJwtSubject(`${header}.${payload}.`)).toBe('user-1');
    expect(extractJwtSubject('not-a-token')).toBeNull();
  });

  test.each(['用户-张旭', 'café', 'user-🚀'])(
    'utf8HeaderByteString keeps UTF-8 wire bytes for %s',
    (subject) => {
      const encoded = utf8HeaderByteString(subject);
      expect([...encoded].map((character) => character.charCodeAt(0))).toEqual(
        [...new TextEncoder().encode(subject)],
      );
      expect(() => new Headers({ 'X-User-Id': encoded })).not.toThrow();
    },
  );

  test('Unicode JWT subjects fail as raw Headers values but succeed after ByteString encoding', () => {
    expect(() => new Headers({ 'X-User-Id': '用户-张旭' })).toThrow(/ByteString|string/i);
    expect(() => new Headers({ 'X-User-Id': 'user-🚀' })).toThrow(/ByteString|string/i);
    // Accented text is accepted by Headers but would be sent as Latin-1 0xe9, not UTF-8.
    const accented = new Headers({ 'X-User-Id': 'café' });
    expect([...accented.get('X-User-Id') ?? ''].map((c) => c.charCodeAt(0))).toEqual([
      0x63, 0x61, 0x66, 0xe9,
    ]);
    const encoded = new Headers({ 'X-User-Id': utf8HeaderByteString('café') });
    expect([...encoded.get('X-User-Id') ?? ''].map((c) => c.charCodeAt(0))).toEqual([
      0x63, 0x61, 0x66, 0xc3, 0xa9,
    ]);
  });

});
