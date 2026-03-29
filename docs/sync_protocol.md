# Incremental Sync Protocol Specification

## Overview

The incremental sync protocol enables efficient state synchronization between edge (CLI) and cloud (MatrixOne) with minimal bandwidth usage. Full snapshots are ~40KB; typical deltas are 2-5KB (85-90% reduction).

## Architecture

```
Edge (CLI)                              Cloud (MatrixOne)
──────────                              ──────────────────
sync_state.json                         sync_state table
  ├─ version_vector                     ├─ version_vector
  ├─ checkpoint_id                      ├─ checkpoint_id
  └─ pending_deltas ───────POST──────▶  └─ delta_log
         ▲                              (compacted periodically)
         └────────────GET──────────────┘
```

## Delta Encoding Format

### Delta Types

```typescript
// Base delta operation
interface DeltaOp {
  op: "add" | "replace" | "remove" | "merge";
  path: string;        // JSON Pointer (RFC 6901)
  value?: any;         // For add/replace/merge
  old_value?: any;     // For conflict detection (optional)
}

// Delta batch container
interface DeltaBatch {
  // Versioning
  source_version: number;       // Client's base version
  target_version: number;       // Expected version after apply
  
  // Checkpoint reference
  checkpoint_id: string;        // Reference checkpoint for compaction
  
  // Delta contents
  operations: DeltaOp[];
  
  // Metadata
  timestamp: string;            // ISO 8601
  entity_count: number;         // Number of entity changes
  pattern_count: number;        // Number of pattern changes
  
  // Tombstones for deleted items
  tombstones?: Tombstone[];
}

// Tombstone for deleted items
interface Tombstone {
  key: string;                  // Entity/pattern identifier
  deleted_at: string;           // ISO 8601 timestamp
  version: number;              // Version at deletion
}
```

### Delta Operation Semantics

| Operation | Behavior | Idempotent |
|-----------|----------|------------|
| `add` | Add new element at path; error if exists | No |
| `replace` | Replace element at path; error if missing | Yes |
| `remove` | Remove element at path; noop if already removed | Yes |
| `merge` | Deep merge object at path; creates if missing | Yes |

### Example Delta Batch

```json
{
  "source_version": 42,
  "target_version": 45,
  "checkpoint_id": "cp-2024-06-15",
  "operations": [
    {
      "op": "replace",
      "path": "/entities/bash",
      "value": {"name": "bash", "observations": 150, "confidence": 0.95}
    },
    {
      "op": "add",
      "path": "/patterns/new-pattern",
      "value": {"signature": "new-pattern", "frequency": 5}
    },
    {
      "op": "remove",
      "path": "/entities/deprecated-tool"
    },
    {
      "op": "merge",
      "path": "/calibration",
      "value": {"last_calibrated": "2024-06-15T10:30:00Z"}
    }
  ],
  "timestamp": "2024-06-15T10:30:00Z",
  "entity_count": 2,
  "pattern_count": 1,
  "tombstones": [
    {"key": "deprecated-tool", "deleted_at": "2024-06-15T10:30:00Z", "version": 45}
  ]
}
```

## Versioning Strategy

### Version Vector

Each session maintains a monotonic version counter:

```typescript
interface VersionVector {
  // Monotonic counter incremented on each mutation
  version: number;
  
  // Session that owns this version
  session_id: string;
  
  // Timestamp of last update
  updated_at: string;
  
  // Hash of state for integrity verification
  state_hash: string;
}
```

### Version Rules

1. **Monotonic**: `version` only increases (strictly monotonic)
2. **Sequential**: No gaps in version sequence for a session
3. **Conflict Resolution**: Higher version wins; equal versions use timestamp tiebreaker
4. **Fork Detection**: If client version > server version + 1, client must reconcile

## Checkpoint Mechanism

### Checkpoint Structure

```typescript
interface Checkpoint {
  id: string;                   // Unique checkpoint identifier
  version: number;              // Version at checkpoint
  timestamp: string;            // ISO 8601
  
  // State snapshot (full or reference)
  state_ref: {
    type: "inline" | "s3" | "db";
    location: string;
    checksum: string;           // SHA-256 of snapshot
  };
  
  // Delta log compaction info
  delta_range: {
    from_version: number;
    to_version: number;
  };
  
  // Expiry for garbage collection
  expires_at?: string;
}
```

### Checkpoint Lifecycle

1. **Creation**: Triggered every N versions or after M deltas
2. **Compaction**: Old deltas merged into checkpoint, deltas < `from_version` purged
3. **Expiry**: Checkpoints expire after 30 days (configurable)
4. **GC**: Expired checkpoints deleted; orphaned deltas purged

### Checkpoint Triggers

