# @astra/sdk

TypeScript SDK for the Astra AI Agent Runtime — provides type-safe REST, WebSocket, and SSE clients for building AI-powered applications.

## Installation

```bash
npm install @astra/sdk
```

## Quick Start

### REST Client

```typescript
import { AstraClient } from '@astra/sdk';

const client = new AstraClient({ baseUrl: 'http://localhost:8000' });

// Authenticate
const { access_token } = await client.login('alice', 'password');

// Create a session and start a run
const session = await client.createSession();
const run = await client.createRun({
  message: 'Hello, world!',
  sessionId: session.sessionId,
});

// Check run status
const status = await client.getRunStatus(run.runId);
```

### Streaming (SSE)

```typescript
import { AstraClient } from '@astra/sdk';

const client = new AstraClient({
  baseUrl: 'http://localhost:8000',
  accessToken: 'your-token',
});

const stream = client.streamChat(
  { message: 'Explain quantum computing', sessionId: 'sess-1' },
  {
    onEvent(event) {
      if (event.type === 'text_delta') {
        process.stdout.write(event.delta);
      }
    },
  },
);

// To cancel: stream.close()
```

### WebSocket

```typescript
import { AstraWebSocket } from '@astra/sdk';

const ws = new AstraWebSocket({
  url: 'ws://localhost:8000/api/chat/ws',
  token: 'your-token',
});

await ws.connect();

// Listen for specific event types
ws.on('text_delta', (event) => console.log(event.delta));
ws.on('tool_approval_request', (event) => {
  ws.approveToolCall({ callId: event.request_id, approved: true });
});

ws.sendMessage('Build a REST API', { sessionId: 'sess-1' });

// Pause / resume
ws.pauseRun('run-id');
ws.resumeRun('run-id');
```

### React Hooks

```tsx
import { useAstraChat } from '@astra/sdk/react';

function Chat() {
  const { messages, sendMessage, isStreaming, plan, usage } = useAstraChat({
    baseUrl: 'http://localhost:8000',
    accessToken: token,
  });

  return (
    <div>
      {messages.map((m) => (
        <div key={m.id}>{m.content}</div>
      ))}
      <button onClick={() => sendMessage('Hello!')}>Send</button>
    </div>
  );
}
```

## API Reference

### `AstraClient`

| Method | Description |
|--------|-------------|
| `login(username, password)` | Authenticate and store tokens |
| `register(username, password)` | Create account and store tokens |
| `logout()` | Clear session |
| `getMe()` | Get current user info |
| `createSession()` | Create a new chat session |
| `getSession(id)` | Get session details |
| `listSessions()` | List all sessions |
| `deleteSession(id)` | Delete a session |
| `getSessionAudit(id)` | Get session activity audit |
| `createRun(request)` | Start a new agent run |
| `getRunStatus(id)` | Check run status |
| `cancelRun(id)` | Cancel a running agent |
| `pauseRun(id)` | Pause a running agent |
| `resumeRun(id)` | Resume a paused agent |
| `getRunEvents(id, start?)` | Fetch run events |
| `streamChat(request, callbacks)` | Stream via SSE |
| `memoryStore(entry)` | Store a memory entry |
| `memorySearch(query, topK?)` | Search memories |
| `memoryRetrieve(query, topK?)` | Retrieve memories |
| `memoryPurge(topic)` | Purge memories by topic |
| `listSkills()` | List available skills |

### `AstraWebSocket`

| Method | Description |
|--------|-------------|
| `connect()` | Connect (returns Promise) |
| `close()` | Disconnect |
| `on(event, handler)` | Subscribe to events |
| `off(event, handler)` | Unsubscribe |
| `sendMessage(content, options?)` | Send a chat message |
| `cancelRun(runId?)` | Cancel current run |
| `pauseRun(runId?)` | Pause current run |
| `resumeRun(runId?)` | Resume paused run |
| `approveToolCall(approval)` | Respond to tool approval |

### React Hooks

| Hook | Description |
|------|-------------|
| `useAstraChat(config)` | Full chat state management with streaming |
| `useAstraRun(config)` | Lower-level run monitoring |

## Types

All 31 stream event types are exported:

```typescript
import type {
  StreamEvent,
  TextDeltaEvent,
  ToolApprovalRequestEvent,
  RunStatus,
  SessionInfo,
  AuthResult,
  MemoryEntry,
  MemorySearchResult,
  // ... and more
} from '@astra/sdk';
```

## License

MIT
