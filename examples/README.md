# Examples

This directory contains examples demonstrating mo-dev-agent capabilities.

## Quick Start

**File**: `quick_start.py`

Basic usage showing session creation and event logging.

```bash
python examples/quick_start.py
```

## Complete Example

**File**: `complete_example.py`

Comprehensive demonstration of all core features:
- Session management
- Event logging (user queries + LLM responses)
- Causal chain tracking and validation
- Multi-turn conversations
- Git for Data time machine (checkpoints)
- Git for Data sandbox (isolated experiments)
- Cross-session queries

```bash
python examples/complete_example.py
```

## Git for Data

**File**: `git_for_data_example.py`

Demonstrates MatrixOne's Git for Data capabilities:
- Creating checkpoints
- Restoring to previous states
- Running isolated experiments in sandboxes

```bash
python examples/git_for_data_example.py
```

## Prerequisites

Make sure you have:
1. Activated the virtual environment: `conda activate dev-agent`
2. Started services: `make dev-up`
3. Initialized database: `make db-init`

## Example Output

All examples include detailed output showing:
- ✓ Success indicators for each operation
- Event IDs and session IDs
- Statistics (event counts, token usage, etc.)
- Validation results

## Next Steps

After running the examples, explore:
- [Design Documents](../docs/design/) - Architecture and design decisions
- [Development Guide](../docs/development.md) - Contributing guidelines
- [Tests](../tests/) - Test suite for reference implementations
