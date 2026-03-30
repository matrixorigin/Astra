import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { AgentsTableClient } from '@/components/agents/agents-table-client';
import type { AgentSummary } from '@/lib/models/platform';

const mockAgents: AgentSummary[] = [
  {
    id: 'a1',
    name: 'Code Agent',
    type: 'code',
    model: 'gpt-4',
    status: 'active',
    skills: ['bash', 'python'],
    owner: 'user1',
    updatedAt: '2026-01-01T00:00:00Z',
  },
  {
    id: 'a2',
    name: 'Review Agent',
    type: 'review',
    model: 'claude-3',
    status: 'inactive',
    skills: ['code-review'],
    owner: 'user2',
    updatedAt: '2026-01-02T00:00:00Z',
  },
  {
    id: 'a3',
    name: 'Deploy Agent',
    type: 'code',
    model: 'gpt-4',
    status: 'active',
    skills: ['docker', 'k8s'],
    owner: 'user1',
    updatedAt: '2026-01-03T00:00:00Z',
  },
];

describe('AgentsTableClient', () => {
  it('renders all agents by default', () => {
    render(<AgentsTableClient agents={mockAgents} />);
    expect(screen.getByText('Code Agent')).toBeInTheDocument();
    expect(screen.getByText('Review Agent')).toBeInTheDocument();
    expect(screen.getByText('Deploy Agent')).toBeInTheDocument();
    expect(screen.getByText(/3 of 3/)).toBeInTheDocument();
  });

  it('filters by search query', async () => {
    const user = userEvent.setup();
    render(<AgentsTableClient agents={mockAgents} />);

    const searchInput = screen.getByPlaceholderText(/search/i);
    await user.type(searchInput, 'Review');

    expect(screen.getByText('Review Agent')).toBeInTheDocument();
    expect(screen.queryByText('Code Agent')).not.toBeInTheDocument();
    expect(screen.getByText(/1 of 3/)).toBeInTheDocument();
  });

  it('filters by status', async () => {
    const user = userEvent.setup();
    render(<AgentsTableClient agents={mockAgents} />);

    const selects = screen.getAllByRole('combobox');
    await user.selectOptions(selects[0], 'inactive');

    expect(screen.getByText('Review Agent')).toBeInTheDocument();
    expect(screen.queryByText('Code Agent')).not.toBeInTheDocument();
    expect(screen.queryByText('Deploy Agent')).not.toBeInTheDocument();
  });

  it('filters by type', async () => {
    const user = userEvent.setup();
    render(<AgentsTableClient agents={mockAgents} />);

    const selects = screen.getAllByRole('combobox');
    await user.selectOptions(selects[1], 'review');

    expect(screen.getByText('Review Agent')).toBeInTheDocument();
    expect(screen.queryByText('Code Agent')).not.toBeInTheDocument();
  });

  it('shows skill badges', () => {
    render(<AgentsTableClient agents={mockAgents} />);
    expect(screen.getByText('bash')).toBeInTheDocument();
    expect(screen.getByText('python')).toBeInTheDocument();
    expect(screen.getByText('code-review')).toBeInTheDocument();
  });

  it('handles empty agents list', () => {
    render(<AgentsTableClient agents={[]} />);
    expect(screen.getByText(/0 of 0/)).toBeInTheDocument();
    expect(screen.getByText(/No agents match/)).toBeInTheDocument();
  });
});
