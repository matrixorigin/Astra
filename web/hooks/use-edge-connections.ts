'use client';

import { useCallback } from 'react';
import { usePolling } from './use-polling';

export type EdgeConnection = {
  edge_agent_id: string;
  hostname: string | null;
  workspace_dir: string | null;
  connected_secs: number;
};

type EdgeStatusResponse = {
  edges: EdgeConnection[];
};

/**
 * Polls the backend for connected edge agents.
 * Uses the catch-all proxy route: /api/backend/edges/status → GET /edges/status
 */
export function useEdgeConnections({ enabled = true, intervalMs = 10000 } = {}) {
  const fetcher = useCallback(async (): Promise<EdgeConnection[]> => {
    const res = await fetch('/api/backend/edges/status');
    if (!res.ok) return [];
    const data: EdgeStatusResponse = await res.json();
    return data.edges;
  }, []);

  const { data, error, isLoading, refresh } = usePolling<EdgeConnection[]>({
    fetcher,
    intervalMs,
    enabled,
  });

  return {
    edges: data ?? [],
    hasEdge: (data?.length ?? 0) > 0,
    error,
    isLoading,
    refresh,
  };
}
