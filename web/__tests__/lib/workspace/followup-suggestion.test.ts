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

  it('matches chinese style for edit turns', () => {
    expect(
      suggestFollowupPrompt({
        userMessage: '修一下这个 bug',
        assistantMessage: '已经修好了。',
        toolCalls: [tool('str_replace')],
      }),
    ).toBe('跑一下测试');
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
        assistantMessage: 'I have two valid options. Which one do you want me to try?',
        toolCalls: [tool('str_replace')],
      }),
    ).toBeNull();
  });

  it('suggests continue when the assistant asks to continue', () => {
    expect(
      suggestFollowupPrompt({
        userMessage: '修一下这个 bug',
        assistantMessage: '已经定位到原因了，要我继续改吗？',
        toolCalls: [],
      }),
    ).toBe('继续');
  });

  it('suggests commit when the assistant asks about commit', () => {
    expect(
      suggestFollowupPrompt({
        userMessage: '修一下这个 bug',
        assistantMessage: '已经修好并验证了，要我直接提交吗？',
        toolCalls: [tool('str_replace'), tool('run_build_test')],
      }),
    ).toBe('提交一下');
  });
});
