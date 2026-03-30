import {
  readStringArray,
  readModelName,
  normalizeAgent,
  normalizeSession,
  normalizeEvent,
  normalizeHealth,
  buildOverviewData,
  type ApiAgent,
  type ApiSession,
  type ApiEvent,
  type ApiHealthResponse,
} from '@/lib/api/platform-types';

// ---------------------------------------------------------------------------
// readStringArray
// ---------------------------------------------------------------------------
describe('readStringArray', () => {
  it('returns empty array for non-array input', () => {
    expect(readStringArray(null)).toEqual([]);
    expect(readStringArray(undefined)).toEqual([]);
    expect(readStringArray(42)).toEqual([]);
    expect(readStringArray('hello')).toEqual([]);
    expect(readStringArray({})).toEqual([]);
  });

  it('returns string items only', () => {
    expect(readStringArray(['a', 'b', 'c'])).toEqual(['a', 'b', 'c']);
  });

  it('filters out non-string items from mixed arrays', () => {
    expect(readStringArray(['a', 1, null, 'b', undefined, true])).toEqual(['a', 'b']);
  });

  it('returns empty array for array with no strings', () => {
    expect(readStringArray([1, 2, 3])).toEqual([]);
  });

  it('returns empty array for empty array', () => {
    expect(readStringArray([])).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// readModelName
// ---------------------------------------------------------------------------
describe('readModelName', () => {
  it('returns model when present', () => {
    expect(readModelName({ model: 'gpt-4' })).toBe('gpt-4');
  });

  it('falls back to model_name', () => {
    expect(readModelName({ model_name: 'claude-3' })).toBe('claude-3');
  });

  it('prefers model over model_name', () => {
    expect(readModelName({ model: 'gpt-4', model_name: 'claude-3' })).toBe('gpt-4');
  });

  it('returns "unassigned" when neither is present', () => {
    expect(readModelName({})).toBe('unassigned');
  });

  it('returns "unassigned" for empty string model', () => {
    expect(readModelName({ model: '' })).toBe('unassigned');
  });

  it('falls back to model_name when model is empty string', () => {
    expect(readModelName({ model: '', model_name: 'claude-3' })).toBe('claude-3');
  });

  it('returns "unassigned" when both are empty strings', () => {
    expect(readModelName({ model: '', model_name: '' })).toBe('unassigned');
  });
});

// ---------------------------------------------------------------------------
// normalizeAgent
// ---------------------------------------------------------------------------
describe('normalizeAgent', () => {
  const baseAgent: ApiAgent = {
    agent_id: 'agent-1',
    name: 'Test Agent',
    agent_type: 'coder',
    owner_user_id: 'user-1',
    agent_config: { model: 'gpt-4', skill_filter: ['code', 'review'] },
    is_active: true,
    updated_at: '2024-01-01T00:00:00Z',
  };

  it('normalizes an active agent', () => {
    const result = normalizeAgent(baseAgent);
    expect(result).toEqual({
      id: 'agent-1',
      name: 'Test Agent',
      type: 'coder',
      owner: 'user-1',
      status: 'active',
      model: 'gpt-4',
      skills: ['code', 'review'],
      updatedAt: '2024-01-01T00:00:00Z',
    });
  });

  it('normalizes an inactive agent', () => {
    const result = normalizeAgent({ ...baseAgent, is_active: false });
    expect(result.status).toBe('inactive');
  });

  it('reads skills from skill_filter', () => {
    const result = normalizeAgent({
      ...baseAgent,
      agent_config: { model: 'gpt-4', skill_filter: ['deploy'] },
    });
    expect(result.skills).toEqual(['deploy']);
  });

  it('handles empty agent_config', () => {
    const result = normalizeAgent({ ...baseAgent, agent_config: {} });
    expect(result.model).toBe('unassigned');
    expect(result.skills).toEqual([]);
  });

  it('handles missing updated_at', () => {
    const { updated_at, ...agentNoDate } = baseAgent;
    const result = normalizeAgent(agentNoDate as ApiAgent);
    expect(result.updatedAt).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// normalizeSession
// ---------------------------------------------------------------------------
describe('normalizeSession', () => {
  const baseSession: ApiSession = {
    session_id: 'sess-1',
    user_id: 'user-1',
    agent_id: 'agent-1',
    title: 'My Session',
    status: 'active',
    event_count: 10,
    created_at: '2024-01-01T00:00:00Z',
    updated_at: '2024-01-02T00:00:00Z',
  };

  it('normalizes a full session', () => {
    expect(normalizeSession(baseSession)).toEqual({
      id: 'sess-1',
      title: 'My Session',
      owner: 'user-1',
      status: 'active',
      agentId: 'agent-1',
      eventCount: 10,
      createdAt: '2024-01-01T00:00:00Z',
      updatedAt: '2024-01-02T00:00:00Z',
    });
  });

  it('defaults title to "Untitled session" when missing', () => {
    const { title, ...noTitle } = baseSession;
    const result = normalizeSession(noTitle as ApiSession);
    expect(result.title).toBe('Untitled session');
  });

  it('handles missing optional fields', () => {
    const { agent_id, updated_at, ...minimal } = baseSession;
    const result = normalizeSession(minimal as ApiSession);
    expect(result.agentId).toBeUndefined();
    expect(result.updatedAt).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// normalizeEvent
// ---------------------------------------------------------------------------
describe('normalizeEvent', () => {
  it('normalizes an event', () => {
    const event: ApiEvent = {
      event_id: 'evt-1',
      session_id: 'sess-1',
      event_type: 'message',
      content: 'Hello world',
      agent_id: 'agent-1',
      created_at: '2024-01-01T12:00:00Z',
    };
    expect(normalizeEvent(event)).toEqual({
      id: 'evt-1',
      sessionId: 'sess-1',
      type: 'message',
      summary: 'Hello world',
      agentId: 'agent-1',
      createdAt: '2024-01-01T12:00:00Z',
    });
  });

  it('handles missing agent_id', () => {
    const event: ApiEvent = {
      event_id: 'evt-2',
      session_id: 'sess-2',
      event_type: 'tool_call',
      content: 'Ran code',
      created_at: '2024-01-01T13:00:00Z',
    };
    expect(normalizeEvent(event).agentId).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// normalizeHealth
// ---------------------------------------------------------------------------
describe('normalizeHealth', () => {
  it('normalizes a health response', () => {
    const health: ApiHealthResponse = {
      status: 'ok',
      database: 'connected',
      persist_ok: 100,
      persist_fail: 2,
    };
    expect(normalizeHealth(health)).toEqual({
      status: 'ok',
      database: 'connected',
      persistOk: 100,
      persistFail: 2,
    });
  });
});

// ---------------------------------------------------------------------------
// buildOverviewData
// ---------------------------------------------------------------------------
describe('buildOverviewData', () => {
  const health = { status: 'ok', database: 'connected', persistOk: 50, persistFail: 1 };
  const agents = [
    { id: '1', name: 'A', type: 'coder', owner: 'u', status: 'active' as const, model: 'gpt-4', skills: [] },
    { id: '2', name: 'B', type: 'coder', owner: 'u', status: 'inactive' as const, model: 'gpt-4', skills: [] },
    { id: '3', name: 'C', type: 'coder', owner: 'u', status: 'active' as const, model: 'gpt-4', skills: [] },
  ];
  const sessions = [
    { id: 's1', title: 'S1', owner: 'u', status: 'active', eventCount: 5, createdAt: '2024-01-01' },
    { id: 's2', title: 'S2', owner: 'u', status: 'closed', eventCount: 3, createdAt: '2024-01-01' },
    { id: 's3', title: 'S3', owner: 'u', status: 'idle', eventCount: 1, createdAt: '2024-01-01' },
  ];
  const events = [
    { id: 'e1', sessionId: 's1', type: 'msg', summary: 'hi', createdAt: '2024-01-01' },
    { id: 'e2', sessionId: 's1', type: 'msg', summary: 'bye', createdAt: '2024-01-01' },
  ];

  it('computes correct stats', () => {
    const result = buildOverviewData(health, agents, sessions, events);
    expect(result.stats.activeAgents).toBe(2);
    expect(result.stats.openSessions).toBe(2); // active + idle
    expect(result.stats.recentEvents).toBe(2);
    expect(result.stats.persistOk).toBe(50);
  });

  it('includes all passed-through data', () => {
    const result = buildOverviewData(health, agents, sessions, events);
    expect(result.health).toBe(health);
    expect(result.agents).toBe(agents);
    expect(result.sessions).toBe(sessions);
    expect(result.events).toBe(events);
  });

  it('handles empty arrays', () => {
    const result = buildOverviewData(health, [], [], []);
    expect(result.stats.activeAgents).toBe(0);
    expect(result.stats.openSessions).toBe(0);
    expect(result.stats.recentEvents).toBe(0);
  });
});
