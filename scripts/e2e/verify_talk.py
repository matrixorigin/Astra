#!/usr/bin/env python3
"""
Talk Verification — run designed conversation cases against real CLI + API + LLM.

Each case is a YAML file in scripts/e2e/cases/ defining multi-turn conversations
with rule-based checks (hard pass/fail) and LLM judge (soft scoring).

Requires: API server running (make dev-start), LLM model configured.

Usage:
    make verify-talk                    # run all cases
    make verify-talk CASE=memory_basic  # run one case
    make verify-talk VERBOSE=1          # verbose
"""
from __future__ import annotations

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent))

# Force HuggingFace offline mode — model is cached locally, avoid proxy hang
os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"
for _k in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"):
    os.environ.pop(_k, None)

import argparse
import logging

import yaml

from api.database import SessionLocal
from scripts.e2e.talk_checks import run_rule_checks, llm_judge
from scripts.e2e.talk_runner import TalkSession

CASES_DIR = Path(__file__).parent / "cases"
API_URL = os.environ.get("MO_AGENT_API_URL", "http://127.0.0.1:8000")

logging.basicConfig(level=logging.WARNING)


def _get_cheapest_model() -> str | None:
    try:
        from core.llm.model_resolver import _resolve_cheapest
        model = _resolve_cheapest("")
        return model if model else None
    except Exception:
        return None


def _get_llm_client():
    from core.llm.client import LLMClient
    return LLMClient(SessionLocal)


def _init_prev_counts(case: dict, uid: str, sid: str, prev_counts: dict, uid_only: bool = False) -> None:
    """Snapshot initial DB counts for relative checks (+N)."""
    from sqlalchemy import text as sa_text
    for turn_def in case.get("turns", []):
        for rule in turn_def.get("checks", {}).get("rules", []):
            if "db" in rule:
                db_rule = rule["db"]
                table = db_rule["table"]
                where_raw = db_rule["where"]
                if uid_only and ":sid" in where_raw:
                    continue  # skip sid-based counts until sid is known
                where = where_raw.replace(":uid", f"'{uid}'").replace(":sid", f"'{sid}'")
                key = f"{table}:{where}"
                if key not in prev_counts:
                    with SessionLocal() as db:
                        prev_counts[key] = db.execute(
                            sa_text(f"SELECT COUNT(*) FROM {table} WHERE {where}")
                        ).scalar() or 0


def _seed_graph_nodes(user_id: str, count: int = 50) -> None:
    """Seed graph nodes for a user so activation:v1 threshold is met."""
    from core.memory.factory import create_editor

    editor = create_editor(SessionLocal, user_id=user_id)
    if not editor._embed_client:
        return  # no embedding client, skip

    topics = [
        "Python", "Rust", "Go", "TypeScript", "Java", "C++", "Ruby", "Swift",
        "machine learning", "data analysis", "web development", "database",
        "API design", "microservices", "DevOps", "cloud computing",
        "algorithms", "data structures", "system design", "testing",
    ]
    batch: list[dict] = []
    for i in range(count):
        topic = topics[i % len(topics)]
        batch.append({
            "content": f"Background: user has experience with {topic} (seed {i})",
            "type": "semantic", "trust": "T3",
        })
        if len(batch) >= 10:
            editor.batch_inject(user_id, batch, source="verify-talk-seed")
            batch = []
    if batch:
        editor.batch_inject(user_id, batch, source="verify-talk-seed")


def run_case(case: dict, *, verbose: bool = False, model: str | None = None) -> tuple[int, int]:
    """Run one case. Returns (passed, failed)."""
    name = case["name"]
    passed = failed = 0
    prev_counts: dict[str, int] = {}

    print(f"\n{'='*60}")
    print(f"Case: {name}")
    print(f"  {case.get('description', '')}")

    session = TalkSession(api_url=API_URL, db_factory=SessionLocal, model=model)

    try:
        session.setup()

        uid = session.user_uuid or session.username

        # Set activation:v1 strategy for this test user so graph path is used
        from core.memory.factory import set_user_strategy
        set_user_strategy(SessionLocal, uid, "activation:v1")

        # Pre-seed graph nodes only for cases that need graph activation check
        check_graph = case.get("check_graph_activation", False)
        if check_graph:
            _seed_graph_nodes(uid, count=50)

        # Run first turn to establish session_id
        turns = case.get("turns", [])
        if not turns:
            print("  ⚠️  No turns defined")
            return 0, 0

        # Snapshot uid-based counts BEFORE first turn
        _init_prev_counts(case, uid, "", prev_counts, uid_only=True)

        # First turn
        record = session.say(turns[0]["user"])
        sid = session.session_id or ""

        print(f"  session={sid}  user={uid}")

        # Now snapshot sid-based counts (sid is known after first turn)
        _init_prev_counts(case, uid, sid, prev_counts)

        # Process first turn checks
        p, f = _check_turn(0, turns[0], record, uid, sid, prev_counts, verbose, model)
        passed += p
        failed += f

        # Remaining turns
        for i, turn_def in enumerate(turns[1:], start=1):
            if turn_def.get("new_session"):
                session.new_session()
            record = session.say(turn_def["user"])
            # Update sid if new session was created
            if turn_def.get("new_session") and session.session_id:
                sid = session.session_id
                print(f"  [new session: {sid}]")
            p, f = _check_turn(i, turn_def, record, uid, sid, prev_counts, verbose, model)
            passed += p
            failed += f

        # Final checks
        final = case.get("final_checks", {})
        if "rules" in final:
            print(f"\n  Final checks:")
            results = run_rule_checks(
                final["rules"], "", [], SessionLocal, uid, sid, prev_counts,
            )
            for r in results:
                status = "✅" if r.passed else "❌"
                print(f"    {status} {r.name}" + (f" — {r.message}" if r.message and not r.passed else ""))
                if r.passed:
                    passed += 1
                else:
                    failed += 1

        # Verify graph activation was actually used (only for cases that opt in)
        if check_graph:
            from core.memory.factory import _resolve_strategy, _registry, StrategyDescriptor, _register_builtins
            from core.embedding import get_embedding_client
            _register_builtins()
            sk = _resolve_strategy(SessionLocal, uid, backend=None, strategy=None)
            desc = StrategyDescriptor.parse(sk)
            strategy = _registry.create_strategy(desc, db_factory=SessionLocal)
            ec = get_embedding_client()
            q_emb = ec.embed("Python data analysis")
            _, explain_info = strategy.retrieve(uid, "Python data analysis", q_emb, top_k=3, explain=True)
            path = (explain_info or {}).get("path", "unknown")
            if path == "graph":
                print(f"    ✅ graph_activation_used — path=graph (confirmed via explain)")
                passed += 1
            else:
                print(f"    ❌ graph_activation_used — path={path} (expected graph)")
                failed += 1

    except Exception as e:
        print(f"  ❌ Case failed: {e}")
        failed += 1
    finally:
        session.cleanup()
        _cleanup_user(session.username)
        if session.user_uuid:
            _cleanup_user(session.user_uuid)

    return passed, failed


