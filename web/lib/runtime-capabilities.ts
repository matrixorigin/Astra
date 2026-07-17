import type {
  RuntimeCapabilitiesResponse,
  RuntimeCapabilityProvider,
  WorkspaceSelection,
} from '@/lib/api/types';

export type WebAccessAvailability = {
  available: boolean;
  provider?: RuntimeCapabilityProvider;
  description: string;
};

const WEB_TOOL_BUNDLE = ['web_search', 'web_fetch'] as const;

export function resolveOptionalToolAvailability(
  snapshot: RuntimeCapabilitiesResponse | null,
  workspace: WorkspaceSelection | null | undefined,
  requiredTools: readonly string[],
  capabilityLabel: string,
): WebAccessAvailability {
  if (!snapshot) {
    return { available: false, description: 'Checking execution providers…' };
  }

  const providersByTool = new Map(
    snapshot.tools.map((tool) => [tool.name, tool.providers]),
  );
  const candidates = providersByTool.get(requiredTools[0] ?? '') ?? [];
  const supportsBundle = (provider: RuntimeCapabilityProvider) =>
    requiredTools.every((tool) =>
      (providersByTool.get(tool) ?? []).some(
        (candidate) =>
          candidate.provider_id === provider.provider_id &&
          candidate.kind === provider.kind &&
          candidate.status === 'ready',
      ),
    );

  const provider = candidates.find((candidate) => {
    if (!supportsBundle(candidate)) return false;
    if (workspace?.kind === 'edge_workspace') {
      return (
        candidate.kind === 'edge' &&
        candidate.provider_id === workspace.edgeAgentId
      );
    }
    return candidate.kind === 'server';
  });

  if (provider) {
    return {
      available: true,
      provider,
      description: `Run via ${provider.display_name}`,
    };
  }

  if (workspace?.kind === 'edge_workspace') {
    return {
      available: false,
      description: `${workspace.displayName ?? workspace.edgeAgentId} does not provide ${capabilityLabel}`,
    };
  }
  return {
    available: false,
    description: `No provider with ${capabilityLabel} is available`,
  };
}

export function resolveWebAccessAvailability(
  snapshot: RuntimeCapabilitiesResponse | null,
  workspace: WorkspaceSelection | null | undefined,
): WebAccessAvailability {
  return resolveOptionalToolAvailability(
    snapshot,
    workspace,
    WEB_TOOL_BUNDLE,
    'web access',
  );
}

export function resolveGitHubAccessAvailability(
  snapshot: RuntimeCapabilitiesResponse | null,
  workspace: WorkspaceSelection | null | undefined,
): WebAccessAvailability {
  return resolveOptionalToolAvailability(
    snapshot,
    workspace,
    ['github'],
    'GitHub access',
  );
}
