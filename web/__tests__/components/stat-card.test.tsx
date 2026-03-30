import { render, screen } from '@testing-library/react';
import { StatCard } from '@/components/dashboard/stat-card';

describe('StatCard', () => {
  it('renders label, value, and hint', () => {
    render(<StatCard label="Active Sessions" value="12" hint="3 more than yesterday" />);
    expect(screen.getByText('Active Sessions')).toBeInTheDocument();
    expect(screen.getByText('12')).toBeInTheDocument();
    expect(screen.getByText('3 more than yesterday')).toBeInTheDocument();
  });

  it('renders trend indicator when provided', () => {
    render(<StatCard label="Runs" value="42" hint="Good" trend="up" />);
    expect(screen.getByText('↑')).toBeInTheDocument();
  });

  it('renders down trend', () => {
    render(<StatCard label="Errors" value="3" hint="Decreased" trend="down" />);
    expect(screen.getByText('↓')).toBeInTheDocument();
  });

  it('does not render trend when not provided', () => {
    render(<StatCard label="Total" value="100" hint="All time" />);
    expect(screen.queryByText('↑')).not.toBeInTheDocument();
    expect(screen.queryByText('↓')).not.toBeInTheDocument();
  });

  it('renders sparkline SVG when data provided', () => {
    const { container } = render(
      <StatCard label="Activity" value="7" hint="Weekly" sparkline={[1, 3, 2, 5, 4, 7]} />,
    );
    const svg = container.querySelector('svg');
    expect(svg).toBeInTheDocument();
  });

  it('does not render sparkline with insufficient data', () => {
    const { container } = render(
      <StatCard label="Activity" value="1" hint="Single point" sparkline={[5]} />,
    );
    const svg = container.querySelector('svg');
    expect(svg).not.toBeInTheDocument();
  });
});
