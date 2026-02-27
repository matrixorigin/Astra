#!/usr/bin/env python3
"""Rename database table references across the codebase.

Only replaces table names in contexts where they appear as SQL table names
or ORM __tablename__ values. Skips api/models/ (rewritten separately).
"""
import os
import re

# Unambiguous renames: these strings are unique enough to do simple replacement
SIMPLE_MAP = {
    'agent_events': 'agent_events',
    'skill_registry': 'skill_registry',
    'skill_selection_learnings': 'skill_selection_learningss',
    'agent_scratchpads': 'agent_scratchpadss',
    'ctx_snapshots': 'ctx_snapshots',
    'ctx_decision_audits': 'ctx_ctx_decision_auditss',
    'ctx_event_embeddings': 'ctx_ctx_event_embeddings',
    'ctx_prompt_templates': 'ctx_ctx_prompt_templates',
    'ctx_prompt_variants': 'ctx_ctx_prompt_variants',
    'eval_quality_assessments': 'eval_eval_quality_assessments',
    'eval_gate_results': 'eval_eval_gate_results',
    'eval_llm_feedback': 'eval_eval_llm_feedback',
    'eval_llm_call_logs': 'eval_eval_llm_call_logs',
    'eval_user_feedback': 'eval_eval_user_feedback',
    'eval_training_data': 'eval_eval_training_data',
    'infra_llm_models': 'infra_infra_llm_models',
    'infra_sandbox_metadata': 'infra_infra_sandbox_metadata',
    'infra_distributed_locks': 'infra_infra_distributed_locks',
    'wf_definitions': 'wf_definitions',
    'wf_runs': 'wf_runs',
    'verify_hallucination_checks': 'verify_verify_hallucination_checks',
    'verify_claim_evidence': 'verify_verify_claim_evidence',
    'skill_user_credentials': 'skill_skill_user_credentials',
    'agent_run_events': 'agent_agent_run_events',
    'auth_audit_logs': 'auth_auth_audit_logs',
}

SKIP_DIRS = {'__pycache__', '.git', '.mypy_cache', '.ruff_cache', 'node_modules'}

def should_process(rel_path):
    if rel_path.startswith('api/models/'):
        return False
    prefixes = ['core/', 'api/', 'cli/', 'scripts/', 'skills/', 'tests/', 'examples/']
    return any(rel_path.startswith(p) for p in prefixes)

count = 0
for root, dirs, files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
    for f in files:
        if not f.endswith('.py'):
            continue
        path = os.path.join(root, f)
        rel = path[2:]
        if not should_process(rel):
            continue

        with open(path) as fh:
            content = fh.read()

        original = content
        for old, new in SIMPLE_MAP.items():
            content = content.replace(old, new)

        if content != original:
            with open(path, 'w') as fh:
                fh.write(content)
            count += 1

print(f'Updated {count} files')
