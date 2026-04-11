import { suggestFollowupPrompt } from '@/lib/workspace/followup-suggestion';
import type { ToolCall } from '@/lib/workspace/types';

function tool(tool: string): ToolCall {
  return {
    callId: `${tool}-1`,
    tool,
    status: 'done',
    startedAt: 0,
    finishedAt: 1,
  };
}

describe('suggestFollowupPrompt', () => {
  it('suggests validation after an edit turn', () => {
    expect(
      suggestFollowupPrompt({
        userMessage: 'fix the bug',
        assistantMessage: 'Patched the file.',
        toolCalls: [tool('str_replace')],
      }),
    ).toBe('run the tests');
  });

  it('suggests commit after edit plus validation', () => {
    expect(
      suggestFollowupPrompt({
        userMessage: 'fix the bug',
        assistantMessage: 'Patched and verified.',
        toolCalls: [tool('str_replace'), tool('run_build_test')],
      }),
    ).toBe('commit this');
  });

  it('suppresses suggestions when the assistant is asking a question', () => {
    expect(
      suggestFollowupPrompt({
        userMessage: 'fix the bug',
        assistantMessage: 'The patch is ready. Would you like me to run tests?',
        toolCalls: [tool('str_replace')],
      }),
    ).toBeNull();
  });
});
