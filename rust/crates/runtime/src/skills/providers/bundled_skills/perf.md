---
name: perf
description: "Analyze code for performance issues — profiling, bottlenecks, optimization opportunities"
version: "1.0.0"
triggers:
  - perf
  - performance
  - "slow"
  - "too slow"
  - optimize
  - benchmark
  - bottleneck
when_to_use: "When the user reports slowness, wants performance analysis, or asks for optimization"
category: performance
arguments:
  - name: TARGET
    description: "What to analyze — a command, file, endpoint, or description of the slowness"
    required: false
tags:
  - performance
  - optimization
  - profiling
---
# Performance Analysis

Analyze and optimize performance.

## Target

$ARGUMENTS

## Process

### 1. Establish Baseline

Before optimizing, measure the current state:
- If a specific command is slow → time it: `time <command>`
- If a specific operation is slow → identify the hot path in code
- If general slowness → profile the application

Record the baseline so improvements can be quantified.

### 2. Profile

Choose the right profiling approach:

**Rust**: `cargo build --release` first (debug builds are misleading). Use:
- `cargo bench` for micro-benchmarks
- `perf record` / `perf report` for CPU profiling (Linux)
- `RUSTFLAGS="-C instrument-coverage"` for coverage-guided analysis
- `/usr/bin/time -v` for memory/syscall overview

**Node.js**: `node --prof`, `clinic`, or Chrome DevTools profiling

**Python**: `python -m cProfile`, `py-spy`

**General**: `strace -c` for syscall bottlenecks, `ltrace` for library calls

### 3. Identify Bottlenecks

Look for common performance anti-patterns:
- **Algorithmic**: O(n²) or worse on growing inputs — quadratic loops, repeated linear searches
- **I/O bound**: Synchronous file/network I/O blocking the event loop, unnecessary fsync
- **Allocation heavy**: Excessive heap allocation in hot loops, string building with repeated concatenation
- **Cache unfriendly**: Random access patterns on large data, linked lists where vectors would work
- **Redundant work**: Computing the same value multiple times, re-parsing unchanged data
- **Lock contention**: Holding locks across I/O, too-coarse locking granularity
- **Startup cost**: Loading/parsing large files at startup that could be lazy-loaded
- **N+1 queries**: Database/API calls in loops instead of batched operations

### 4. Optimize

Apply fixes in order of impact:
1. **Algorithm/data structure** changes (biggest wins)
2. **Remove unnecessary work** (redundant I/O, duplicate computation)
3. **Batch/parallelize** independent operations
4. **Cache** expensive computations
5. **Micro-optimizations** only if profiling confirms they matter

### 5. Verify

After each optimization:
- Re-run the baseline measurement
- Quantify the improvement: "X ms → Y ms (Z% faster)"
- Run tests to confirm correctness wasn't sacrificed
- Check that memory usage didn't increase unacceptably

### 6. Report

Summarize:
- Baseline measurement
- Bottlenecks found (with evidence from profiling)
- Optimizations applied
- Final measurement and speedup
- Further opportunities (if any, with estimated effort/impact)

## Rules
- Always measure before and after — don't claim improvement without numbers
- Optimize the bottleneck, not the code that's "obviously" slow
- Don't sacrifice readability for marginal gains
- Profile release/production builds, not debug builds
- If the code is already fast enough, say so — premature optimization is the root of all evil
- Keep optimizations as separate commits for easy revert
