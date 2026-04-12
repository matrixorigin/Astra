import React from 'react';
import { render, screen } from '@testing-library/react';

import { LiveRunPanel } from '@/components/streaming/live-run-panel';
import type { StreamEvent } from '@/lib/streaming/types';

describe('LiveRunPanel', () => {
  it('renders lifecycle terminal events with readable summaries', () => {
    const events: StreamEvent[] = [
      { type: 'run_started', run_id: 'run-1' },
      { type: 'run_finished', status: 'failed', error: 'boom' },
      { type: 'run_cancelled', run_id: 'run-2' },
    ];

    render(<LiveRunPanel events={events} connectionState="disconnected" />);

    expect(screen.getByText('Run started (run-1)')).toBeInTheDocument();
    expect(screen.getByText('Run failed: boom')).toBeInTheDocument();
    expect(screen.getByText('Run cancelled (run-2)')).toBeInTheDocument();
  });
});
