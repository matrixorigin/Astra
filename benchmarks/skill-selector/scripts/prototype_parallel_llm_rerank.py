#!/usr/bin/env python3
from __future__ import annotations

import argparse
import concurrent.futures
import json
import math
import re
import statistics
import threading
import time
from pathlib import Path
from typing import Any

import numpy as np

from prototype_llm_top100_rerank import (
    DEFAULT_EMBED_MODEL,
    RESULTS_DIR,
    Record,
    Skill,
    candidate_text,
    chat_completion,
    extract_json,
    load_embedding_cache,
    load_latest_records,
    load_model_config,
    load_skills,
)


lock = threading.Lock()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Benchmark single vs shard-final cheap-LLM reranking.")
    parser.add_argument("--rerank-model", default="qwen-flash")
    parser.add_argument("--embed-model", default=DEFAULT_EMBED_MODEL)
    parser.add_argument("--limit", type=int, default=None)
    parser.add_argument("--concurrency", type=int, default=4)
    parser.add_argument("--top-k", type=int, default=100)
    parser.add_argument("--max-return", type=int, default=30)
    parser.add_argument("--strategy", choices=["single", "shard-final"], default="shard-final")
    parser.add_argument("--shard-count", type=int, default=4)
    parser.add_argument("--shard-return", type=int, default=8)
    parser.add_argument("--anchor-count", type=int, default=0)
    parser.add_argument(
        "--rows-out",
        default=str(RESULTS_DIR / "parallel-llm-rerank-rows.jsonl"),
    )
    parser.add_argument(
        "--summary-out",
        default=str(RESULTS_DIR / "parallel-llm-rerank-summary.json"),
    )
    return parser.parse_args()


def rerank_candidates(
    *,
    model: Any,
    prompt: str,
    skills: list[Skill],
    candidate_indices: list[int],
    max_return: int,
    phase: str,
) -> list[int]:
    candidate_total = len(candidate_indices)
    wanted = min(max_return, candidate_total)
    system_prompt = (
        "You are a cheap skill selector reranker. "
        f"Given a user request and {candidate_total} candidate skills, return ONLY JSON. "
        "Do not explain. Do not invent candidates. "
        "Rank the most likely matching candidates best-first. "
        "Bias toward recall: include plausible candidates instead of being overly conservative."
    )
    recall_hint = (
        "Prefer recall over precision for positions 10-30."
        if wanted >= 10
        else "Prefer broader recall over narrow precision."
    )
    user_prompt = (
        f"Phase: {phase}\n\n"
        f"User request:\n{prompt}\n\n"
        f"Candidate skills:\n{candidate_text(skills, candidate_indices)}\n\n"
        f"Return JSON in this exact shape:\n"
        f'{{"ranked_candidate_numbers":[1,2,3]}}\n'
        f"Rules:\n"
        f"- Use only candidate numbers from the list above.\n"
        f"- Return exactly {wanted} unique integers.\n"
        f"- Sort best-first.\n"
        f"- If uncertain, still fill all {wanted} slots with your best broad-recall ranking.\n"
        f"- {recall_hint}\n"
    )
    last_raw = ""
    for _ in range(3):
        raw = chat_completion(model, system_prompt, user_prompt)
        last_raw = raw
        nums: list[Any] = []
        try:
            data = extract_json(raw)
            if isinstance(data, list):
                nums = data
            else:
                nums = data.get("ranked_candidate_numbers") or data.get("candidates") or []
        except Exception:
            nums = re.findall(r"\d+", raw)
        out = []
        seen = set()
        for item in nums:
            try:
                num = int(item)
            except (TypeError, ValueError):
                continue
            if num < 1 or num > candidate_total or num in seen:
                continue
            seen.add(num)
            out.append(num)
            if len(out) >= wanted:
                break
        if len(out) == wanted:
            return out
        if out:
            remaining = [idx for idx in range(1, candidate_total + 1) if idx not in seen]
            out.extend(remaining[: wanted - len(out)])
            return out
    raise RuntimeError(f"failed to parse reranker output: {last_raw[:400]}")


def contiguous_shards(candidate_indices: list[int], shard_count: int, anchor_count: int) -> list[list[int]]:
    if shard_count <= 0:
        raise ValueError("shard_count must be > 0")
    anchors = candidate_indices[: max(0, anchor_count)]
    chunk_size = math.ceil(len(candidate_indices) / shard_count)
    shards: list[list[int]] = []
    for shard_idx in range(shard_count):
        start = shard_idx * chunk_size
        end = min(len(candidate_indices), start + chunk_size)
        chunk = candidate_indices[start:end]
        merged = []
        seen = set()
        for idx in anchors + chunk:
            if idx in seen:
                continue
            seen.add(idx)
            merged.append(idx)
        if merged:
            shards.append(merged)
    return shards


