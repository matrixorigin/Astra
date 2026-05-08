export type SessionStateResponse = {
  session_id: string;
  state_revision: { monotonic_id: number; revision_hash: string };
  transcript_high_watermark: number;
  active_run?: {
    run_id: string;
    run_event_high_watermark: number;
    replay_required: boolean;
    replay_start_event_idx: number;
  } | null;
  anchor_memory?: Array<{
    item_id: string;
    category: string;
    item_key: string;
    summary_text?: string | null;
    token_estimate: number;
  }>;
  replay_required: boolean;
  transcript_replay_required: boolean;
  run_event_replay_required: boolean;
};

export type TranscriptItem = {
  session_id: string;
  item_seq: number;
  run_id?: string | null;
  role: 'user' | 'assistant' | 'system';
  content: string;
  created_at: string;
};

export type TranscriptResponse = {
  session_id: string;
  items: TranscriptItem[];
  next_before_seq?: number | null;
  has_more: boolean;
};

export type DeviceLease = {
  lease_id: string;
  session_id: string;
  device_id: string;
  device_fingerprint: string;
  trust_level: 'trusted' | 'new_device' | 'unknown_device';
  status: 'active' | 'revoked' | 'expired';
  last_monotonic_id: number;
  expires_at: string;
};

export type DeviceLeaseEndedEvent = {
  type: 'device_revoked' | 'device_lease_expired';
  lease_id: string;
  session_id: string;
  device_id: string;
  device_fingerprint: string;
  reason: string;
  ended_at_server: string;
};

async function backendJson<T>(path: string): Promise<T> {
  const response = await fetch(`/api/backend${path}`, { cache: 'no-store' });
  if (!response.ok) {
    throw new Error(`API request failed for ${path}: ${response.status} ${response.statusText}`);
  }
  return (await response.json()) as T;
}

async function backendPost<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(`/api/backend${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: 'no-store',
  });
  if (!response.ok) {
    throw new Error(`API request failed for POST ${path}: ${response.status} ${response.statusText}`);
  }
  return (await response.json()) as T;
}

export async function getSessionState(
  sessionId: string,
  options: {
    knownStateRevision?: number;
    knownRevisionHash?: string;
    clientCacheEmpty?: boolean;
    deviceId?: string;
    deviceFingerprint?: string;
  } = {},
): Promise<SessionStateResponse> {
  const params = new URLSearchParams();
  params.set('known_state_revision', String(options.knownStateRevision ?? 0));
  if (options.knownRevisionHash) params.set('known_revision_hash', options.knownRevisionHash);
  if (options.clientCacheEmpty) params.set('client_cache_empty', 'true');
  if (options.deviceId) params.set('device_id', options.deviceId);
  if (options.deviceFingerprint) params.set('device_fingerprint', options.deviceFingerprint);
  return backendJson<SessionStateResponse>(`/sessions/${sessionId}/state?${params.toString()}`);
}

export async function getSessionTranscript(
  sessionId: string,
  beforeSeq?: number,
  limit = 50,
): Promise<TranscriptResponse> {
  const params = new URLSearchParams();
  params.set('limit', String(limit));
  if (beforeSeq !== undefined) params.set('before_seq', String(beforeSeq));
  return backendJson<TranscriptResponse>(`/sessions/${sessionId}/transcript?${params.toString()}`);
}

export async function getSessionDevices(sessionId: string): Promise<DeviceLease[]> {
  const response = await backendJson<{ session_id: string; devices: DeviceLease[] }>(
    `/sessions/${sessionId}/devices`,
  );
  return response.devices;
}

export async function revokeSessionDevice(
  sessionId: string,
  request: { leaseId?: string; deviceId?: string; expectedLastMonotonicId?: number },
): Promise<DeviceLeaseEndedEvent> {
  const response = await backendPost<{ event: DeviceLeaseEndedEvent; idempotent: boolean }>(
    `/sessions/${sessionId}/device/revoke`,
    {
      lease_id: request.leaseId,
      device_id: request.deviceId,
      expected_last_monotonic_id: request.expectedLastMonotonicId,
      reason: 'settings_revoke',
    },
  );
  return response.event;
}

export async function trustSessionDevice(
  sessionId: string,
  request: { deviceId: string; expectedLastMonotonicId?: number },
): Promise<DeviceLease> {
  const response = await backendPost<{ lease: DeviceLease }>(
    `/sessions/${sessionId}/device/trust`,
    {
      device_id: request.deviceId,
      step_up_confirmation: true,
      expected_last_monotonic_id: request.expectedLastMonotonicId,
    },
  );
  return response.lease;
}

export async function cancelRun(runId: string): Promise<void> {
  await backendPost(`/chat/runs/${runId}/cancel`);
}
