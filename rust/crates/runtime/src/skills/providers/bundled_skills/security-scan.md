---
name: security-scan
description: "Audit code for security vulnerabilities — injection, auth, secrets, supply chain"
version: "1.0.0"
context: fork
triggers:
  - "security scan"
  - "security review"
  - "audit security"
  - "find vulnerabilities"
  - "is this secure"
when_to_use: "When the user wants a security audit of code, dependencies, or configuration"
category: security
arguments:
  - name: SCOPE
    description: "Specific area to audit (file, module, feature, or 'full' for everything)"
    required: false
    default: "recent changes"
tags:
  - security
  - audit
  - review
---
# Security Scan

Audit the codebase for security vulnerabilities.

## Scope

$ARGUMENTS

## Process

### 1. Determine Scope

If `$ARGUMENTS` is:
- A file/module → focus on that area and its callers
- "full" or "all" → scan the entire project
- Empty or "recent changes" → `git diff main...HEAD` to review recent changes

### 2. Input Validation & Injection

Check all external input entry points:
- **SQL injection**: Parameterized queries or raw string concatenation?
- **Command injection**: Shell commands built from user input? `std::process::Command` with proper argument separation?
- **Path traversal**: File operations using user-provided paths? Check for `..`, symlinks, canonicalization
- **XSS/HTML injection**: User content rendered without escaping? Template injection?
- **Deserialization**: Untrusted data deserialized without schema validation?
- **Regex DoS**: User-controlled regex patterns? Catastrophic backtracking possible?

### 3. Authentication & Authorization

- Are auth checks present on all protected endpoints/operations?
- Are tokens/sessions properly validated and expired?
- Is there privilege escalation via parameter manipulation?
- Are admin/internal endpoints properly restricted?
- Are CORS, CSP, and other security headers configured?

### 4. Secrets & Configuration

- **Hardcoded secrets**: API keys, passwords, tokens in source code?
- **Environment variables**: Sensitive values logged or exposed in error messages?
- **Config files**: `.env`, credentials files in version control?
- **Default credentials**: Default passwords, test tokens in production configs?
- Check `.gitignore` for proper exclusion of sensitive files

### 5. Dependencies & Supply Chain

- Run `cargo audit` (Rust), `npm audit` (Node), `pip-audit` (Python) if applicable
- Check for known CVEs in dependencies
- Are dependencies pinned to specific versions?
- Are there dependencies with excessive permissions/scope?

### 6. Cryptography & Data Protection

- Are modern algorithms used? (No MD5/SHA1 for security purposes, no DES/3DES)
- Are secrets compared with constant-time functions?
- Is sensitive data encrypted at rest and in transit?
- Are random values generated with cryptographic RNGs?

### 7. Error Handling & Information Disclosure

- Do error messages leak internal paths, stack traces, or query details?
- Are panic/crash paths handled gracefully?
- Is debug/verbose mode properly disabled in production?
- Are log files protected from unauthorized access?

### 8. Report

Structure findings by severity:

**Critical** — Actively exploitable, immediate fix needed
**High** — Exploitable under realistic conditions
**Medium** — Requires specific conditions or limited impact
**Low** — Defense-in-depth, best practice violations
**Info** — Observations, hardening suggestions

For each finding:
- Location (file:line)
- Vulnerability type (CWE if applicable)
- Proof of concept or exploit scenario
- Recommended fix with code example

## Rules
- Don't report theoretical issues without a plausible attack scenario
- Distinguish between "vulnerable" and "not following best practice"
- Suggest fixes, not just findings
- If running external tools, explain what they check and their limitations
- Check for false positives before reporting
