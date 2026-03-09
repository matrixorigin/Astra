#!/usr/bin/env python3
"""
E2E Verification — real DB writes, real assertions, optional real LLM.

Directly imports Python APIs (no subprocess). Runs against the dev database
configured in .env. Uses a dedicated `__verify_<uuid>` user ID to isolate
test data, and cleans up automatically.

Usage:
    make verify                  # core scenarios (no LLM needed)
    make verify-llm              # includes NL→Script via real LLM
    make verify VERBOSE=1        # verbose output
"""
from __future__ import annotations

import sys
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent))

import argparse
import json
import logging

from sqlalchemy import text

from api.database import SessionLocal

# ── Config ────────────────────────────────────────────────────────────

USER_ID = f"__verify_{uuid.uuid4().hex[:8]}"
DB_NAME = SessionLocal.kw["bind"].url.database

_passed = 0
_failed = 0
_verbose = False

logging.basicConfig(level=logging.WARNING)


# ── Helpers ───────────────────────────────────────────────────────────

def ok(name: str, msg: str = "") -> None:
    global _passed
    _passed += 1
    print(f"  ✅ {name}" + (f" — {msg}" if msg else ""))


def fail(name: str, msg: str) -> None:
    global _failed
    _failed += 1
    print(f"  ❌ {name} — {msg}")


def check(name: str, condition: bool, msg: str = "") -> None:
    ok(name, msg) if condition else fail(name, msg)


def vlog(msg: str) -> None:
    if _verbose:
        print(f"    ↳ {msg}")


def query_one(sql: str, **params):
    with SessionLocal() as db:
        return db.execute(text(sql), params).first()


def query_all(sql: str, **params):
    with SessionLocal() as db:
        return db.execute(text(sql), params).fetchall()


def scalar(sql: str, **params) -> int:
    with SessionLocal() as db:
        return db.execute(text(sql), params).scalar() or 0


def count(table: str) -> int:
    return scalar(f"SELECT COUNT(*) FROM {table} WHERE user_id = :uid", uid=USER_ID)


def cleanup() -> None:
    with SessionLocal() as db:
        for t in ("mem_edit_log", "mem_memories"):
            db.execute(text(f"DELETE FROM {t} WHERE user_id = :uid"), {"uid": USER_ID})
        db.commit()

    # Discard any experiment branches created for this user
    try:
        from core.memory.experiment import MemoryExperimentManager
        mgr = MemoryExperimentManager(SessionLocal, source_db=DB_NAME)
        with SessionLocal() as db:
            rows = db.execute(
                text("SELECT experiment_id FROM mem_experiments WHERE user_id = :uid"),
                {"uid": USER_ID},
            ).fetchall()
        for row in rows:
            try:
                mgr.discard(row.experiment_id)
            except Exception:
                pass
    except Exception:
        pass

    vlog(f"cleaned up {USER_ID}")


def _make_programmer():
    from core.memory.experiment import MemoryExperimentManager
    from core.memory.factory import create_editor
    from core.memory.programmer import MemoryProgrammer

    editor = create_editor(SessionLocal, user_id=USER_ID)
    experiments = MemoryExperimentManager(SessionLocal, source_db=DB_NAME)
    return MemoryProgrammer(editor, experiments, SessionLocal)


def _parse(yaml_str: str) -> list[dict]:
    from core.memory.programmer import parse_script
    return parse_script(yaml_str)


# ── Scenario 1: Sandbox inject ───────────────────────────────────────

def test_sandbox_inject() -> str | None:
    print("\n── 1. Sandbox inject ──")

    actions = _parse("version: 1\nactions:\n  - inject:\n      content: 'verify sandbox test'\n      type: semantic\n      trust: T1\n")
    prog = _make_programmer()
    mem_before = count("mem_memories")

    result = prog.execute(USER_ID, actions, sandbox=True, dry_run=False, program_name="verify")
    vlog(f"result: executed={result.actions_executed}, exp={result.experiment_id}")

    check("executed 1 action", result.actions_executed == 1)
    check("has experiment_id", result.experiment_id is not None)
    check("production unchanged", count("mem_memories") == mem_before)

    if result.experiment_id:
        exp = query_one(
            "SELECT status, branch_db FROM mem_experiments WHERE experiment_id = :eid",
            eid=result.experiment_id,
        )
        check("experiment status=active", exp is not None and exp.status == "active")

    return result.experiment_id


# ── Scenario 2: Commit ───────────────────────────────────────────────

