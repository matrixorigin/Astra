#!/usr/bin/env python3
"""Astra runner for SWE-bench Pro local Docker experiments."""

from __future__ import annotations

import argparse
import json
import os
import uuid
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


def run(
    args: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    timeout: int | None = None,
    stdout_path: Path | None = None,
    stderr_path: Path | None = None,
    input_text: str | None = None,
) -> subprocess.CompletedProcess[str]:
    stdout_f = stdout_path.open("w") if stdout_path else subprocess.PIPE
    stderr_f = stderr_path.open("w") if stderr_path else subprocess.PIPE
    try:
        return subprocess.run(
            args,
            cwd=str(cwd) if cwd else None,
            env=env,
            input=input_text,
            text=True,
            stdout=stdout_f,
            stderr=stderr_f,
            timeout=timeout,
        )
    finally:
        if stdout_path:
            stdout_f.close()
        if stderr_path:
            stderr_f.close()


def fail_if_bad(proc: subprocess.CompletedProcess[str], label: str, stderr_path: Path | None = None) -> None:
    if proc.returncode == 0:
        return
    stderr = stderr_path.read_text(errors="replace") if stderr_path and stderr_path.exists() else (proc.stderr or "")
    raise RuntimeError(f"{label} failed with exit {proc.returncode}:\n{stderr[-4000:]}")


def write_json(path: Path, data: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")


def read_jsonl(path: Path) -> list[dict]:
    rows = []
    with path.open() as f:
        for line in f:
            if line.strip():
                rows.append(json.loads(line))
    return rows


def select_instances(path: Path, instance_ids: list[str] | None, limit: int | None) -> list[dict]:
    rows = read_jsonl(path)
    if instance_ids:
        wanted = set(instance_ids)
        rows = [row for row in rows if row["instance_id"] in wanted]
        missing = wanted - {row["instance_id"] for row in rows}
        if missing:
            raise ValueError(f"unknown instance ids: {sorted(missing)}")
    if limit is not None:
        rows = rows[:limit]
    return rows


def prompt_for_instance(instance: dict) -> str:
    return f"""We need solve one SWE-bench Pro issue.

Repository: {instance.get("repo", "")}
Instance: {instance["instance_id"]}
Base commit: {instance.get("base_commit", "")}

Problem statement:
{instance.get("problem_statement", "")}

Instructions:
- You are already inside the target repository checkout.
- Edit source files to fix the described issue.
- Do not modify tests or unrelated files.
- Keep the patch minimal.
- Run focused checks if useful.
- Stop after the fix is applied. In your final response, summarize the changed files only.
"""


def sh_single_quote(value: str) -> str:
    return "'" + value.replace("'", "'\\''") + "'"


def docker_exec(
    container: str,
    command: str,
    *,
    timeout: int,
    stdout_path: Path | None = None,
    stderr_path: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    return run(
        ["docker", "exec", container, "bash", "-lc", command],
        timeout=timeout,
        stdout_path=stdout_path,
        stderr_path=stderr_path,
    )


def start_container(instance: dict, out_dir: Path, pull_timeout: int) -> str:
    image = instance.get("docker_image")
    if not image:
        raise ValueError(f"missing docker_image for {instance['instance_id']}")
    safe_tail = instance["instance_id"][-40:].replace("_", "-").replace("/", "-")
    name = f"astra-pro-{safe_tail}-{uuid.uuid4().hex[:8]}"
    proc = run(
        ["docker", "pull", image],
        timeout=pull_timeout,
        stdout_path=out_dir / "docker_pull_stdout.log",
        stderr_path=out_dir / "docker_pull_stderr.log",
    )
    fail_if_bad(proc, f"docker pull {image}", out_dir / "docker_pull_stderr.log")
    proc = run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            name,
            "--network",
            "host",
            "-w",
            "/app",
            "--entrypoint",
            "/bin/bash",
            image,
            "-lc",
            "sleep 2h",
        ],
        timeout=120,
        stdout_path=out_dir / "docker_run_stdout.log",
        stderr_path=out_dir / "docker_run_stderr.log",
    )
    fail_if_bad(proc, f"docker run {image}", out_dir / "docker_run_stderr.log")
    return name


