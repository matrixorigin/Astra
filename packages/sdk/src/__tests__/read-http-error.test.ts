import { readHttpErrorMessage } from '../sse-client';

function responseOf(
  status: number,
  body: string,
  statusText = 'Error',
  textError?: Error,
): Response {
  return {
    status,
    statusText,
    text: textError
      ? () => Promise.reject(textError)
      : () => Promise.resolve(body),
  } as unknown as Response;
}

describe('readHttpErrorMessage', () => {
  it.each([
    {
      name: 'Axum-style detail',
      res: responseOf(500, JSON.stringify({ detail: 'Resource limit' })),
      want: 'Resource limit',
    },
    {
      name: 'message field',
      res: responseOf(400, JSON.stringify({ message: 'bad input' })),
      want: 'bad input',
    },
    {
      name: 'error as string',
      res: responseOf(502, JSON.stringify({ error: 'upstream' })),
      want: 'upstream',
    },
    {
      name: 'error as object with message',
      res: responseOf(503, JSON.stringify({ error: { message: 'nested' } })),
      want: 'nested',
    },
    {
      name: 'detail wins over message when both set',
      res: responseOf(400, JSON.stringify({ detail: 'd', message: 'm' })),
      want: 'd',
    },
  ])('$name', async ({ res, want }) => {
    await expect(readHttpErrorMessage(res)).resolves.toBe(want);
  });

  it('returns raw text when not JSON', async () => {
    const res = responseOf(500, 'plain text error');
    await expect(readHttpErrorMessage(res)).resolves.toBe('plain text error');
  });

  it('returns raw text when JSON without known keys', async () => {
    const res = responseOf(500, JSON.stringify({ other: 1 }));
    await expect(readHttpErrorMessage(res)).resolves.toBe('{"other":1}');
  });

  it('returns status line when body empty', async () => {
    const res = responseOf(404, '   ', 'Not Found');
    await expect(readHttpErrorMessage(res)).resolves.toBe('404 Not Found');
  });

  it('returns status line when .text() rejects (outer catch)', async () => {
    const res = responseOf(500, '', 'Error', new Error('read failed'));
    await expect(readHttpErrorMessage(res)).resolves.toBe('500 Error');
  });

  it('trims status line (no trailing space issue)', async () => {
    const res = { status: 401, statusText: '', text: () => Promise.resolve('') } as unknown as Response;
    await expect(readHttpErrorMessage(res)).resolves.toBe('401');
  });
});
