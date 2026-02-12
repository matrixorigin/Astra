# Multi-Tenancy Architecture

## Overview

mo-agent-engine supports flexible multi-tenancy deployment models using MatrixOne's native tenant isolation capabilities.

## Tenant Models

### Model 1: Single Tenant (Simple Deployment)

```
MatrixOne Cluster
  │
  └─ agent_platform (Single Tenant)
      └─ agent_engine (Database)
          ├─ users
          ├─ agents
          ├─ events
          ├─ skills
          └─ ...
```

**Use Case**: Small deployment, single organization

**Configuration**:
```bash
DATABASE_TENANT=agent_platform
DATABASE_NAME=agent_engine
```

### Model 2: Multi-Tenant (SaaS Deployment)

```
MatrixOne Cluster
  │
  ├─ platform_core (Platform Tenant)
  │   └─ agent_engine
  │       ├─ users (all users)
  │       ├─ tenants (tenant registry)
  │       └─ ...
  │
  ├─ customer_a (Customer A's Tenant)
  │   └─ agent_engine
  │       ├─ agents (Customer A's agents)
  │       ├─ events (Customer A's events)
  │       └─ ...
  │
  └─ customer_b (Customer B's Tenant)
      └─ agent_engine
          ├─ agents
          ├─ events
          └─ ...
```

**Use Case**: SaaS platform, data sovereignty requirements

**Configuration**:
```bash
# Platform service
DATABASE_TENANT=platform_core
DATABASE_NAME=agent_engine

# Customer A service instance
DATABASE_TENANT=customer_a
DATABASE_NAME=agent_engine

# Customer B service instance
DATABASE_TENANT=customer_b
DATABASE_NAME=agent_engine
```

### Model 3: Hybrid (Shared + Isolated)

```
MatrixOne Cluster
  │
  ├─ shared_platform (Shared Resources)
  │   └─ agent_engine
  │       ├─ users
  │       ├─ shared_skills (public skill library)
  │       └─ shared_models (model registry)
  │
  ├─ agent_alice (Alice's Private Tenant)
  │   └─ agent_engine
  │       ├─ private_events
  │       ├─ private_skills
  │       └─ experiments
  │
  └─ agent_bob (Bob's Private Tenant)
      └─ agent_engine
          ├─ private_events
          └─ experiments
```

**Use Case**: Shared platform with private workspaces

**Configuration**:
```bash
# Shared platform
DATABASE_TENANT=shared_platform
DATABASE_NAME=agent_engine

# Alice's private workspace
DATABASE_TENANT=agent_alice
DATABASE_NAME=agent_engine
```

## Tenant Configuration

### Environment Variables

```bash
# .env
DATABASE_HOST=localhost
DATABASE_PORT=6001
DATABASE_USER=app_user
DATABASE_PASSWORD=secret
DATABASE_TENANT=agent_platform  # ← Configurable, not hardcoded
DATABASE_NAME=agent_engine
```

### Dynamic Tenant Selection

```python
# For multi-tenant SaaS
class Database:
    def __init__(self, tenant: str = None):
        # Use provided tenant or fall back to env
        self.tenant = tenant or os.getenv("DATABASE_TENANT")
        self.connect()
    
    def connect(self):
        self.conn = pymysql.connect(
            host=os.getenv("DATABASE_HOST"),
            port=int(os.getenv("DATABASE_PORT")),
            user=os.getenv("DATABASE_USER"),
            password=os.getenv("DATABASE_PASSWORD"),
            database=f"{self.tenant}.{os.getenv('DATABASE_NAME')}"
        )
```

## Data Isolation Strategies

### Strategy 1: Tenant-Level Isolation (Strongest)

Each customer gets dedicated MatrixOne tenant.

**Pros**:
- Complete data isolation
- Independent backups
- Separate resource limits
- Regulatory compliance (GDPR, HIPAA)

**Cons**:
- More complex deployment
- Higher resource overhead

### Strategy 2: Database-Level Isolation (Medium)

All customers in same tenant, separate databases.

```sql
-- Customer A
CREATE DATABASE customer_a_engine;

-- Customer B
CREATE DATABASE customer_b_engine;
```

**Pros**:
- Simpler deployment
- Shared resources
- Easy cross-customer analytics (if needed)

**Cons**:
- Less isolation
- Shared resource limits

### Strategy 3: Row-Level Isolation (Weakest)

All customers share same database, filtered by tenant_id.

```sql
CREATE TABLE events (
  event_id VARCHAR(64) PRIMARY KEY,
  tenant_id VARCHAR(64) NOT NULL,  -- ← Tenant discriminator
  agent_id VARCHAR(64) NOT NULL,
  content TEXT,
  ...
  INDEX idx_tenant (tenant_id)
);

-- Query with tenant filter
SELECT * FROM events WHERE tenant_id = 'customer_a';
```

