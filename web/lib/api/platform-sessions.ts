import { apiFetch, apiPost, tryApiFetch, getWebDataMode } from '@/lib/api/client';
import { mockPlatformSnapshot } from '@/lib/api/mock-data';
import type {
  EventSummary,
  SessionActivityData,
  SessionActivityEntry,
  SessionSummary,
} from '@/lib/models/platform';
import type {
  ApiSession,
  ApiSessionListResponse,
  ApiEventListResponse,
  ApiReflectResponse,
  ApiSessionActivityEntry,
  ApiSessionActivityResponse,
} from './platform-types';
import { normalizeSession, normalizeEvent } from './platform-types';

export async function getSessions(limit = 50): Promise<SessionSummary[]> {
  if ((await getWebDataMode()) === 'demo') {
    return mockPlatformSnapshot.sessions;
  }

  const response = await tryApiFetch<ApiSessionListResponse>(`/sessions?limit=${limit}`);
  return response ? response.sessions.map(normalizeSession) : [];
}

export async function getSessionWorkspace(sessionId: string): Promise<{
  session: SessionSummary;
  events: EventSummary[];
  reflection?: string;
  reflectionError?: string;
}> {
  if ((await getWebDataMode()) === 'demo') {
    const session = mockPlatformSnapshot.sessions.find((item) => item.id === sessionId);

    if (!session) {
      throw new Error(`Demo session not found: ${sessionId}`);
    }

    return {
      session,
      events: mockPlatformSnapshot.events.filter((event) => event.sessionId === sessionId),
      reflection: 'Demo reflection placeholder. Wire `/chat/session/{session_id}/reflect` next.',
    };
  }

  const [session, events, reflectionResult] = await Promise.all([
    apiFetch<ApiSession>(`/sessions/${sessionId}`),
    apiFetch<ApiEventListResponse>(`/events/session/${sessionId}?limit=20`),
    apiFetch<ApiReflectResponse>(`/chat/session/${sessionId}/reflect`)
      .then((value) => ({ ok: true as const, value }))
      .catch((error: Error) => ({ ok: false as const, error: error.message })),
  ]);

  const reflectionText = reflectionResult.ok
    ? reflectionResult.value.summary ??
      reflectionResult.value.report ??
      reflectionResult.value.diagnosis ??
      undefined
    : undefined;

  return {
    session: normalizeSession(session),
    events: events.events.map(normalizeEvent),
    reflection: reflectionText,
    reflectionError: reflectionResult.ok ? undefined : reflectionResult.error,
  };
}

export async function resumeSession(sessionId: string): Promise<SessionSummary> {
  const response = await apiPost<ApiSession>(`/sessions/${sessionId}/resume`);
  return normalizeSession(response);
}

export async function cancelSession(sessionId: string): Promise<SessionSummary> {
  const response = await apiPost<ApiSession>(`/sessions/${sessionId}/cancel`);
  return normalizeSession(response);
}

export async function closeSession(sessionId: string): Promise<SessionSummary> {
  const response = await apiPost<ApiSession>(`/sessions/${sessionId}/close`);
  return normalizeSession(response);
}

// ── Session activity audit ──────────────────────────────────────────────────

function normalizeActivityEntry(entry: ApiSessionActivityEntry): SessionActivityEntry {
  return {
    logId: entry.log_id,
    action: entry.action,
    details: entry.details,
    createdAt: entry.created_at,
  };
}

export async function getSessionActivity(
  sessionId: string,
  limit = 100,
): Promise<SessionActivityData> {
  if ((await getWebDataMode()) === 'demo') {
    return { sessionId, activities: [], total: 0 };
  }

  const response = await tryApiFetch<ApiSessionActivityResponse>(
    `/sessions/${sessionId}/activity?limit=${limit}`,
  );
  if (!response) {
    return { sessionId, activities: [], total: 0 };
  }
  return {
    sessionId: response.session_id,
    activities: response.activities.map(normalizeActivityEntry),
    total: response.total,
  };
}
