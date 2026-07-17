import { describe, expect, it } from 'vitest';
import {
  resolveGitHubAccessAvailability,
  resolveWebAccessAvailability,
} from '@/lib/runtime-capabilities';
import type { RuntimeCapabilitiesResponse } from '@/lib/api/types';

function snapshot(
  webSearchProviders: RuntimeCapabilitiesResponse['tools'][number]['providers'],
  webFetchProviders = webSearchProviders,
): RuntimeCapabilitiesResponse {
  return {
    tools: [
      { name: 'web_search', providers: webSearchProviders },
      { name: 'web_fetch', providers: webFetchProviders },
    ],
  };
}

const server = {
  provider_id: 'server-builtin',
  kind: 'server' as const,
  display_name: 'Server',
  status: 'ready' as const,
};
const edge = {
  provider_id: 'edge-1',
  kind: 'edge' as const,
  display_name: 'MacBook Pro',
  status: 'ready' as const,
};

describe('resolveWebAccessAvailability', () => {
  it('does not offer web access without a declared provider', () => {
    expect(resolveWebAccessAvailability(snapshot([]), null).available).toBe(false);
  });

  it('offers the server only when it declares the complete web bundle', () => {
    const available = resolveWebAccessAvailability(snapshot([server]), null);
    expect(available).toMatchObject({ available: true, provider: server });

    expect(
      resolveWebAccessAvailability(snapshot([server], []), null).available,
    ).toBe(false);
  });

  it('uses the bound edge instead of silently falling back to the server', () => {
    const workspace = {
      kind: 'edge_workspace' as const,
      edgeAgentId: 'edge-1',
      displayName: 'MacBook Pro',
      cwd: '/workspace',
    };
    expect(
      resolveWebAccessAvailability(snapshot([server, edge]), workspace),
    ).toMatchObject({ available: true, provider: edge });

    expect(
      resolveWebAccessAvailability(snapshot([server]), workspace).available,
    ).toBe(false);
  });
});

describe('resolveGitHubAccessAvailability', () => {
  it('uses the same selected-provider semantics as the web bundle', () => {
    const capabilities: RuntimeCapabilitiesResponse = {
      tools: [{ name: 'github', providers: [server, edge] }],
    };
    expect(resolveGitHubAccessAvailability(capabilities, null)).toMatchObject({
      available: true,
      provider: server,
    });
    expect(
      resolveGitHubAccessAvailability(capabilities, {
        kind: 'edge_workspace',
        edgeAgentId: 'edge-1',
        cwd: '/workspace',
      }),
    ).toMatchObject({ available: true, provider: edge });
  });
});