def _check_turn(
    idx: int, turn_def: dict, record, uid: str, sid: str,
    prev_counts: dict, verbose: bool, model: str | None,
) -> tuple[int, int]:
    """Run checks for one turn. Returns (passed, failed)."""
    passed = failed = 0
    user_msg = turn_def["user"]
    print(f"\n  Turn {idx+1}: \"{user_msg}\"")

    if record.error:
        print(f"    ❌ ERROR: {record.error}")
        return 0, 1

    if verbose:
        print(f"    response: {record.response[:500]}")
        if record.tool_calls:
            print(f"    tools: {[tc['name'] for tc in record.tool_calls]}")
            for tc in record.tool_calls:
                print(f"      {tc['name']} args: {str(tc.get('args',''))[:200]}")

    checks = turn_def.get("checks", {})

    # Rule checks
    if "rules" in checks:
        results = run_rule_checks(
            checks["rules"], record.response, record.tool_calls,
            SessionLocal, uid, sid, prev_counts,
        )
        for r in results:
            status = "✅" if r.passed else "❌"
            print(f"    {status} {r.name}" + (f" — {r.message}" if r.message and not r.passed else ""))
            if r.passed:
                passed += 1
            else:
                failed += 1

    # LLM judge
    if "llm_judge" in checks:
        jc = checks["llm_judge"]
        r = llm_judge(
            record.response, user_msg, jc["criteria"],
            jc.get("pass_threshold", 0.7),
            _get_llm_client(), model=model,
        )
        status = "🤖✅" if r.passed else "🤖❌"
        print(f"    {status} {r.message}")
        if r.passed:
            passed += 1
        else:
            failed += 1

    return passed, failed


def _cleanup_user(user_id: str) -> None:
    from sqlalchemy import text as sa_text
    with SessionLocal() as db:
        for t in ("mem_edit_log", "mem_memories", "memory_graph_nodes", "memory_graph_edges"):
            db.execute(sa_text(f"DELETE FROM {t} WHERE user_id = :uid"), {"uid": user_id})
        db.commit()

    # Discard experiment branches
    try:
        from core.memory.experiment import MemoryExperimentManager
        db_name = SessionLocal.kw["bind"].url.database
        mgr = MemoryExperimentManager(SessionLocal, source_db=db_name)
        with SessionLocal() as db:
            rows = db.execute(
                sa_text("SELECT experiment_id FROM mem_experiments WHERE user_id = :uid"),
                {"uid": user_id},
            ).fetchall()
        for row in rows:
            try:
                mgr.discard(row.experiment_id)
            except Exception:
                pass
    except Exception:
        pass

def main() -> None:
    parser = argparse.ArgumentParser(description="Talk Verification")
    parser.add_argument("--case", default=None, help="Run specific case")
    parser.add_argument("--model", default=None, help="Model (default: cheapest)")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()

    model = args.model or _get_cheapest_model()
    if not model:
        print("❌ No LLM model configured. Run: mo-admin model load .models.yaml")
        sys.exit(1)

    if args.case:
        case_files = [CASES_DIR / f"{args.case}.yaml"]
        if not case_files[0].exists():
            print(f"❌ Case not found: {case_files[0]}")
            sys.exit(1)
    else:
        case_files = sorted(CASES_DIR.glob("*.yaml"))

    if not case_files:
        print("❌ No cases found in scripts/e2e/cases/")
        sys.exit(1)

    print(f"Talk Verification — {len(case_files)} case(s), model={model}")

    total_passed = total_failed = 0
    for cf in case_files:
        case = yaml.safe_load(cf.read_text())
        p, f = run_case(case, verbose=args.verbose, model=model)
        total_passed += p
        total_failed += f

    print(f"\n{'='*60}")
    print(f"Total: {total_passed} passed, {total_failed} failed")
    sys.exit(1 if total_failed else 0)


if __name__ == "__main__":
    main()