def copy_repo_from_container(container: str, out_dir: Path) -> Path:
    worktree = out_dir / "worktree"
    if worktree.exists():
        shutil.rmtree(worktree)
    proc = run(["docker", "cp", f"{container}:/app", str(worktree)], timeout=600)
    fail_if_bad(proc, "docker cp /app from container")
    return worktree


def copy_session_artifacts(session_id: str | None, out_dir: Path) -> None:
    if not session_id:
        return
    sessions = Path.home() / ".astra" / "sessions"
    journal = sessions / f"{session_id}.jsonl"
    step_dir = sessions / session_id
    target = out_dir / "astra_session"
    target.mkdir(exist_ok=True)
    if journal.exists():
        shutil.copy2(journal, target / journal.name)
    if step_dir.exists():
        shutil.copytree(step_dir, target / session_id, dirs_exist_ok=True)


def copy_container_astra_home(container: str, out_dir: Path) -> None:
    target = out_dir / "astra_home"
    if target.exists():
        shutil.rmtree(target)
    proc = run(["docker", "cp", f"{container}:/root/.astra", str(target)], timeout=300)
    # Older/minimal images may not have this path if Astra failed before startup.
    if proc.returncode != 0:
        (out_dir / "copy_astra_home_error.txt").write_text((proc.stderr or proc.stdout or "")[-4000:])


def prepare_isolated_credentials(out_dir: Path) -> Path:
    src = Path.home() / ".astra" / "credentials.json"
    if not src.exists():
        raise FileNotFoundError(f"missing Astra credentials: {src}")
    creds_dir = out_dir / "credentials"
    creds_dir.mkdir(exist_ok=True)
    data = json.loads(src.read_text())
    for profile in (data.get("profiles") or {}).values():
        if isinstance(profile, dict):
            profile["last_session_id"] = None
    dst = creds_dir / "credentials.json"
    dst.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    dst.chmod(0o600)
    return creds_dir


def cache_hit_rate(cache: dict, prompt_tokens: int) -> float:
    read = int(cache.get("read_tokens") or 0)
    return read / prompt_tokens if prompt_tokens else 0.0


RESUMABLE_INTERRUPTION_KINDS = {
    "budget_exhausted",
    "circuit_breaker",
    "circuit_breaker_abort",
    "empty_completion",
    "harness_blocked",
    "harness_paused",
    "stream_idle",
    "stream_transport",
}

RESUME_TEXT_MARKERS = (
    "[empty_completion]",
    "Circuit breaker abort",
    "checkpoint was saved",
)

RESUME_PROMPT = """Continue from the existing evidence. You have not produced a usable patch yet.
Do not repeat broad exploration. Apply the minimal code change required to fix the issue.
If you believe no fix is possible, state that explicitly after checking the most relevant evidence.
"""


def to_int(value: object) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def read_astra_result(path: Path) -> dict:
    text = path.read_text(errors="replace") if path.exists() else ""
    if not text.strip():
        return {}
    try:
        return json.loads(text)
    except json.JSONDecodeError as exc:
        return {"json_error": str(exc), "raw_tail": text[-4000:]}


def result_interruption_kind(result: dict) -> str | None:
    kind = result.get("interruption_kind")
    if kind:
        return str(kind)
    interruption = result.get("interruption")
    if isinstance(interruption, dict):
        kind = interruption.get("kind") or interruption.get("interruption_kind")
        if kind:
            return str(kind)
    return None


def result_final_state(result: dict) -> str:
    state = result.get("final_state")
    if state:
        return str(state)
    if result_interruption_kind(result):
        return "interrupted"
    text = str(result.get("text") or "")
    if not text.strip():
        return "empty"
    return "completed" if result.get("success") is True else "missing"


def detect_failure_signal(result: dict, patch: str) -> str:
    kind = result_interruption_kind(result)
    if kind:
        return kind
    state = result_final_state(result)
    if state == "empty":
        return "empty_completion"
    text = str(result.get("text") or "")
    if "Circuit breaker abort" in text:
        return "circuit_breaker_abort"
    if "[empty_completion]" in text:
        return "empty_completion"
    if "checkpoint was saved" in text:
        return "checkpoint_saved"
    if not patch.strip():
        return "empty_patch"
    return "none"


