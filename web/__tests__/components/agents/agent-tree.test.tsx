import React from 'react';
import { render, screen } from '@testing-library/react';

import { AgentTree } from '@/components/agents/agent-tree';
import type { StreamEvent } from '@/lib/streaming/types';

describe('AgentTree', () => {
  it('counts only successful agent completions in the header summary', () => {
    const events: StreamEvent[] = [
      {
        type: 'agent_spawned',
        agent_id: 'agent-1',
        run_id: 'run-1',
        parent_run_id: 'root',
        agent_type: 'code',
        description: 'First child',
      },
      {
        type: 'agent_spawned',
        agent_id: 'agent-2',
        run_id: 'run-2',
        parent_run_id: 'root',
        agent_type: 'code',
        description: 'Second child',
      },
      {
        type: 'agent_completed',
        agent_id: 'agent-1',
        status: 'completed',
      },
      {
        type: 'agent_completed',
        agent_id: 'agent-2',
        status: 'failed',
        error: 'boom',
      },
    ];

    render(<AgentTree events={events} />);

    expect(screen.getByText('1/2 completed')).toBeInTheDocument();
    expect(screen.getByText('agent-2')).toBeInTheDocument();
  });
});
