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
    assistantRequestsReply(assistantMessage) ||
    assistantLooksIncomplete(assistantMessage)
  ) {
    return null;
  }

  const toolNames = toolCalls.map((tool) => tool.tool);
  const edited = toolNames.some(isEditTool);
  const validated = toolNames.some(isValidationTool);
  const committed = toolNames.includes('git_commit');

  if (committed) return 'push it';
  if (edited && validated) return 'commit this';
  if (edited) return 'run the tests';
  return null;
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