| Trigger | Default | Description |
|---------|---------|-------------|
| `max_deltas` | 100 | Create checkpoint after N deltas |
| `max_version_gap` | 50 | Create checkpoint if version lag > N |
| `time_interval` | 24h | Create checkpoint every N hours |
| `manual` | - | Explicit checkpoint request |

## API Contracts

### HTTP Endpoints

#### 1. Get Changes Since Version

```
GET /api/v1/sync/changes?since_version={version}&limit={limit}
```

**Request Headers:**
```
Authorization: Bearer {token}
X-Session-Id: {session_id}
Accept: application/json
```

**Query Parameters:**
| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `since_version` | integer | Yes | Base version (exclusive) |
| `limit` | integer | No | Max deltas to return (default: 100, max: 500) |
| `checkpoint_id` | string | No | Preferred checkpoint for compaction |

**Success Response (200 OK):**
```json
{
  "current_version": 150,
  "checkpoint_id": "cp-2024-06-15",
  "deltas": [
    {
      "source_version": 100,
      "target_version": 125,
      "operations": [...],
      "timestamp": "2024-06-15T10:00:00Z"
    },
    {
      "source_version": 125,
      "target_version": 150,
      "operations": [...],
      "timestamp": "2024-06-15T10:30:00Z"
    }
  ],
  "has_more": false,
  "sync_type": "incremental"
}
```

**Full Sync Required (409 Conflict):**
```json
{
  "error": "delta_too_large",
  "message": "Deltas since version 42 exceed threshold; full sync required",
  "current_version": 150,
  "checkpoint_url": "/api/v1/sync/checkpoint/cp-2024-06-15",
  "fallback": "full_sync"
}
```

**Version Not Found (410 Gone):**
```json
{
  "error": "version_expired",
  "message": "Version 42 is older than the oldest available checkpoint",
  "oldest_version": 100,
  "checkpoint_url": "/api/v1/sync/checkpoint/cp-2024-06-14",
  "fallback": "full_sync"
}
```

#### 2. Apply Delta Batch

```
POST /api/v1/sync/apply
```

**Request Headers:**
```
Authorization: Bearer {token}
X-Session-Id: {session_id}
Content-Type: application/json
```

**Request Body:**
```json
{
  "expected_version": 42,
  "batch": {
    "source_version": 42,
    "target_version": 43,
    "checkpoint_id": "cp-2024-06-15",
    "operations": [...],
    "timestamp": "2024-06-15T10:30:00Z",
    "entity_count": 5,
    "pattern_count": 3
  },
  "options": {
    "validate_only": false,
    "atomic": true
  }
}
```

**Options:**
| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `validate_only` | boolean | false | Validate without applying |
| `atomic` | boolean | true | All-or-nothing apply |
| `conflict_resolution` | string | "server_wins" | "server_wins", "client_wins", "merge" |

**Success Response (200 OK):**
```json
{
  "success": true,
  "new_version": 43,
  "applied_operations": 5,
  "conflicts": [],
  "checkpoint_created": false
}
```

**Conflict Response (409 Conflict):**
```json
{
  "success": false,
  "error": "version_conflict",
  "expected_version": 42,
  "actual_version": 45,
  "conflicts": [
    {
      "path": "/entities/bash",
      "server_value": {...},
      "client_value": {...}
    }
  ],
  "resolution_url": "/api/v1/sync/resolve"
}
```

**Validation Error (422 Unprocessable Entity):**
```json
{
  "success": false,
  "error": "validation_failed",
  "validation_errors": [
    {"path": "/entities/unknown", "message": "Path not found for replace operation"}
  ]
}
```

#### 3. Get Full State (Fallback)

```
GET /api/v1/sync/state?checkpoint_id={checkpoint_id}
```

**Response:**
```json
{
  "version": 150,
  "checkpoint_id": "cp-2024-06-15",
  "state": {
    "entities": {...},
    "patterns": {...},
    "calibration": {...}
  },
  "checksum": "sha256:abc123..."
}
```

#### 4. Create Checkpoint

```
POST /api/v1/sync/checkpoint
```

**Request:**
```json
{
  "name": "manual-backup",
  "ttl_days": 7
}
```

**Response:**
```json
{
  "checkpoint_id": "manual-backup-2024-06-15",
  "version": 150,
  "created_at": "2024-06-15T12:00:00Z",
  "expires_at": "2024-06-22T12:00:00Z"
}
```

### gRPC Service

