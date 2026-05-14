import { firstName, greetingFor } from '@/lib/utils/time';

describe('time utilities', () => {
  it('selects greeting by local hour', () => {
    expect(greetingFor(new Date('2026-05-08T08:00:00'))).toBe('Morning');
    expect(greetingFor(new Date('2026-05-08T14:00:00'))).toBe('Afternoon');
    expect(greetingFor(new Date('2026-05-08T21:00:00'))).toBe('Evening');
  });

  it('extracts first name with fallback', () => {
    expect(firstName('Astra User')).toBe('Astra');
    expect(firstName('')).toBe('there');
  });
});