def aggregate_stage1(shard_rows: list[tuple[int, list[int]]], candidate_indices: list[int]) -> list[int]:
    embed_rank = {idx: pos + 1 for pos, idx in enumerate(candidate_indices)}
    scores: dict[int, float] = {}
    appearances: dict[int, int] = {}
    for shard_id, selected in shard_rows:
        for rank, idx in enumerate(selected, 1):
            scores[idx] = scores.get(idx, 0.0) + (1.0 / (20 + rank))
            appearances[idx] = appearances.get(idx, 0) + 1
    return sorted(
        scores,
        key=lambda idx: (-appearances[idx], -scores[idx], embed_rank[idx]),
    )


def rerank_single(
    *,
    model: Any,
    prompt: str,
    skills: list[Skill],
    candidate_indices: list[int],
    max_return: int,
) -> tuple[list[int], dict[str, Any]]:
    start = time.perf_counter()
    candidate_numbers = rerank_candidates(
        model=model,
        prompt=prompt,
        skills=skills,
        candidate_indices=candidate_indices,
        max_return=max_return,
        phase="single-top100",
    )
    total_sec = time.perf_counter() - start
    reranked = [candidate_indices[num - 1] for num in candidate_numbers if num - 1 < len(candidate_indices)]
    return reranked, {
        "strategy": "single",
        "api_calls": 1,
        "stage1_latency_sec": None,
        "final_latency_sec": total_sec,
        "end_to_end_latency_sec": total_sec,
        "final_pool_size": len(candidate_indices),
    }


def rerank_shard_final(
    *,
    model: Any,
    prompt: str,
    skills: list[Skill],
    candidate_indices: list[int],
    shard_count: int,
    shard_return: int,
    anchor_count: int,
    max_return: int,
) -> tuple[list[int], dict[str, Any]]:
    total_start = time.perf_counter()
    shards = contiguous_shards(candidate_indices, shard_count=shard_count, anchor_count=anchor_count)

    def run_shard(item: tuple[int, list[int]]) -> tuple[int, list[int]]:
        shard_id, shard_candidates = item
        shard_numbers = rerank_candidates(
            model=model,
            prompt=prompt,
            skills=skills,
            candidate_indices=shard_candidates,
            max_return=min(shard_return, len(shard_candidates)),
            phase=f"shard-{shard_id + 1}",
        )
        chosen = [shard_candidates[num - 1] for num in shard_numbers if num - 1 < len(shard_candidates)]
        return shard_id, chosen

    stage1_start = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(shards)) as executor:
        shard_rows = list(executor.map(run_shard, list(enumerate(shards))))
    stage1_sec = time.perf_counter() - stage1_start

    finalists = aggregate_stage1(shard_rows, candidate_indices)
    finalists = finalists[: max_return + shard_count * max(0, anchor_count)]
    final_start = time.perf_counter()
    final_numbers = rerank_candidates(
        model=model,
        prompt=prompt,
        skills=skills,
        candidate_indices=finalists,
        max_return=min(max_return, len(finalists)),
        phase="global-final",
    )
    final_sec = time.perf_counter() - final_start
    reranked = [finalists[num - 1] for num in final_numbers if num - 1 < len(finalists)]
    total_sec = time.perf_counter() - total_start
    return reranked, {
        "strategy": "shard-final",
        "api_calls": len(shards) + 1,
        "stage1_latency_sec": stage1_sec,
        "final_latency_sec": final_sec,
        "end_to_end_latency_sec": total_sec,
        "final_pool_size": len(finalists),
        "shard_count": len(shards),
        "shard_return": shard_return,
        "anchor_count": anchor_count,
    }


def summarize_hits(rows: list[dict[str, Any]], field: str) -> dict[str, Any]:
    total = len(rows)
    cutoffs = (1, 10, 20, 30)
    hits = {k: 0 for k in cutoffs}
    best = []
    for row in rows:
        rank = row[field]
        if rank is not None:
            best.append(rank)
        for cutoff in cutoffs:
            if rank is not None and rank <= cutoff:
                hits[cutoff] += 1
    summary = {f"hit_at_{cutoff}_rate": hits[cutoff] / total for cutoff in cutoffs}
    summary["avg_best_rank_on_hit"] = (sum(best) / len(best)) if best else None
    summary["misses_not_shortlisted"] = total - hits[30]
    return summary


