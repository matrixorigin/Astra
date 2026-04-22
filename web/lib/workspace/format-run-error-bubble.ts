/**
 * Puts the **exact** run error where the user is looking (the assistant bubble),
 * so it still makes sense if the top banner was dismissed.
 */
export function formatRunErrorBubbleText(serverDetail: string, priorAssistantText: string): string {
  const detail = serverDetail.trim() || 'No error text was returned (check Network → chat/stream).';
  const prior = priorAssistantText.trim();
  if (prior) {
    return `${prior}\n\n---\n\n**Error (from run)**\n\`\`\`text\n${detail}\n\`\`\``;
  }
  return `**The run failed.**\n\n**Details:**\n\`\`\`text\n${detail}\n\`\`\``;
}

/**
 * When the server sends `turn_complete` with no `text_delta` and no `error` in the same stream
 * (or the client could not get a different signal), the model area would otherwise be blank.
 * This is not a success case — the user may need to check auth, runtime, or Network.
 */
export const streamEndedWithNoAssistantMarkdown = `**The stream ended with no assistant text.**

If you expected a reply, check the top banner, **Settings → connection**, and in **Network** the \`chat/stream\` request (HTTP status and body).`;
