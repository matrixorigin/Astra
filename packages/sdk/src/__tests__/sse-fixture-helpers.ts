import { readFileSync } from 'node:fs';
import { join } from 'node:path';

export function readSseFixture(name: string): string {
  return readFileSync(join(process.cwd(), 'src', '__tests__', '__fixtures__', 'sse', name), 'utf-8');
}

/** Chunks for `streamFrom` mocks — same contract as inline SSE `data: …\\n\\n` blocks. */
export function sseChunksForStreamMock(raw: string): string[] {
  return raw
    .split(/\n\n+/)
    .map((b) => b.trim())
    .filter(Boolean)
    .map((b) => `${b}\n\n`);
}
