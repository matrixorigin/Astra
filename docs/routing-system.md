# Query Routing System

The query routing system automatically analyzes user queries and routes them to specialized agent types for optimal performance.

## Agent Types

### CODE
- **Purpose**: Code writing, review, debugging, and development tasks
- **Temperature**: 0.3 (more deterministic)
- **Preferred Tools**: `read_file`, `write_file`, `list_files`, `search_files`, `run_command`, `git_status`, `git_diff`, `create_file`
- **Triggers**: Programming languages, file extensions, code blocks, git operations

### PLANNING  
- **Purpose**: Project planning, architecture design, requirement analysis
- **Temperature**: 0.5 (balanced creativity/structure)
- **Preferred Tools**: `create_file`, `write_file`, `search_files`, `web_search`, `analyze_requirements`
- **Triggers**: Planning keywords, project management terms, design/architecture requests

### DEBUGGING
- **Purpose**: Error diagnosis, troubleshooting, bug fixing
- **Temperature**: 0.2 (highly focused)
- **Preferred Tools**: `read_file`, `search_files`, `run_command`, `check_logs`, `analyze_error`, `test_code`, `git_log`
- **Triggers**: Error messages, debugging keywords, problem-solving requests

### GENERAL
- **Purpose**: General assistance, information gathering, miscellaneous tasks
- **Temperature**: 0.7 (more creative)
- **Preferred Tools**: `web_search`, `read_file`, `write_file`, `create_file`, `run_command`, `search_files`
- **Triggers**: Default for queries that don't match other patterns

## Usage

### Automatic Routing
The system automatically routes queries in the chat loop:

```python
# This happens automatically in ChatLoop.run_step_stream()
from core.routing import RoutingService

service = RoutingService()
decision = service.route_query("Write a Python function to parse JSON")
# Returns: agent_type=CODE, confidence=0.8, preferred_tools=[...]
```

### Manual Testing
Test routing decisions via API:

```bash
curl -X POST /chat/route \
  -H "Content-Type: application/json" \
  -d '{"query": "Debug this Python error: KeyError"}'
```

### Integration
The routing system integrates with:
- **Context Manager**: Provides specialized system prompts
- **Tool Registry**: Prioritizes relevant tools
- **LLM Client**: Uses appropriate temperature settings

## Configuration

Agent configurations are defined in `core/routing/agent_config.py`:

```python
AgentConfig(
    agent_type=AgentType.CODE,
    system_prompt="You are a specialized code assistant...",
    preferred_tools=["read_file", "write_file", ...],
    temperature=0.3
)
```

## Pattern Matching

The router uses regex patterns to classify queries:

- **Code patterns**: Programming languages, file extensions, code blocks
- **Planning patterns**: Project management terms, design keywords  
- **Debugging patterns**: Error messages, troubleshooting terms

Confidence scores are based on:
- Number of pattern matches
- Diversity of matched patterns
- Pattern specificity

## Fallback Behavior

- If routing fails, defaults to GENERAL agent
- If confidence < 0.3, uses GENERAL agent
- Original context is preserved and merged with routing decisions
- System continues to work even if routing is disabled