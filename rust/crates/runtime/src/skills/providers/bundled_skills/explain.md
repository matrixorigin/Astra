---
name: explain
description: "Explain how code works — trace execution, clarify architecture, document intent"
version: "1.0.0"
triggers:
  - explain
  - "how does"
  - "what does"
  - "walk me through"
  - "help me understand"
when_to_use: "When the user wants to understand how existing code works, its architecture, or the reasoning behind design decisions"
category: learning
arguments:
  - name: TARGET
    description: "Code, file, function, or concept to explain"
    required: true
tags:
  - documentation
  - learning
  - architecture
---
# Explain: Code Understanding

Explain how the specified code works in a clear, progressive manner.

## Target

$ARGUMENTS

## Process

### 1. Locate the Code

Find the relevant code. If `$ARGUMENTS` is:
- A file path → read that file
- A function/class name → search for its definition
- A concept → find the module(s) that implement it
- A question → identify what code is relevant to answer it

### 2. Build Context

Before explaining details, establish the big picture:
- What module/crate/package does this belong to?
- What is its role in the overall architecture?
- Who calls it? What does it call?
- What problem does it solve?

### 3. Explain Progressively

Layer the explanation from high-level to detailed:

**Level 1 — One sentence**: What does this do, for someone who's never seen it?

**Level 2 — Key concepts**: What are the main types, traits, interfaces? What's the data flow?

**Level 3 — Walk-through**: Step through the execution path, explaining non-obvious decisions. Focus on:
- Why this approach was chosen (trade-offs)
- Edge cases handled (and why they matter)
- Invariants maintained
- Performance considerations

### 4. Highlight Non-Obvious Details

Call out things a reader might miss:
- Subtle ordering dependencies
- Implicit contracts with callers
- Error handling strategies
- Concurrency/safety considerations
- Historical context from git blame if relevant

### 5. Summarize

End with:
- A one-paragraph summary of how it all fits together
- Suggestions for further reading (related modules, external docs)
- Any potential improvements or tech debt worth noting

## Rules
- Explain intent, not syntax — assume the reader can read code
- Use the project's own terminology, not generic CS terms
- If the code is confusing, say so — "this is complex because..."
- Don't explain obvious things (imports, variable declarations)
- If asked about a pattern, explain WHY the pattern exists here, not what the pattern is in general
