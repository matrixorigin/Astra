const CHAT_PINNED_THRESHOLD_PX = 80;

export type ChatScrollMetrics = {
  scrollHeight: number;
  scrollTop: number;
  clientHeight: number;
};

export function isChatScrolledToBottom(metrics: ChatScrollMetrics, thresholdPx = CHAT_PINNED_THRESHOLD_PX) {
  return metrics.scrollHeight - metrics.scrollTop - metrics.clientHeight < thresholdPx;
}

export function shouldAutoScrollChat(params: { pinnedToBottom: boolean }) {
  return params.pinnedToBottom;
}