def should_resume_attempt(args: argparse.Namespace, result: dict, patch: str, attempt_index: int) -> bool:
    if not args.resume_on_empty_patch:
        return False
    if attempt_index >= args.max_resume_attempts:
        return False
    state = result_final_state(result)
    kind = result_interruption_kind(result)
    text = str(result.get("text") or "")
    if not patch.strip():
        return True
    if state == "empty":
        return True
    if kind in RESUMABLE_INTERRUPTION_KINDS:
        return True
    return any(marker in text for marker in RESUME_TEXT_MARKERS)


def pipeline_env_overrides(args: argparse.Namespace) -> dict[str, str]:
    """ASTRA_PIPELINE_* variables selecting the context-pipeline experiment arm.

    The pipeline assembles inside the API server process, so the server at
    --api-url must be launched with the SAME variables; they are also passed
    to the astra CLI here so embedded-runtime paths follow the arm too.
    """
    env: dict[str, str] = {}
    if args.pipeline_pressure_mode:
        env["ASTRA_PIPELINE_PRESSURE_MODE"] = args.pipeline_pressure_mode
    if args.pipeline_assembler_mode:
        env["ASTRA_PIPELINE_ASSEMBLER_MODE"] = args.pipeline_assembler_mode
    if args.pipeline_tier_thresholds:
        env["ASTRA_PIPELINE_TIER_THRESHOLDS"] = args.pipeline_tier_thresholds
    if args.pipeline_reserve_percentiles:
        env["ASTRA_PIPELINE_RESERVE_PERCENTILES"] = args.pipeline_reserve_percentiles
    return env


def astra_chat_command(args: argparse.Namespace, session_id: str | None) -> list[str]:
    cmd = [
        args.astra_bin,
        "chat",
        "--stdin",
        "--json",
        "--quiet",
        "--stream-events",
        "--permission-mode",
        "auto",
        "--benchmark-profile",
        args.benchmark_profile,
        "--no-resume",
        "--model",
        args.model,
    ]
    if session_id:
        cmd.extend(["--session-id", session_id])
    return cmd


def run_astra_attempt(
    args: argparse.Namespace,
    *,
    attempt_dir: Path,
    worktree: Path,
    env: dict[str, str],
    input_text: str,
    session_id: str | None,
) -> tuple[subprocess.CompletedProcess[str], dict]:
    attempt_dir.mkdir(parents=True, exist_ok=True)
    attempt_env = env.copy()
    attempt_env["ASTRA_LOG_FILE"] = str(attempt_dir / "astra_cli_json.log")
    proc = run(
        astra_chat_command(args, session_id),
        cwd=worktree,
        env=attempt_env,
        timeout=args.astra_timeout,
        stdout_path=attempt_dir / "astra_stdout.json",
        stderr_path=attempt_dir / "astra_stderr_events.jsonl",
        input_text=input_text,
    )
    return proc, read_astra_result(attempt_dir / "astra_stdout.json")


def git_diff_and_status(worktree: Path, out_dir: Path) -> tuple[str, str]:
    status_proc = run(["git", "status", "--short"], cwd=worktree, timeout=120)
    fail_if_bad(status_proc, "git status in worktree")
    status = status_proc.stdout or ""

    # git diff ignores untracked files. Mark them as intent-to-add so generated
    # patches include new source files created by the agent.
    add_proc = run(["git", "add", "-N", "--", "."], cwd=worktree, timeout=120)
    fail_if_bad(add_proc, "git add -N in worktree")
    diff_proc = run(["git", "diff", "--binary"], cwd=worktree, timeout=120)
    fail_if_bad(diff_proc, "git diff in worktree")
    patch = diff_proc.stdout or ""
    (out_dir / "patch.diff").write_text(patch)
    (out_dir / "git_status_after.txt").write_text(status)
    return patch, status


def attempt_usage(result: dict) -> dict:
    cache = result.get("cache") or {}
    prompt_tokens = to_int(result.get("prompt_tokens") or result.get("fresh_prompt_tokens"))
    return {
        "prompt_tokens": prompt_tokens,
        "fresh_prompt_tokens": to_int(result.get("fresh_prompt_tokens")),
        "completion_tokens": to_int(result.get("completion_tokens")),
        "cache_read_tokens": to_int(cache.get("read_tokens")),
        "cache_creation_tokens": to_int(cache.get("creation_tokens")),
    }


