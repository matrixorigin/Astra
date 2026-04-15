import { tryApiFetch, getWebDataMode } from '@/lib/api/client';

export interface EdgeAgent {
  edge_agent_id: string;
  hostname: string | null;
  workspace_dir: string | null;
  connected_secs: number;
}

interface EdgeStatusResponse {
  edges: EdgeAgent[];
}

const mockEdges: EdgeAgent[] = [
  {
    edge_agent_id: 'edge-laptop-abc123',
    hostname: 'dev-laptop',
    workspace_dir: '/home/alice/projects/app',
    connected_secs: 3621,
  },
  {
    edge_agent_id: 'edge-workstation-def456',
    hostname: 'build-server',
    workspace_dir: '/opt/workspaces/api',
    connected_secs: 86412,
  },
];

export async function getEdges(): Promise<EdgeAgent[]> {
  if ((await getWebDataMode()) === 'demo') {
    return mockEdges;
  }

  const response = await tryApiFetch<EdgeStatusResponse>('/edges/status');
  return response?.edges ?? [];
}
