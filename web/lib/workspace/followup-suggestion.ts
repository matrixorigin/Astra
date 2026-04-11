import type { ToolCall } from '@/lib/workspace/types';

export function suggestFollowupPrompt({
  userMessage,
  assistantMessage,
  toolCalls,
}: {
  userMessage: string;
  assistantMessage: string;
  toolCalls: ToolCall[];
}): string | null {
  const trimmed = userMessage.trim();
  if (
    trimmed.length === 0 ||
    trimmed.startsWith('/') ||
    isShortContinuationPrompt(trimmed) ||
    assistantMessage.trim().length === 0 ||
    assistantLooksIncomplete(assistantMessage)
  ) {
    return null;
  }

  const lexicon = suggestionLexicon(trimmed, assistantMessage);
  const toolNames = toolCalls.map((tool) => tool.tool);
  const edited = toolNames.some(isEditTool);
  const validated = toolNames.some(isValidationTool);
  const committed = toolNames.includes('git_commit');

  const questionReply = suggestReplyToAssistantQuestion({
    assistantMessage,
    edited,
    validated,
    committed,
    lexicon,
  });
  if (questionReply) return questionReply;

  if (assistantRequestsReply(assistantMessage)) return null;
  if (committed) return lexicon.push;
  if (edited && validated) return lexicon.commit;
  if (edited) return lexicon.validate;
  return null;
}

function suggestionLexicon(userMessage: string, assistantMessage: string) {
  if (prefersChinese(userMessage) || prefersChinese(assistantMessage)) {
    return {
      validate: '跑一下测试',
      commit: '提交一下',
      push: '推上去',
      continuePrompt: '继续',
    };
  }
  return {
    validate: 'run the tests',
    commit: 'commit this',
    push: 'push it',
    continuePrompt: 'go ahead',
  };
}

function prefersChinese(text: string): boolean {
  return /[\u4e00-\u9fff]/.test(text);
}

function isShortContinuationPrompt(line: string): boolean {
  return /^(continue|go on|继续|继续啊|继续吧|接着|下一步)$/i.test(line.trim());
}

function isEditTool(tool: string): boolean {
  return [
    'write_file',
    'str_replace',
    'multi_edit',
    'create_file',
    'delete_file',
    'move_file',
    'git_commit',
  ].includes(tool);
}

function isValidationTool(tool: string): boolean {
  return tool === 'run_build_test';
}

function assistantRequestsReply(message: string): boolean {
  const lines = message
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean);
  const lastLine = lines.at(-1) ?? message.trim();
  if (lastLine.endsWith('?') || lastLine.endsWith('？')) return true;

  const lower = message.toLowerCase();
  return (
    lower.includes('would you like') ||
    lower.includes('do you want') ||
    lower.includes('should i') ||
    lower.includes('want me to') ||
    lower.includes('which option') ||
    lower.includes('which one') ||
    message.includes('要我') ||
    message.includes('你想') ||
    message.includes('是否需要') ||
    message.includes('要不要')
  );
}

function suggestReplyToAssistantQuestion({
  assistantMessage,
  edited,
  validated,
  committed,
  lexicon,
}: {
  assistantMessage: string;
  edited: boolean;
  validated: boolean;
  committed: boolean;
  lexicon: ReturnType<typeof suggestionLexicon>;
}): string | null {
  if (!assistantRequestsReply(assistantMessage)) return null;

  const lower = assistantMessage.toLowerCase();
  if (committed && mentionsPushQuestion(lower, assistantMessage)) {
    return lexicon.push;
  }
  if (edited && validated && mentionsCommitQuestion(lower, assistantMessage)) {
    return lexicon.commit;
  }
  if (edited && mentionsTestQuestion(lower, assistantMessage)) {
    return lexicon.validate;
  }
  if (mentionsContinueQuestion(lower, assistantMessage)) {
    return lexicon.continuePrompt;
  }
  return null;
}

function mentionsContinueQuestion(lower: string, message: string): boolean {
  return (
    lower.includes('continue') ||
    lower.includes('keep going') ||
    lower.includes('keep working') ||
    lower.includes('go ahead') ||
    message.includes('继续') ||
    message.includes('接着') ||
    message.includes('往下')
  );
}

function mentionsTestQuestion(lower: string, message: string): boolean {
  return (
    lower.includes('run the tests') ||
    lower.includes('run tests') ||
    lower.includes('run the test') ||
    lower.includes('test this') ||
    lower.includes('verify it') ||
    message.includes('测试') ||
    message.includes('验证')
  );
}

function mentionsCommitQuestion(lower: string, message: string): boolean {
  return lower.includes('commit') || message.includes('提交');
}

function mentionsPushQuestion(lower: string, message: string): boolean {
  return (
    lower.includes('push') ||
    message.includes('推上去') ||
    message.includes('推到远端')
  );
}

function assistantLooksIncomplete(message: string): boolean {
  const lower = message.toLowerCase();
  return (
    lower.includes('error:') ||
    lower.includes("i couldn't") ||
    lower.includes('i could not') ||
    lower.includes('i can’t') ||
    lower.includes('unable to') ||
    lower.includes('failed to') ||
    message.includes('失败') ||
    message.includes('出错') ||
    message.includes('无法')
  );
}
