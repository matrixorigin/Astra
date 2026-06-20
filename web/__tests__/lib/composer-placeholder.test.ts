import { compactComposerPlaceholder } from '@/components/app/composer';

describe('compactComposerPlaceholder', () => {
  it('keeps the default placeholder readable', () => {
    expect(compactComposerPlaceholder('Reply to Astra...')).toBe('Reply to Astra...');
  });

  it('uses short visual copy for deferred input on narrow screens', () => {
    expect(compactComposerPlaceholder('Queue a follow-up for the next execution boundary...')).toBe('Queue follow-up...');
  });

  it('uses short visual copy for paused and stopping runs', () => {
    expect(compactComposerPlaceholder('Run paused. Resume or stop it to continue.')).toBe('Run paused...');
    expect(compactComposerPlaceholder('Stopping current run...')).toBe('Stopping...');
  });

  it('uses short visual copy for unknown non-terminal statuses', () => {
    expect(
      compactComposerPlaceholder('Run status is initializing-provider. Stop it or refresh before sending.'),
    ).toBe('Run status blocked...');
  });

  it('caps arbitrary long placeholder copy', () => {
    expect(compactComposerPlaceholder('This placeholder is too long to fit comfortably on a phone')).toBe(
      'This placeholder is too long to f...',
    );
  });
});
