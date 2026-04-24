import { formatRunErrorBubbleText } from '@/lib/workspace/format-run-error-bubble';

describe('formatRunErrorBubbleText', () => {
  it('shows full server detail in a text fence when there is no prior content', () => {
    const s = formatRunErrorBubbleText('Could not validate credentials', '');
    expect(s).toContain('Could not validate credentials');
    expect(s).toContain('```text');
  });

  it('appends to prior partial assistant text', () => {
    const s = formatRunErrorBubbleText('Timeout', 'Partial reply…');
    expect(s).toContain('Partial reply');
    expect(s).toContain('Timeout');
  });
});
