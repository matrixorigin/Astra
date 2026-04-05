---
name: refactor
description: "Restructure code without changing behavior — improve design, reduce coupling, enhance modularity"
version: "1.0.0"
context: fork
triggers:
  - refactor
  - restructure
  - "tech debt"
  - "clean up architecture"
  - "extract module"
  - "split file"
when_to_use: "When the user wants to restructure code for better design without changing external behavior"
category: code-quality
arguments:
  - name: TARGET
    description: "What to refactor and/or the desired outcome"
    required: true
tags:
  - refactoring
  - design
  - architecture
---
# Refactor: Structural Improvement

Restructure code to improve design without changing behavior.

## Target

$ARGUMENTS

## Process

### 1. Understand Current State

Before changing anything:
- Read and understand the code to refactor
- Map its dependencies: what calls it, what it calls
- Identify the public API surface (what must not change)
- Run existing tests to establish a green baseline

### 2. Identify Refactoring Opportunities

Common patterns to look for:
- **God object/module**: Too many responsibilities → extract focused modules
- **Feature envy**: Code that mostly uses another module's data → move it there
- **Shotgun surgery**: One change requires touching many files → consolidate
- **Long parameter lists**: Many params → introduce a config/options struct
- **Duplicated logic**: Similar code in multiple places → extract shared function
- **Deep nesting**: >3 levels of if/match → extract functions, use early returns
- **Tight coupling**: Direct dependency on concrete types → introduce traits/interfaces
- **Primitive obsession**: Raw strings/ints for domain concepts → introduce newtypes

### 3. Plan the Refactoring

Write a step-by-step plan:
1. Each step should leave the code in a working state
2. Each step should be independently committable
3. Order steps to minimize risk: extract → move → rename → delete

Present the plan for approval if the refactoring is large.

### 4. Execute

For each step:
1. Make the structural change
2. Run tests — they must still pass
3. If tests fail, the refactoring changed behavior — fix it before continuing

### 5. Verify

After all steps:
- Run the full test suite
- Verify the public API surface hasn't changed
- `git diff --stat` to confirm the scope matches the plan
- Check that no TODO/FIXME was introduced without explanation

## Rules
- Never change behavior during refactoring — that's a separate step
- Keep the test suite green after every step
- Don't refactor and add features in the same commit
- If you discover bugs during refactoring, note them but fix separately
- Preserve git blame usefulness — don't reformat files you're not restructuring
- If the codebase lacks tests, add characterization tests BEFORE refactoring
