#!/usr/bin/env python3
from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import re
import threading
import time
import urllib.error
import urllib.request
from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np
import yaml


REPO_ROOT = Path(__file__).resolve().parents[3]
DEFAULT_BASE_DIR = Path(
    os.environ.get("ASTRA_SKILL_SELECTOR_BENCH_BASE", REPO_ROOT / "tmp" / "selector-skill-libraries")
)
DEFAULT_MODELS_YAML_PATH = Path(os.environ.get("ASTRA_MODELS_YAML", REPO_ROOT / ".models.yaml"))

BASE_DIR = DEFAULT_BASE_DIR
SAMPLE_DIR = BASE_DIR / "astra-benchmark-1000"
PRIMARY_PATH = BASE_DIR / "selector-benchmark-dataset" / "primary.jsonl"
RESULTS_DIR = BASE_DIR / "selector-benchmark-dataset" / "benchmark-results"
MODELS_YAML_PATH = DEFAULT_MODELS_YAML_PATH

DEFAULT_RERANK_MODEL = "qwen2.5-3b-instruct"
DEFAULT_EMBED_MODEL = "BAAI/bge-m3"

lock = threading.Lock()


@dataclass
class ModelConfig:
    name: str
    base_url: str
    api_key: str


@dataclass
class Skill:
    name: str
    description: str
    when_to_use: str | None
    aliases: list[str]


@dataclass
class Record:
    record_id: str
    prompt_id: str
    target_skill: str
    prompt: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Rerank embedding top100 candidates with a cheap LLM.")
    parser.add_argument("--base-dir", default=str(DEFAULT_BASE_DIR))
    parser.add_argument("--models-yaml", default=str(DEFAULT_MODELS_YAML_PATH))
    parser.add_argument("--rerank-model", default=DEFAULT_RERANK_MODEL)
    parser.add_argument("--embed-model", default=DEFAULT_EMBED_MODEL)
    parser.add_argument("--limit", type=int, default=None)
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--top-k", type=int, default=100)
    parser.add_argument("--max-return", type=int, default=30)
    parser.add_argument(
        "--rows-out",
        default=None,
    )
    parser.add_argument(
        "--summary-out",
        default=None,
    )
    return parser.parse_args()


def configure_paths(args: argparse.Namespace) -> None:
    global BASE_DIR, SAMPLE_DIR, PRIMARY_PATH, RESULTS_DIR, MODELS_YAML_PATH
    BASE_DIR = Path(args.base_dir)
    SAMPLE_DIR = BASE_DIR / "astra-benchmark-1000"
    PRIMARY_PATH = BASE_DIR / "selector-benchmark-dataset" / "primary.jsonl"
    RESULTS_DIR = BASE_DIR / "selector-benchmark-dataset" / "benchmark-results"
    MODELS_YAML_PATH = Path(args.models_yaml)
    if args.rows_out is None:
        args.rows_out = str(RESULTS_DIR / "llm-top100-rerank-rows.jsonl")
    if args.summary_out is None:
        args.summary_out = str(RESULTS_DIR / "llm-top100-rerank-summary.json")


def load_model_config(model_name: str) -> ModelConfig:
    items = yaml.safe_load(MODELS_YAML_PATH.read_text(encoding="utf-8")) or []
    for item in items:
        if not isinstance(item, dict):
            continue
        if str(item.get("name") or "") != model_name:
            continue
        api_key = str(item.get("api_key") or "").strip()
        base_url = str(item.get("base_url") or "").strip()
        if not api_key or not base_url:
            raise SystemExit(f"model {model_name} missing api_key/base_url in .models.yaml")
        return ModelConfig(name=model_name, base_url=base_url.rstrip("/"), api_key=api_key)
    raise SystemExit(f"model {model_name} not found in .models.yaml")


def extract_json(text: str) -> Any:
    text = text.strip()
    if text.startswith("```"):
        text = re.sub(r"^```(?:json)?", "", text).strip()
        text = re.sub(r"```$", "", text).strip()
    text = re.sub(r"[\x00-\x08\x0b\x0c\x0e-\x1f]", "", text)
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        match = re.search(r"(\{[\s\S]*\}|\[[\s\S]*\])", text)
        if not match:
            raise
        decoder = json.JSONDecoder()
        return decoder.raw_decode(match.group(1))[0]