def test_commit(experiment_id: str) -> None:
    print("\n── 2. Commit ──")
    from core.memory.experiment import MemoryExperimentManager

    mem_before = count("mem_memories")
    mgr = MemoryExperimentManager(SessionLocal, source_db=DB_NAME)
    mgr.commit(experiment_id)

    check("memory count +1", count("mem_memories") == mem_before + 1)

    exp = query_one("SELECT status FROM mem_experiments WHERE experiment_id = :eid", eid=experiment_id)
    check("experiment committed", exp is not None and exp.status == "committed")

    # field-level verification
    mem = query_one(
        "SELECT content, memory_type, trust_tier, is_active, user_id "
        "FROM mem_memories WHERE user_id = :uid ORDER BY created_at DESC LIMIT 1",
        uid=USER_ID,
    )
    check("content correct", mem is not None and "verify sandbox test" in (mem.content or ""))
    check("is_active=1", mem is not None and mem.is_active == 1)
    check("user_id correct", mem is not None and mem.user_id == USER_ID)


# ── Scenario 3: Dry-run ──────────────────────────────────────────────

def test_dry_run() -> None:
    print("\n── 3. Dry-run ──")

    actions = _parse("version: 1\nactions:\n  - inject:\n      content: 'should not persist'\n      type: semantic\n")
    prog = _make_programmer()
    mem_before = count("mem_memories")
    audit_before = count("mem_edit_log")

    result = prog.execute(USER_ID, actions, sandbox=False, dry_run=True, program_name="verify")

    check("dry_run=True", result.dry_run is True)
    check("memories unchanged", count("mem_memories") == mem_before)
    check("audits unchanged", count("mem_edit_log") == audit_before)


# ── Scenario 4: Discard ──────────────────────────────────────────────

def test_discard() -> None:
    print("\n── 4. Discard ──")
    from core.memory.experiment import MemoryExperimentManager

    actions = _parse("version: 1\nactions:\n  - inject:\n      content: 'to discard'\n      type: semantic\n")
    prog = _make_programmer()
    mem_before = count("mem_memories")

    result = prog.execute(USER_ID, actions, sandbox=True, dry_run=False, program_name="verify")
    exp_id = result.experiment_id

    mgr = MemoryExperimentManager(SessionLocal, source_db=DB_NAME)
    mgr.discard(exp_id)

    check("production unchanged", count("mem_memories") == mem_before)
    exp = query_one("SELECT status FROM mem_experiments WHERE experiment_id = :eid", eid=exp_id)
    check("experiment discarded", exp is not None and exp.status == "discarded")


# ── Scenario 5: Direct write + dual audit ────────────────────────────

def test_direct_write() -> None:
    print("\n── 5. Direct write (no sandbox) ──")

    actions = _parse("version: 1\nactions:\n  - inject:\n      content: 'direct write verify'\n      type: semantic\n      trust: T1\n")
    prog = _make_programmer()
    mem_before = count("mem_memories")
    audit_before = count("mem_edit_log")

    result = prog.execute(USER_ID, actions, sandbox=False, dry_run=False, program_name="verify")

    check("memory count +1", count("mem_memories") == mem_before + 1)
    check("dual audit (>=+2)", count("mem_edit_log") >= audit_before + 2,
          f"was {audit_before}, now {count('mem_edit_log')}")

    mem = query_one(
        "SELECT content, user_id FROM mem_memories "
        "WHERE user_id = :uid ORDER BY created_at DESC LIMIT 1",
        uid=USER_ID,
    )
    check("content correct", mem is not None and "direct write verify" in (mem.content or ""))


# ── Scenario 6: Multi-turn (inject → correct → verify) ───────────────

def test_multi_turn() -> None:
    print("\n── 6. Multi-turn (inject → correct) ──")

    prog = _make_programmer()

    # Turn 1: inject
    r1 = prog.execute(USER_ID, _parse(
        "version: 1\nactions:\n  - inject:\n      content: 'prefers Python for data analysis'\n      type: semantic\n"
    ), sandbox=False, dry_run=False, program_name="verify")

    mem1_id = r1.results[0].detail.get("memory_id") if r1.results else None
    check("turn1 injected", mem1_id is not None)

    if not mem1_id:
        return

    # Turn 2: correct
    r2 = prog.execute(USER_ID, _parse(json.dumps(
        {"version": 1, "actions": [{"correct": {
            "memory_id": mem1_id,
            "new_content": "prefers Rust for data analysis now",
            "reason": "preference changed",
        }}]}
    )), sandbox=False, dry_run=False, program_name="verify")

    check("turn2 corrected", r2.actions_executed == 1)

    # Verify the corrected memory
    new_id = r2.results[0].detail.get("new_id") if r2.results else None
    if new_id:
        mem = query_one(
            "SELECT content FROM mem_memories WHERE memory_id = :mid", mid=new_id,
        )
        check("corrected content", mem is not None and "Rust" in (mem.content or ""))

    # Verify audit chain
    audits = query_all(
        "SELECT operation FROM mem_edit_log WHERE user_id = :uid ORDER BY created_at",
        uid=USER_ID,
    )
    ops = [a.operation for a in audits]
    check("audit has inject+correct", "inject" in ops and "correct" in ops)


