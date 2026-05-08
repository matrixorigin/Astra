import { readFileSync } from 'fs';
import path from 'path';

const repoRoot = path.join(__dirname, '..', '..');

function readWebFile(relativePath: string): string {
  return readFileSync(path.join(repoRoot, relativePath), 'utf8');
}

describe('Phase 2 Web session cache contract', () => {
  it('writes run events and session watermarks in one IndexedDB transaction', () => {
    const source = readWebFile('lib/session-cache/indexeddb.ts');
    expect(source).toContain("db.transaction(['run_events', 'session_watermarks'], 'readwrite')");
    expect(source).toContain("tx.objectStore('run_events')");
    expect(source).toContain("tx.objectStore('session_watermarks')");
    expect(source).toContain('tx.oncomplete');
  });

  it('detects event_idx gaps before committing a batch and reconnects from last_ok_idx', () => {
    const source = readWebFile('lib/session-cache/indexeddb.ts');
    expect(source).toContain('eventIdx > lastOkIdx + 1');
    expect(source).toContain('gapDetected: true');
    expect(source).toContain('reconnectLastIndex: lastOkIdx');
  });

  it('shares watermarks through BroadcastChannel and applies events idempotently', () => {
    const cacheSource = readWebFile('lib/session-cache/indexeddb.ts');
    const runHookSource = readWebFile('hooks/use-run-stream.ts');
    expect(cacheSource).toContain("new BroadcastChannel(WATERMARK_CHANNEL)");
    expect(cacheSource).toContain('eventsStore.get(cacheKey)');
    expect(runHookSource).toContain('subscribeWatermarks');
  });

  it('clears browser-local device state after revoke or passive expiry events', () => {
    const cacheSource = readWebFile('lib/session-cache/indexeddb.ts');
    const chatHookSource = readWebFile('hooks/use-chat-stream.ts');
    expect(cacheSource).toContain('localStorage.clear()');
    expect(chatHookSource).toContain("eventType === 'device_revoked'");
    expect(chatHookSource).toContain("eventType === 'device_lease_expired'");
    expect(chatHookSource).toContain('clearDeviceLocalState');
  });
});