def sum_attempt_usage(attempts: list[dict]) -> dict:
    usage = {
        "prompt_tokens": 0,
        "fresh_prompt_tokens": 0,
        "completion_tokens": 0,
        "cache_read_tokens": 0,
        "cache_creation_tokens": 0,
    }
    for attempt in attempts:
        for key in usage:
            usage[key] += to_int(attempt.get(key))
    return usage


def write_pred_artifacts(out_dir: Path, instance_id: str, model: str, patch: str) -> None:
    pred_json = {"instance_id": instance_id, "model_name_or_path": model, "model_patch": patch, "patch": patch}
    (out_dir / f"{instance_id}.pred").write_text(json.dumps(pred_json, ensure_ascii=False) + "\n")
    (out_dir / "patch.diff").write_text(patch)


def run_astra_on_instance(args: argparse.Namespace, instance: dict, run_dir: Path) -> dict:
    instance_id = instance["instance_id"]
    out_dir = run_dir / args.model / instance_id
    out_dir.mkdir(parents=True, exist_ok=True)
    write_json(out_dir / "instance.json", instance)
    prompt = prompt_for_instance(instance)
    (out_dir / "prompt.txt").write_text(prompt)

    container = None
    started = time.time()
    try:
        container = start_container(instance, out_dir, args.pull_timeout)
        worktree = copy_repo_from_container(container, out_dir)
        run(["docker", "rm", "-f", container], timeout=120)
        container = None

        env = os.environ.copy()
        env["ASTRA_API_URL"] = args.api_url
        env["ASTRA_CAPTURE_FULL_LLM"] = "1"
        env["ASTRA_CLI_CREDENTIALS_DIR"] = str(prepare_isolated_credentials(out_dir))
        env["NO_PROXY"] = "127.0.0.1,localhost"
        env["no_proxy"] = "127.0.0.1,localhost"
        env.update(pipeline_env_overrides(args))

        attempts = []
        session_id = None
        astra_result = {}
        proc = None
        patch = ""
        status = ""
        input_text = prompt
        final_attempt_dir = out_dir / "attempt_0"

        for attempt_index in range(args.max_resume_attempts + 1):
            attempt_dir = out_dir / f"attempt_{attempt_index}"
            final_attempt_dir = attempt_dir
            proc, astra_result = run_astra_attempt(
                args,
                attempt_dir=attempt_dir,
                worktree=worktree,
                env=env,
                input_text=input_text,
                session_id=session_id,
            )
            session_id = astra_result.get("session_id") or session_id
            patch, status = git_diff_and_status(worktree, attempt_dir)
            usage = attempt_usage(astra_result)
            final_state = result_final_state(astra_result)
            interruption_kind = result_interruption_kind(astra_result)
            failure_signal = detect_failure_signal(astra_result, patch)
            resume_next = bool(session_id) and should_resume_attempt(args, astra_result, patch, attempt_index)
            attempts.append(
                {
                    "attempt": attempt_index,
                    "exit_code": proc.returncode,
                    "success": astra_result.get("success"),
                    "session_id": session_id,
                    "final_state": final_state,
                    "interruption_kind": interruption_kind,
                    "failure_signal": failure_signal,
                    "patch_bytes": len(patch.encode()),
                    "empty_patch": not patch.strip(),
                    "resume_triggered": resume_next,
                    **usage,
                }
            )
            if not resume_next:
                break
            input_text = RESUME_PROMPT

        duration_s = time.time() - started

        shutil.copy2(final_attempt_dir / "astra_stdout.json", out_dir / "astra_stdout.json")
        shutil.copy2(final_attempt_dir / "astra_stderr_events.jsonl", out_dir / "astra_stderr_events.jsonl")
        shutil.copy2(final_attempt_dir / "patch.diff", out_dir / "patch.diff")
        shutil.copy2(final_attempt_dir / "git_status_after.txt", out_dir / "git_status_after.txt")
        write_pred_artifacts(out_dir, instance_id, args.model, patch)
        (out_dir / "git_status_after.txt").write_text(status)

        copy_session_artifacts(session_id, out_dir)
        usage = sum_attempt_usage(attempts)
        final_state = result_final_state(astra_result)
        interruption_kind = result_interruption_kind(astra_result)
        initial_empty_patch = bool(attempts[0]["empty_patch"]) if attempts else True
        final_empty_patch = not patch.strip()
        interrupted = final_state == "interrupted" or bool(interruption_kind)
        failure_signal = detect_failure_signal(astra_result, patch)
        metrics = {
            "instance_id": instance_id,
            "model": args.model,
            "arm_label": args.arm_label,
            "astra_exit_code": proc.returncode if proc else None,
            "astra_success": astra_result.get("success"),
            "astra_session_id": session_id,
            "astra_run_id": astra_result.get("run_id"),
            "duration_s": duration_s,
            "prompt_tokens": usage["prompt_tokens"],
            "fresh_prompt_tokens": usage["fresh_prompt_tokens"],
            "completion_tokens": usage["completion_tokens"],
            "cache_read_tokens": usage["cache_read_tokens"],
            "cache_creation_tokens": usage["cache_creation_tokens"],
            "cache_hit": bool(usage["cache_read_tokens"]),
            "cache_hit_rate": usage["cache_read_tokens"] / usage["prompt_tokens"] if usage["prompt_tokens"] else 0.0,
            "final_state": final_state,
            "interruption_kind": interruption_kind,
            "interrupted": interrupted,
            "interrupted_with_patch": interrupted and bool(patch.strip()),
            "failure_signal": failure_signal,
            "resume_attempts": max(len(attempts) - 1, 0),
            "initial_empty_patch": initial_empty_patch,
            "final_empty_patch": final_empty_patch,
            "patch_bytes": len(patch.encode()),
            "empty_patch": not patch.strip(),
            "attempts": attempts,
        }
        write_json(out_dir / "metrics.json", metrics)
        return metrics
    finally:
        if container:
            run(["docker", "rm", "-f", container], timeout=120)