# ── Scenario 7: NL → Script (real LLM) ───────────────────────────────

def test_nl_to_script() -> None:
    print("\n── 7. NL → Script (real LLM) ──")
    from core.llm.client import LLMClient
    from core.memory.programmer import nl_to_script

    llm = LLMClient(SessionLocal)
    instruction = f"Remember that user {USER_ID} prefers Python for data analysis"

    try:
        actions = nl_to_script(instruction, USER_ID, llm)
    except Exception as e:
        fail("nl_to_script", str(e))
        return

    check("LLM returned actions", len(actions) > 0, f"got {len(actions)}")
    vlog(f"actions: {json.dumps(actions, ensure_ascii=False)}")

    # Execute in sandbox to verify full chain
    prog = _make_programmer()
    mem_before = count("mem_memories")

    result = prog.execute(USER_ID, actions, sandbox=True, dry_run=False, program_name="verify-nl")
    check("sandbox executed", result.actions_executed >= 1)
    check("production unchanged", count("mem_memories") == mem_before)
    check("has experiment", result.experiment_id is not None)


# ── Scenario 8: CLI session state (-m + --session-id) ────────────────

def test_session_state() -> None:
    print("\n── 8. CLI session state (-m + --session-id) ──")
    import subprocess

    cli = [sys.executable, str(Path(__file__).parent.parent.parent / "cli" / "mo_agent_api.py")]

    def run(*args: str) -> str:
        r = subprocess.run(cli + list(args), capture_output=True, text=True)
        return r.stdout.strip()

    # Resolve model: explicit > cheapest
    from core.llm.model_resolver import _resolve_cheapest
    cheapest = _model or _resolve_cheapest("deepseek-chat")

    # Turn 1 — no session-id, auto-create
    run("chat", "-m", "hello, my name is verify-test", "--auto-approve", "--model", cheapest)

    # Read session_id from credentials
    import json
    cred_path = Path.home() / ".mo-agent" / "credentials.json"
    creds = json.loads(cred_path.read_text())
    current = creds.get("current_profile", "default")
    sid = creds["profiles"][current]["last_session_id"]
    check("session_id created", bool(sid), sid)
    vlog(f"session_id={sid}")

    # Turn 2 — reuse session
    turn2_response = run("chat", "-m", "what did I just say my name was?", "--session-id", sid, "--auto-approve", "--model", cheapest)

    # Verify turn2 response references turn1 content (LLM actually saw history)
    check("turn2 recalls name", "verify-test" in turn2_response.lower(), f"response: {turn2_response[:100]}")

    # Shared session integrity checks (same logic as verify-talk)
    from scripts.e2e.talk_checks import check_session_integrity, check_turn_count_increases
    prev_counts: dict = {}
    for r in check_session_integrity(SessionLocal, sid):
        check(r.name, r.passed, r.message)
    r = check_turn_count_increases(SessionLocal, sid, prev_counts)
    check(r.name, r.passed, r.message)


# ── Main ──────────────────────────────────────────────────────────────

_model: str | None = None


def main() -> None:
    global _verbose, _model

    parser = argparse.ArgumentParser(description="E2E Verification")
    parser.add_argument("--with-llm", action="store_true", help="Include NL→Script + session state tests (needs LLM + API server)")
    parser.add_argument("--model", default=None, help="LLM model to use (default: cheapest)")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    _verbose = args.verbose
    _model = args.model

    print(f"E2E Verification — user={USER_ID}  db={DB_NAME}")

    # Clean any leftover __verify_ data
    cleanup()

    try:
        exp_id = test_sandbox_inject()
        if exp_id:
            test_commit(exp_id)
        test_dry_run()
        test_discard()
        test_direct_write()
        test_multi_turn()

        if args.with_llm:
            test_nl_to_script()
            test_session_state()
    finally:
        cleanup()

    print(f"\n{'='*50}")
    print(f"Result: {_passed} passed, {_failed} failed")
    sys.exit(1 if _failed else 0)


if __name__ == "__main__":
    main()