```protobuf
syntax = "proto3";
package sync.v1;

service SyncService {
  // Stream deltas from server to client
  rpc StreamChanges(StreamChangesRequest) returns (stream DeltaBatch);
  
  // Apply delta batch with transaction semantics
  rpc ApplyDelta(ApplyDeltaRequest) returns (ApplyDeltaResponse);
  
  // Get current state (full sync fallback)
  rpc GetState(GetStateRequest) returns (StateSnapshot);
  
  // Create manual checkpoint
  rpc CreateCheckpoint(CreateCheckpointRequest) returns (Checkpoint);
  
  // Resolve conflicts with server-side merge
  rpc ResolveConflicts(ResolveConflictsRequest) returns (ResolveConflictsResponse);
}

message StreamChangesRequest {
  string session_id = 1;
  int64 since_version = 2;
  int32 limit = 3;
  string checkpoint_id = 4;
}

message ApplyDeltaRequest {
  string session_id = 1;
  int64 expected_version = 2;
  DeltaBatch batch = 3;
  ApplyOptions options = 4;
}

message ApplyOptions {
  bool validate_only = 1;
  bool atomic = 2;
  ConflictResolution conflict_resolution = 3;
}

enum ConflictResolution {
  SERVER_WINS = 0;
  CLIENT_WINS = 1;
  MERGE = 2;
}

message ApplyDeltaResponse {
  bool success = 1;
  int64 new_version = 2;
  int32 applied_operations = 3;
  repeated Conflict conflicts = 4;
  bool checkpoint_created = 5;
}
```

## Full Sync Fallback

### Trigger Conditions

Full sync is triggered when:

1. **Delta Too Large**: Cumulative delta size > 50% of full state
2. **Version Gap**: Client version < oldest available checkpoint version
3. **Corruption**: State hash mismatch detected
4. **Explicit Request**: Client requests `?force_full=true`
5. **First Sync**: Client has no local state

### Fallback Flow

```
Client                          Server
──────                          ──────
GET /changes?since=42
                      ────────>
<─ 409 delta_too_large ────────

GET /state (full)
                      ────────>
<─ 200 + full state ───────────

[Apply locally]

POST /apply (with new base)
                      ────────>
<─ 200 success ────────────────
```

### Bandwidth Optimization

When falling back to full sync:

1. **Compression**: Brotli compression for state payload
2. **Pagination**: Large states split into chunks with `?page=` parameter
3. **Diff Encoding**: If client has similar checkpoint, send binary diff

## Conflict Resolution

### Automatic Resolution

| Scenario | Resolution |
|----------|------------|
| Server wins (default) | Server value replaces client value |
| Client wins | Client value accepted (server version bumped) |
| Merge | Deep merge of objects; arrays use union |
| Timestamp | Most recent update wins |

### Manual Resolution

For unresolvable conflicts:

```
POST /api/v1/sync/resolve
```

```json
{
  "conflict_id": "conflict-123",
  "resolution": "accept_server",  // or "accept_client", "custom"
  "custom_value": {...}  // if resolution == "custom"
}
```

## Error Handling

### Retryable Errors

| Error | HTTP Status | Retry Strategy |
|-------|-------------|----------------|
| Database timeout | 503 | Exponential backoff, max 3 retries |
| Network partition | 502 | Immediate retry with backoff |
| Lock contention | 423 | Linear backoff, max 5 retries |

### Non-Retryable Errors

| Error | HTTP Status | Client Action |
|-------|-------------|---------------|
| Invalid delta format | 400 | Fix and resubmit |
| Version conflict | 409 | Pull fresh and retry |
| Authentication failed | 401 | Re-authenticate |
| Delta too large | 409 | Fall back to full sync |

## Security

1. **Authentication**: All endpoints require valid JWT
2. **Authorization**: Users can only access their own sync state
3. **Integrity**: All deltas include HMAC-SHA256 signature
4. **Encryption**: TLS 1.3 for all transport
5. **Rate Limiting**: 100 sync requests/minute per session

## Metrics & Telemetry

### Client Metrics

```typescript
interface SyncMetrics {
  sync_duration_ms: number;
  bytes_sent: number;
  bytes_received: number;
  operations_applied: number;
  conflicts_resolved: number;
  fallback_to_full_sync: boolean;
}
```

### Server Metrics

- Delta compression ratio
- Full sync frequency
- Conflict rate
- Checkpoint creation rate
- Average sync latency

## Implementation Notes

### Client-Side

1. **Queue Deltas**: Queue local changes when offline
2. **Debounce**: Batch rapid changes before sending
3. **Retry Queue**: Maintain queue of failed syncs
4. **Optimistic Updates**: Apply locally before server confirmation
5. **Conflict Cache**: Cache server values for conflict resolution

### Server-Side

1. **Delta Coalescing**: Merge adjacent deltas from same session
2. **Read Replicas**: Serve GET /changes from read replicas
3. **Connection Pooling**: Reuse database connections
4. **Async Checkpointing**: Create checkpoints in background
5. **Delta TTL**: Purge old deltas after checkpoint creation
