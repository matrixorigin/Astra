# Permission Sync System

Mo-Agent's permission sync system enables hierarchical permission management across parent and child agents. When a parent agent spawns a child, permissions flow down automatically, and children can request additional permissions dynamically.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                      Parent Agent                           │
│  ┌────────────────────────────────────────────────────────┐ │
│  │              PermissionSyncContext                      │ │
│  │  - inherited: InheritedPermissions (from its parent)   │ │
│  │  - session_rules: allow/deny rules added during session│ │
│  └────────────────────────────────────────────────────────┘ │
│                           │                                 │
│              exports InheritedPermissions                   │
│                           ▼                                 │
├─────────────────────────────────────────────────────────────┤
│                      Child Agent                            │
│  ┌────────────────────────────────────────────────────────┐ │
│  │              PermissionSyncContext                      │ │
│  │  - inherited: rules from parent                        │ │
│  │  - session_rules: child's own additions                │ │
│  └────────────────────────────────────────────────────────┘ │
│                           │                                 │
│              request_permission() via Mailbox               │
│                           ▼                                 │
│  ┌────────────────────────────────────────────────────────┐ │
│  │          PermissionRequestHandler (parent)             │ │
│  │  - process incoming PermissionRequest                  │ │
│  │  - apply Auto/Prompt/Deny mode                         │ │
│  │  - return PermissionResponse                           │ │
│  └────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

## Core Types

### PermissionMode

Controls how permission requests are handled:

```rust
pub enum PermissionMode {
    /// Automatically approve safe operations
    Auto,
    /// Always prompt user for confirmation  
    Prompt,
    /// Deny all requests (background agents)
    Deny,
}
```

### PermissionRule

A rule that matches tool invocations:

```rust
// Examples of rule patterns:
"edit"           // matches any edit call
"edit(src:*)"    // matches edit with arg starting with "src"
"bash(git:*)"    // matches bash commands starting with "git"
"*"              // matches everything
```

### InheritedPermissions

Permissions passed from parent to child:

```rust
pub struct InheritedPermissions {
    pub mode: PermissionMode,
    pub allow_rules: Vec<PermissionRule>,
    pub deny_rules: Vec<PermissionRule>,
    pub allowed_tools: Option<Vec<String>>,
    pub is_background: bool,
}
```

### PermissionSyncContext

Runtime permission state for an agent:

```rust
let ctx = PermissionSyncContext::root(PermissionMode::Auto);
ctx.add_session_allow_rule("edit(src:*)");
ctx.add_session_deny_rule("bash(rm:*)");

// Check if operation is allowed
if ctx.is_allowed("edit", Some("src/main.rs")) {
    // proceed
}

// Export for child agent
let inherited = ctx.export_for_child(Some(vec!["view", "edit", "grep"]));
```

## Usage Patterns

### 1. Parent Creating Child with Inherited Permissions

```rust
use astra_runtime::orchestration::{
    PermissionMode, PermissionSyncContext, InheritedPermissions,
};

// Parent's permission context
let parent_ctx = PermissionSyncContext::root(PermissionMode::Auto);
parent_ctx.add_session_allow_rule("edit(src:*)");

// Export permissions for child (optionally restrict tools)
let inherited = parent_ctx.export_for_child(Some(vec![
    "view".to_string(),
    "edit".to_string(),
    "grep".to_string(),
]));

// Child starts with inherited permissions
let child_ctx = PermissionSyncContext::with_inherited(inherited);

// Child inherits parent's rules
assert!(child_ctx.is_allowed("edit", Some("src/main.rs")));
```

### 2. Child Requesting Permission from Parent

When a child needs permission beyond what it inherited:

```rust
use astra_runtime::messaging::{AgentMailbox, AgentMailboxRouter};
use astra_runtime::orchestration::PermissionRequest;

// Child creates request
let request = PermissionRequest::new(
    "edit",
    serde_json::json!({"path": "/etc/config"}),
)
.with_suggested_rule("edit(/etc/config:*)");

// Send to parent via mailbox, wait for response
let response = child_mailbox
    .request_permission(request, Duration::from_secs(30))
    .await?;

if response.approved {
    // Apply any permission updates from parent
    for rule in response.updates {
        child_ctx.add_session_allow_rule(&rule);
    }
    // Proceed with operation
}
```

### 3. Parent Handling Permission Requests