def latency_summary(rows: list[dict[str, Any]]) -> dict[str, Any]:
    end_to_end = [row["end_to_end_latency_sec"] for row in rows if row.get("end_to_end_latency_sec") is not None]
    stage1 = [row["stage1_latency_sec"] for row in rows if row.get("stage1_latency_sec") is not None]
    final = [row["final_latency_sec"] for row in rows if row.get("final_latency_sec") is not None]
    api_calls = [row["api_calls"] for row in rows if row.get("api_calls") is not None]

    def stats(values: list[float]) -> dict[str, Any] | None:
        if not values:
            return None
        ordered = sorted(values)
        return {
            "avg": statistics.fmean(ordered),
            "p50": ordered[len(ordered) // 2],
            "p95": ordered[min(len(ordered) - 1, math.floor(len(ordered) * 0.95))],
        }

    return {
        "api_calls_avg": statistics.fmean(api_calls) if api_calls else None,
        "end_to_end_sec": stats(end_to_end),
        "stage1_sec": stats(stage1),
        "final_sec": stats(final),
    }


def main() -> None:
    args = parse_args()
    model = load_model_config(args.rerank_model)
    skills = load_skills()
    skill_index = {skill.name: idx for idx, skill in enumerate(skills)}
    records = load_latest_records(args.limit)
    skill_emb, prompt_emb = load_embedding_cache(args.embed_model, skills, records)

    rows_path = Path(args.rows_out)
    rows_path.parent.mkdir(parents=True, exist_ok=True)

    baseline_rows: list[dict[str, Any]] = []
    work: list[tuple[Record, list[int], int]] = []
    for record, p_emb in zip(records, prompt_emb):
        sims = skill_emb @ p_emb
        candidate_indices = np.argsort(-sims, kind="stable")[: args.top_k].tolist()
        target_idx = skill_index[record.target_skill]
        embedding_rank = candidate_indices.index(target_idx) + 1 if target_idx in candidate_indices else None
        baseline_rows.append(
            {
                "prompt_id": record.prompt_id,
                "record_id": record.record_id,
                "target_skill": record.target_skill,
                "embedding_rank": embedding_rank,
            }
        )
        work.append((record, candidate_indices, target_idx))

    with rows_path.open("w", encoding="utf-8") as fh:

        def run_one(item: tuple[Record, list[int], int]) -> dict[str, Any]:
            record, candidate_indices, target_idx = item
            try:
                if args.strategy == "single":
                    reranked_indices, meta = rerank_single(
                        model=model,
                        prompt=record.prompt,
                        skills=skills,
                        candidate_indices=candidate_indices,
                        max_return=args.max_return,
                    )
                else:
                    reranked_indices, meta = rerank_shard_final(
                        model=model,
                        prompt=record.prompt,
                        skills=skills,
                        candidate_indices=candidate_indices,
                        shard_count=args.shard_count,
                        shard_return=args.shard_return,
                        anchor_count=args.anchor_count,
                        max_return=args.max_return,
                    )
                rank = reranked_indices.index(target_idx) + 1 if target_idx in reranked_indices else None
                return {
                    "prompt_id": record.prompt_id,
                    "record_id": record.record_id,
                    "target_skill": record.target_skill,
                    "embedding_rank": next(
                        row["embedding_rank"] for row in baseline_rows if row["prompt_id"] == record.prompt_id
                    ),
                    "llm_rank": rank,
                    "llm_candidate_count": len(reranked_indices),
                    "llm_candidate_names": [skills[idx].name for idx in reranked_indices],
                    **meta,
                }
            except Exception as exc:
                return {
                    "prompt_id": record.prompt_id,
                    "record_id": record.record_id,
                    "target_skill": record.target_skill,
                    "embedding_rank": next(
                        row["embedding_rank"] for row in baseline_rows if row["prompt_id"] == record.prompt_id
                    ),
                    "llm_rank": None,
                    "llm_candidate_count": 0,
                    "llm_candidate_names": [],
                    "error": str(exc),
                }

        with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as executor:
            futures = [executor.submit(run_one, item) for item in work]
            for future in concurrent.futures.as_completed(futures):
                row = future.result()
                with lock:
                    fh.write(json.dumps(row, ensure_ascii=False) + "\n")
                    fh.flush()

    rerank_rows = []
    for line in rows_path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        rerank_rows.append(json.loads(line))
    rerank_map = {row["prompt_id"]: row for row in rerank_rows}

    merged_rows = []
    for base in baseline_rows:
        rerank = rerank_map.get(base["prompt_id"], {})
        merged_rows.append(
            {
                "prompt_id": base["prompt_id"],
                "record_id": base["record_id"],
                "target_skill": base["target_skill"],
                "embedding_only": base["embedding_rank"],
                "llm_rerank": rerank.get("llm_rank"),
                "end_to_end_latency_sec": rerank.get("end_to_end_latency_sec"),
                "stage1_latency_sec": rerank.get("stage1_latency_sec"),
                "final_latency_sec": rerank.get("final_latency_sec"),
                "api_calls": rerank.get("api_calls"),
            }
        )

    summary = {
        "rerank_model": args.rerank_model,
        "embed_model": args.embed_model,
        "record_count": len(merged_rows),
        "candidate_top_k": args.top_k,
        "llm_return_top_k": args.max_return,
        "strategy": args.strategy,
        "strategy_params": {
            "shard_count": args.shard_count,
            "shard_return": args.shard_return,
            "anchor_count": args.anchor_count,
        },
        "methods": {
            "embedding_only": summarize_hits(merged_rows, "embedding_only"),
            "llm_rerank": summarize_hits(merged_rows, "llm_rerank"),
        },
        "latency": latency_summary(rerank_rows),
    }
    summary_path = Path(args.summary_out)
    summary_path.parent.mkdir(parents=True, exist_ok=True)
    summary_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
