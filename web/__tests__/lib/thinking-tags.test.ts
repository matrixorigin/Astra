import { splitThinkingTags } from '@/lib/api/chats';

describe('splitThinkingTags', () => {
  it('moves complete thinking tags out of visible content', () => {
    const split = splitThinkingTags('<thinking>step one\nstep two</thinking>\nFinal answer.');

    expect(split.visibleText).toBe('Final answer.');
    expect(split.reasoning).toBe('step one\nstep two');
    expect(split.hasThinking).toBe(true);
    expect(split.reasoningOpen).toBe(false);
  });

  it('supports short think tags emitted by reasoning models', () => {
    const split = splitThinkingTags('<think>reasoning</think>\nAnswer.');

    expect(split.visibleText).toBe('Answer.');
    expect(split.reasoning).toBe('reasoning');
    expect(split.hasThinking).toBe(true);
    expect(split.reasoningOpen).toBe(false);
  });

  it('moves orphan closing think tags out of visible content', () => {
    const split = splitThinkingTags(
      'The user said hello. I should answer briefly.</think>你好！有什么我可以帮你的吗？',
    );

    expect(split.visibleText).toBe('你好！有什么我可以帮你的吗？');
    expect(split.reasoning).toBe('The user said hello. I should answer briefly.');
    expect(split.hasThinking).toBe(true);
    expect(split.reasoningOpen).toBe(false);
  });

  it('keeps open thinking blocks streaming until the close tag arrives', () => {
    const split = splitThinkingTags('<thinking>still calculating');

    expect(split.visibleText).toBe('');
    expect(split.reasoning).toBe('still calculating');
    expect(split.hasThinking).toBe(true);
    expect(split.reasoningOpen).toBe(true);
  });

  it('leaves plain assistant text untouched', () => {
    const split = splitThinkingTags('Plain final answer.');

    expect(split.visibleText).toBe('Plain final answer.');
    expect(split.reasoning).toBe('');
    expect(split.hasThinking).toBe(false);
    expect(split.reasoningOpen).toBe(false);
  });
});