```rust
use astra_runtime::orchestration::{
    PermissionRequestHandler, PermissionDecision, PermissionCallback,
};

let parent_ctx = Arc::new(RwLock::new(
    PermissionSyncContext::root(PermissionMode::Auto)
));

// Create handler with optional callback
let callback: PermissionCallback = Arc::new(|req| {
    // Custom logic - e.g., prompt user, check policy server
    if req.tool == "bash" && req.args.to_string().contains("sudo") {
        PermissionDecision::Deny("sudo not allowed".into())
    } else {
        PermissionDecision::Approve(None)
    }
});

let handler = PermissionRequestHandler::new(parent_ctx.clone())
    .with_callback(callback);

// Process incoming permission request
let response = handler.handle_request(&request).await;

// Send response back to child
let reply = response.to_message(child_address, correlation_id);
mailbox.send(reply).await?;
```

### 4. Background Agents

Background agents cannot show prompts, so they run in Deny mode:

```rust
let inherited = InheritedPermissions {
    mode: PermissionMode::Deny,  // Forces deny mode
    is_background: true,
    allow_rules: vec![
        PermissionRule::parse("view").unwrap(),
        PermissionRule::parse("grep").unwrap(),
    ],
    deny_rules: vec![],
    allowed_tools: Some(vec!["view", "grep", "glob"]),
};

let child_ctx = PermissionSyncContext::with_inherited(inherited);
// Child can only use pre-approved tools
```

### 5. Multi-Level Delegation

Permissions accumulate through delegation chain:

```rust
// Grandparent → Parent → Child
//
// Grandparent: mode=Auto, allow=["edit(src:*)"]
// Parent: inherits, adds allow=["bash(git:*)"]  
// Child: inherits both rules

let grandparent = PermissionSyncContext::root(PermissionMode::Auto);
grandparent.add_session_allow_rule("edit(src:*)");

let parent_inherited = grandparent.export_for_child(None);
let parent = PermissionSyncContext::with_inherited(parent_inherited);
parent.add_session_allow_rule("bash(git:*)");

let child_inherited = parent.export_for_child(None);
let child = PermissionSyncContext::with_inherited(child_inherited);

// Child has both grandparent's and parent's rules
assert!(child.is_allowed("edit", Some("src/main.rs")));
assert!(child.is_allowed("bash", Some("git status")));
```

## Rule Matching

Rules use a simple pattern syntax:

| Pattern | Matches |
|---------|---------|
| `edit` | Any call to `edit` tool |
| `edit(src:*)` | `edit` where first arg starts with "src" |
| `bash(git:*)` | `bash` commands starting with "git" |
| `*` | Everything |

Matching order:
1. If tool is not in `allowed_tools` (when set) → deny
2. Check `deny_rules` first → if any match → deny
3. Check `allow_rules` → if any match → allow
4. If mode is `Deny` → deny
5. Otherwise → check inherited rules

## Message Types

### PermissionRequest

```rust
pub struct PermissionRequest {
    pub tool: String,
    pub args: serde_json::Value,
    pub context: Option<String>,
    pub suggested_rule: Option<String>,
}
```

### PermissionResponse

```rust
pub struct PermissionResponse {
    pub approved: bool,
    pub reason: Option<String>,
    pub updates: Vec<String>,  // New rules to add
}
```

## Error Handling

```rust
match mailbox.request_permission(request, timeout).await {
    Ok(response) => {
        if response.approved {
            // proceed
        } else {
            // handle denial
            eprintln!("Denied: {:?}", response.reason);
        }
    }
    Err(MailboxError::Timeout(_)) => {
        // Parent didn't respond in time
    }
    Err(MailboxError::Disconnected) => {
        // Parent mailbox unavailable
    }
    Err(e) => {
        // Other error
    }
}
```

## Integration with Agentic Loop

The permission sync system integrates with the agentic loop at tool execution time:

1. Before executing a tool, check `ctx.is_allowed(tool, args)`
2. If denied and child has parent mailbox, call `request_permission()`
3. If approved, apply updates and proceed
4. If denied or timeout, skip tool or return error

## Best Practices

1. **Always export minimal permissions** - Use `allowed_tools` to restrict what children can do
2. **Use deny rules for dangerous patterns** - e.g., `bash(rm -rf:*)`, `bash(sudo:*)`
3. **Background agents should be read-only** - Only allow view/grep/glob tools
4. **Set reasonable timeouts** - 30s is typical for permission requests
5. **Apply suggested rules in Auto mode** - Reduces future requests for same pattern
