/**
 * @jest-environment jsdom
 */
import { render, screen, fireEvent } from '@testing-library/react';
import { EdgesListClient } from '@/components/edges/edges-list-client';
import type { EdgeAgent } from '@/lib/api/platform-edges';

// Re-test the pure utility functions via component rendering
// and also test them in isolation by importing the module internals.

// Since formatUptime and edgeStatus are not exported, we test them
// indirectly through the component's rendered output.

const connected: EdgeAgent = {
  edge_agent_id: 'edge-abc',
  hostname: 'dev-laptop',
  workspace_dir: '/home/user/project',
  connected_secs: 60,
};

const stale: EdgeAgent = {
  edge_agent_id: 'edge-stale',
  hostname: 'old-server',
  workspace_dir: '/opt/work',
  connected_secs: 7200,
};

describe('EdgesListClient', () => {
  it('renders empty state when no edges', () => {
    render(<EdgesListClient initialEdges={[]} isLive={false} />);
    expect(screen.getByText(/no edge agents connected/i)).toBeInTheDocument();
    expect(screen.getByText(/quick start/i)).toBeInTheDocument();
  });

  it('renders edge with hostname and workspace', () => {
    render(<EdgesListClient initialEdges={[connected]} isLive={false} />);
    expect(screen.getByText('dev-laptop')).toBeInTheDocument();
    expect(screen.getByText('edge-abc')).toBeInTheDocument();
    expect(screen.getByText('/home/user/project')).toBeInTheDocument();
  });

  it('shows connected status for recent heartbeat', () => {
    render(<EdgesListClient initialEdges={[connected]} isLive={false} />);
    expect(screen.getByText('connected')).toBeInTheDocument();
  });

  it('shows stale status for old heartbeat (>= 120s)', () => {
    render(<EdgesListClient initialEdges={[stale]} isLive={false} />);
    expect(screen.getByText('stale')).toBeInTheDocument();
  });

  it('formats uptime in hours and minutes', () => {
    render(<EdgesListClient initialEdges={[stale]} isLive={false} />);
    expect(screen.getByText(/Uptime: 2h/)).toBeInTheDocument();
  });

  it('formats uptime in seconds for short durations', () => {
    const shortLived = { ...connected, connected_secs: 45 };
    render(<EdgesListClient initialEdges={[shortLived]} isLive={false} />);
    expect(screen.getByText('Uptime: 45s')).toBeInTheDocument();
  });

  it('formats uptime in minutes', () => {
    const minEdge = { ...connected, connected_secs: 300 };
    render(<EdgesListClient initialEdges={[minEdge]} isLive={false} />);
    expect(screen.getByText('Uptime: 5m')).toBeInTheDocument();
  });

  it('shows count of edges', () => {
    render(<EdgesListClient initialEdges={[connected, stale]} isLive={false} />);
    expect(screen.getByText(/2 edge agents connected/)).toBeInTheDocument();
  });

  it('shows auto-refresh notice in live mode', () => {
    render(<EdgesListClient initialEdges={[connected]} isLive={true} />);
    expect(screen.getByText(/auto-refreshing/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /refresh/i })).toBeInTheDocument();
  });

  it('hides refresh button in non-live mode', () => {
    render(<EdgesListClient initialEdges={[connected]} isLive={false} />);
    expect(screen.queryByRole('button', { name: /refresh/i })).not.toBeInTheDocument();
  });

  it('filters edges by search query', () => {
    render(<EdgesListClient initialEdges={[connected, stale]} isLive={false} />);
    const search = screen.getByPlaceholderText(/search/i);
    fireEvent.change(search, { target: { value: 'old-server' } });
    expect(screen.getByText('old-server')).toBeInTheDocument();
    expect(screen.queryByText('dev-laptop')).not.toBeInTheDocument();
  });
});