def format_skill_description(skill: Skill, max_chars: int = 250) -> str:
    desc = skill.description
    if skill.when_to_use:
        desc = f"{desc} (use when: {skill.when_to_use})"
    if skill.aliases:
        desc += f" [aliases: {', '.join(skill.aliases)}]"
    if len(desc) <= max_chars:
        return desc
    end = max_chars - 1
    while end > 0 and not desc.isascii() and not desc.isidentifier():
        end -= 1
    return desc[: max_chars - 1] + "…"


def load_skills() -> list[Skill]:
    skills = []
    for entry in SAMPLE_DIR.iterdir():
        if not entry.is_dir():
            continue
        text = (entry / "SKILL.md").read_text(encoding="utf-8")
        lines = text.splitlines()
        end = next(i for i, line in enumerate(lines[1:], 1) if line.strip() == "---")
        frontmatter = yaml.safe_load("\n".join(lines[1:end])) or {}
        aliases = frontmatter.get("aliases") or []
        if not isinstance(aliases, list):
            aliases = [aliases]
        skills.append(
            Skill(
                name=str(frontmatter["name"]),
                description=str(frontmatter.get("description") or ""),
                when_to_use=(str(frontmatter.get("when_to_use")) if frontmatter.get("when_to_use") else None),
                aliases=[str(alias) for alias in aliases if str(alias).strip()],
            )
        )
    return skills


def load_latest_records(limit: int | None) -> list[Record]:
    latest: OrderedDict[str, dict[str, Any]] = OrderedDict()
    for line in PRIMARY_PATH.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        if row.get("passes", False):
            latest[row["target_skill"]] = row
    records = [
        Record(
            record_id=row["record_id"],
            prompt_id=row["prompt_id"],
            target_skill=row["target_skill"],
            prompt=row["prompt"],
        )
        for row in latest.values()
    ]
    if limit is not None:
        records = records[:limit]
    return records


def load_embedding_cache(embed_model: str, skills: list[Skill], records: list[Record]) -> tuple[np.ndarray, np.ndarray]:
    safe = embed_model.replace("/", "__")
    cache_dir = RESULTS_DIR / "embedding-cache"
    skill_emb = np.load(cache_dir / f"skills-{safe}.npy")
    prompt_emb = np.load(cache_dir / f"prompts-{safe}.npy")
    skill_meta = json.loads((cache_dir / f"skills-{safe}.json").read_text(encoding="utf-8"))
    prompt_meta = json.loads((cache_dir / f"prompts-{safe}.json").read_text(encoding="utf-8"))

    if skill_meta["ids"] != [skill.name for skill in skills]:
        idx_map = {name: idx for idx, name in enumerate(skill_meta["ids"])}
        skill_emb = skill_emb[[idx_map[skill.name] for skill in skills]]
    prompt_idx = {pid: idx for idx, pid in enumerate(prompt_meta["ids"])}
    prompt_emb = prompt_emb[[prompt_idx[record.prompt_id] for record in records]]
    return skill_emb, prompt_emb


def candidate_text(skills: list[Skill], candidate_indices: list[int]) -> str:
    lines = []
    for rank, idx in enumerate(candidate_indices, 1):
        skill = skills[idx]
        desc = format_skill_description(skill)
        lines.append(f"{rank}. {skill.name}: {desc}")
    return "\n".join(lines)


def chat_completion(config: ModelConfig, system_prompt: str, user_prompt: str) -> str:
    payload = {
        "model": config.name,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_prompt},
        ],
        "temperature": 0,
        "max_tokens": 256,
    }
    req = urllib.request.Request(
        f"{config.base_url}/chat/completions",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {config.api_key}",
        },
        method="POST",
    )
    last_error = None
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                data = json.load(resp)
                return data["choices"][0]["message"]["content"]
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError, KeyError, json.JSONDecodeError) as exc:
            last_error = exc
            time.sleep(2 + attempt * 2)
    raise RuntimeError(f"chat completion failed: {last_error}")