**Pros**:
- Simplest deployment
- Minimal overhead

**Cons**:
- Weakest isolation
- Risk of data leakage (application bugs)

## Recommended Deployment

### For MVP / Small Scale
**Model 1: Single Tenant**
- One MatrixOne tenant
- All users in same database
- Simple configuration

### For SaaS / Enterprise
**Model 2: Multi-Tenant**
- Each customer gets dedicated tenant
- Platform tenant for shared resources
- Strong data isolation

### For Hybrid Use Cases
**Model 3: Shared + Isolated**
- Shared platform for collaboration
- Private tenants for sensitive data
- Best of both worlds

## Migration Between Models

### From Single to Multi-Tenant

```sql
-- 1. Create new tenant for customer
CREATE ACCOUNT customer_a ADMIN_NAME 'admin' IDENTIFIED BY 'password';

-- 2. Export data from single tenant
mysqldump agent_platform.agent_engine > backup.sql

-- 3. Import to customer tenant
mysql customer_a.agent_engine < backup.sql

-- 4. Update service configuration
DATABASE_TENANT=customer_a
```

### From Multi-Tenant to Single

```sql
-- 1. Merge all customer data
INSERT INTO platform.agent_engine.events
SELECT * FROM customer_a.agent_engine.events;

INSERT INTO platform.agent_engine.events
SELECT * FROM customer_b.agent_engine.events;

-- 2. Update service configuration
DATABASE_TENANT=platform
```

## Best Practices

1. **Start Simple**: Use single tenant for MVP
2. **Plan for Growth**: Design schema to support multi-tenancy later
3. **Use Environment Variables**: Never hardcode tenant names
4. **Test Isolation**: Verify data cannot leak between tenants
5. **Monitor Resources**: Track per-tenant resource usage
6. **Backup Strategy**: Per-tenant backups for SaaS

## Security Considerations

### Tenant Isolation

```python
# Always validate user belongs to tenant
def get_events(user_id: str, tenant_id: str):
    # 1. Verify user belongs to tenant
    user = db.fetchone("SELECT * FROM users WHERE user_id = %s", (user_id,))
    if user["tenant_id"] != tenant_id:
        raise PermissionError("User does not belong to tenant")
    
    # 2. Query with tenant filter
    return db.fetchall(
        "SELECT * FROM events WHERE tenant_id = %s",
        (tenant_id,)
    )
```

### Cross-Tenant Access

```python
# Explicit permission required for cross-tenant access
def share_skill(from_tenant: str, to_tenant: str, skill_id: str):
    # 1. Check permission
    if not has_permission(user_id, "skill:share:cross_tenant"):
        raise PermissionError("Cross-tenant sharing not allowed")
    
    # 2. Copy skill
    skill = db.fetchone(
        f"SELECT * FROM {from_tenant}.agent_engine.skills WHERE skill_id = %s",
        (skill_id,)
    )
    db.execute(
        f"INSERT INTO {to_tenant}.agent_engine.skills (...) VALUES (...)",
        (...)
    )
```

## Performance Optimization

### Connection Pooling

```python
# Per-tenant connection pools
class TenantConnectionPool:
    def __init__(self):
        self.pools = {}
    
    def get_connection(self, tenant: str):
        if tenant not in self.pools:
            self.pools[tenant] = create_pool(tenant)
        return self.pools[tenant].get_connection()
```

### Query Optimization

```sql
-- Always include tenant_id in WHERE clause
SELECT * FROM events 
WHERE tenant_id = 'customer_a'  -- ← Enables partition pruning
  AND created_at > '2026-01-01';

-- Create composite indexes
CREATE INDEX idx_tenant_created ON events(tenant_id, created_at);
```

## Monitoring

### Per-Tenant Metrics

```sql
-- Storage usage per tenant
SELECT 
  tenant_id,
  COUNT(*) as event_count,
  SUM(LENGTH(content)) as total_bytes
FROM events
GROUP BY tenant_id;

-- Query performance per tenant
SELECT 
  tenant_id,
  AVG(query_time_ms) as avg_query_time,
  MAX(query_time_ms) as max_query_time
FROM query_logs
GROUP BY tenant_id;
```

## Summary

- **Flexible**: Support single-tenant, multi-tenant, and hybrid models
- **Configurable**: Tenant name from .env, not hardcoded
- **Scalable**: Start simple, grow to multi-tenant
- **Secure**: Strong isolation with MatrixOne tenants
- **Standard**: No custom multi-tenancy logic, use MatrixOne native features