def evaluate(args: argparse.Namespace, run_dir: Path, prefix: str, instances_path: Path) -> dict:
    patches_json = run_dir / f"{prefix}_patches.json"
    eval_dir = run_dir / f"{prefix}_eval"
    eval_dir.mkdir(parents=True, exist_ok=True)
    proc = run(
        [
            sys.executable,
            str(args.pro_repo / "helper_code" / "gather_patches.py"),
            "--directory",
            str(run_dir / args.model),
            "--prefix",
            prefix,
            "--output",
            str(patches_json),
        ],
        cwd=args.pro_repo,
        timeout=300,
        stdout_path=run_dir / "gather_stdout.log",
        stderr_path=run_dir / "gather_stderr.log",
    )
    fail_if_bad(proc, "gather patches", run_dir / "gather_stderr.log")
    proc_eval = run(
        [
            sys.executable,
            str(args.pro_repo / "swe_bench_pro_eval.py"),
            "--raw_sample_path",
            str(instances_path),
            "--patch_path",
            str(patches_json),
            "--output_dir",
            str(eval_dir),
            "--scripts_dir",
            str(args.pro_repo / "run_scripts"),
            "--num_workers",
            str(args.eval_workers),
            "--dockerhub_username",
            "jefzda",
            "--use_local_docker",
        ],
        cwd=args.pro_repo,
        timeout=args.eval_timeout,
        stdout_path=run_dir / "pro_eval_stdout.log",
        stderr_path=run_dir / "pro_eval_stderr.log",
    )
    result = {"exit_code": proc_eval.returncode, "patches_json": str(patches_json), "eval_dir": str(eval_dir)}
    eval_results = eval_dir / "eval_results.json"
    if eval_results.exists():
        result["eval_results"] = json.loads(eval_results.read_text())
    if proc_eval.returncode != 0:
        result["error_tail"] = (run_dir / "pro_eval_stderr.log").read_text(errors="replace")[-4000:]
    write_json(run_dir / "eval_summary.json", result)
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--instances", type=Path, required=True)
    parser.add_argument("--instance-ids", nargs="*")
    parser.add_argument("--limit", type=int)
    parser.add_argument("--run-id", default=datetime.now(timezone.utc).strftime("astra-pro-%Y%m%dT%H%M%SZ"))
    parser.add_argument("--run-root", type=Path, default=Path.home() / "astra-eval-runs" / "swebench-pro")
    parser.add_argument("--pro-repo", type=Path, default=Path.home() / "SWE-bench_Pro-os")
    parser.add_argument("--astra-repo", type=Path, default=Path.home() / "code" / "astra")
    parser.add_argument("--astra-bin", default=str(Path.home() / "code" / "astra" / "rust" / "target" / "debug" / "astra"))
    parser.add_argument("--api-url", default="http://127.0.0.1:8010")
    parser.add_argument("--astra-timeout", type=int, default=3600)
    parser.add_argument("--eval-timeout", type=int, default=7200)
    parser.add_argument("--eval-workers", type=int, default=1)
    parser.add_argument("--pull-timeout", type=int, default=1200)
    parser.add_argument("--benchmark-profile", default="swebench")
    parser.add_argument(
        "--arm-label",
        default="production-default",
        help="Experiment arm label stamped into run_metadata.json and every metrics.json.",
    )
    parser.add_argument(
        "--pipeline-pressure-mode",
        choices=["predictive", "reactive"],
        help="Context-pipeline tier selection (ablation X2a). Server at --api-url must run with the same ASTRA_PIPELINE_PRESSURE_MODE.",
    )
    parser.add_argument(
        "--pipeline-assembler-mode",
        choices=["structured", "flat"],
        help="Structured pipeline vs flat baseline (experiment E2). Server must run with the same ASTRA_PIPELINE_ASSEMBLER_MODE.",
    )
    parser.add_argument(
        "--pipeline-tier-thresholds",
        help='Compaction-tier ladder, e.g. "0.60,0.75,0.90" (sweep X2b). Server must run with the same ASTRA_PIPELINE_TIER_THRESHOLDS.',
    )
    parser.add_argument(
        "--pipeline-reserve-percentiles",
        help='Reserve percentiles steady,recovery, e.g. "0.75,0.95" (sweep X2c). Server must run with the same ASTRA_PIPELINE_RESERVE_PERCENTILES.',
    )
    parser.add_argument("--max-resume-attempts", type=int, default=2)
    parser.add_argument("--resume-on-empty-patch", action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument("--evaluate", action=argparse.BooleanOptionalAction, default=True)
    args = parser.parse_args()

    run_dir = args.run_root / args.run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    instances = select_instances(args.instances, args.instance_ids, args.limit)
    astra_commit = (run(["git", "rev-parse", "HEAD"], cwd=args.astra_repo, timeout=120).stdout or "").strip()
    astra_status = run(["git", "status", "--short"], cwd=args.astra_repo, timeout=120).stdout or ""
    write_json(
        run_dir / "run_metadata.json",
        {
            "run_id": args.run_id,
            "created_at": datetime.now(timezone.utc).isoformat(),
            "model": args.model,
            "instances": str(args.instances),
            "instance_ids": [inst["instance_id"] for inst in instances],
            "astra_commit": astra_commit,
            "astra_status": astra_status,
            "astra_dirty": bool(astra_status.strip()),
            "astra_api_url": args.api_url,
            "pro_repo": str(args.pro_repo),
            "benchmark_profile": args.benchmark_profile,
            "max_resume_attempts": args.max_resume_attempts,
            "resume_on_empty_patch": args.resume_on_empty_patch,
            "arm_label": args.arm_label,
            "pipeline_env": pipeline_env_overrides(args),
            "pipeline_env_note": (
                "Context assembly runs in the API server; the server at astra_api_url "
                "must be launched with the same ASTRA_PIPELINE_* variables for the arm "
                "to take effect. Check the server log for 'pipeline experiment overrides active'."
            ),
        },
    )
    all_metrics = []
    for inst in instances:
        try:
            metrics = run_astra_on_instance(args, inst, run_dir)
        except Exception as exc:
            metrics = {
                "instance_id": inst["instance_id"],
                "model": args.model,
                "arm_label": args.arm_label,
                "error": str(exc),
                "duration_s": None,
                "empty_patch": True,
            }
            write_json(run_dir / args.model / inst["instance_id"] / "metrics.json", metrics)
        all_metrics.append(metrics)
        print(json.dumps(metrics, ensure_ascii=False), flush=True)
    write_json(run_dir / "metrics_summary.json", all_metrics)
    if args.evaluate:
        eval_summary = evaluate(args, run_dir, args.model, args.instances)
        result_map = eval_summary.get("eval_results") or {}
        for metrics in all_metrics:
            iid = metrics["instance_id"]
            metrics["eval_completed"] = iid in result_map
            metrics["resolved"] = bool(result_map.get(iid, False)) if iid in result_map else None
        write_json(run_dir / "metrics_summary.json", all_metrics)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