def rerank_record(config: ModelConfig, prompt: str, candidates_text: str, max_return: int) -> list[int]:
    system_prompt = (
        "You are a cheap skill selector reranker. "
        "Given a user request and 100 candidate skills, return ONLY JSON. "
        "Do not explain. Do not invent candidates. "
        "Rank the most likely matching candidates best-first. "
        "Bias toward recall: include plausible candidates instead of being overly conservative."
    )
    user_prompt = (
        f"User request:\n{prompt}\n\n"
        f"Candidate skills:\n{candidates_text}\n\n"
        f"Return JSON in this exact shape:\n"
        f'{{"ranked_candidate_numbers":[1,2,3]}}\n'
        f"Rules:\n"
        f"- Use only candidate numbers from the list above.\n"
        f"- Return exactly {max_return} unique integers.\n"
        f"- Sort best-first.\n"
        f"- If uncertain, still fill all {max_return} slots with your best broad-recall ranking.\n"
        f"- Prefer recall over precision for positions 10-30.\n"
    )
    last_raw = ""
    for _ in range(3):
        raw = chat_completion(config, system_prompt, user_prompt)
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
            if num < 1 or num > 100 or num in seen:
                continue
            seen.add(num)
            out.append(num)
            if len(out) >= max_return:
                break
        if out:
            return out
    raise RuntimeError(f"failed to parse reranker output: {last_raw[:400]}")


def summarize(rows: list[dict[str, Any]], field: str) -> dict[str, Any]:
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


def main() -> None:
    args = parse_args()
    configure_paths(args)
    model = load_model_config(args.rerank_model)
    skills = load_skills()
    skill_index = {skill.name: idx for idx, skill in enumerate(skills)}
    records = load_latest_records(args.limit)
    skill_emb, prompt_emb = load_embedding_cache(args.embed_model, skills, records)

    rows_path = Path(args.rows_out)
    rows_path.parent.mkdir(parents=True, exist_ok=True)
    existing: dict[str, dict[str, Any]] = {}
    if rows_path.exists():
        for line in rows_path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            row = json.loads(line)
            existing[row["prompt_id"]] = row

    work: list[tuple[Record, list[int], int]] = []
    baseline_rows: list[dict[str, Any]] = []
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
        if record.prompt_id in existing:
            continue
        work.append((record, candidate_indices, target_idx))

    if work:
        with rows_path.open("a", encoding="utf-8") as fh:
            def run_one(item: tuple[Record, list[int], int]) -> dict[str, Any]:
                record, candidate_indices, target_idx = item
                try:
                    reranked_numbers = rerank_record(
                        model,
                        record.prompt,
                        candidate_text(skills, candidate_indices),
                        args.max_return,
                    )
                    reranked_indices = [candidate_indices[num - 1] for num in reranked_numbers if num - 1 < len(candidate_indices)]
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

    rerank_rows = {}
    for line in rows_path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        rerank_rows[row["prompt_id"]] = row

    merged_rows = []
    for base in baseline_rows:
        rerank = rerank_rows.get(base["prompt_id"], {})
        merged_rows.append(
            {
                "prompt_id": base["prompt_id"],
                "record_id": base["record_id"],
                "target_skill": base["target_skill"],
                "embedding_only": base["embedding_rank"],
                "llm_rerank": rerank.get("llm_rank"),
            }
        )

    summary = {
        "rerank_model": args.rerank_model,
        "embed_model": args.embed_model,
        "record_count": len(merged_rows),
        "candidate_top_k": args.top_k,
        "llm_return_top_k": args.max_return,
        "methods": {
            "embedding_only": summarize(merged_rows, "embedding_only"),
            "llm_rerank": summarize(merged_rows, "llm_rerank"),
        },
    }
    summary_path = Path(args.summary_out)
    summary_path.parent.mkdir(parents=True, exist_ok=True)
    summary_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
