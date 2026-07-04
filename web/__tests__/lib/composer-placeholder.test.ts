import { compactComposerPlaceholder } from '@/components/app/composer';

describe('compactComposerPlaceholder', () => {
  it('keeps the default placeholder readable', () => {
    expect(compactComposerPlaceholder('Reply to Astra...')).toBe('Reply to Astra...');
  });

  it('uses short visual copy for deferred input on narrow screens', () => {
    expect(compactComposerPlaceholder('Message Astra while it works...')).toBe('Message Astra...');
  });

  it('uses short visual copy for paused and stopping runs', () => {
    expect(compactComposerPlaceholder('Paused. Continue or close this run.')).toBe('Paused...');
    expect(compactComposerPlaceholder('Task needs direction before continuing.')).toBe(
      'Task needs direction...',
    );
    expect(compactComposerPlaceholder('Stopping...')).toBe('Stopping...');
  });

  it('uses short visual copy for unknown non-terminal statuses', () => {
    expect(
      compactComposerPlaceholder('Astra is busy. Stop it or wait to continue.'),
    ).toBe('Astra is busy...');
  });

  it('caps arbitrary long placeholder copy', () => {
    expect(compactComposerPlaceholder('This placeholder is too long to fit comfortably on a phone')).toBe(
      'This placeholder is too long to f...',
    );
  });
});
