#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import math
import os
import time
import urllib.error
import urllib.request
from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np
import yaml


BASE_DIR = Path("/Users/ghs-mo/MOWorkSpace/mo-agent-engine-selector-metrics/tmp/selector-skill-libraries")
SAMPLE_DIR = BASE_DIR / "astra-benchmark-1000"
PRIMARY_PATH = BASE_DIR / "selector-benchmark-dataset" / "primary.jsonl"
RESULTS_DIR = BASE_DIR / "selector-benchmark-dataset" / "benchmark-results"
CACHE_DIR = RESULTS_DIR / "embedding-cache"

DEFAULT_MODEL = "BAAI/bge-m3"
RRF_K = 60.0
RRF_WEIGHTS = (0.5, 1.0, 2.0, 4.0)
GUARDED_RRF_WEIGHTS = (2.0, 4.0)


@dataclass
class Skill:
    name: str
    description: str
    when_to_use: str | None
    category: str | None
    tags: list[str]
    triggers: list[str]
    aliases: list[str]
    intent_doc: str
    keyword_doc: str
    selector_doc: str
    full_text: str


@dataclass
class Record:
    record_id: str
    prompt_id: str
    target_skill: str
    prompt: str
    difficulty: str | None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Prototype embedding+lexical hybrid selector on the benchmark corpus.")
    parser.add_argument("--api-base", default=os.environ.get("EMBEDDING_API_BASE", "https://api.siliconflow.cn/v1"))
    parser.add_argument("--api-key", default=os.environ.get("EMBEDDING_API_KEY"))
    parser.add_argument("--model", default=os.environ.get("EMBEDDING_MODEL", DEFAULT_MODEL))
    parser.add_argument("--batch-size", type=int, default=32)
    parser.add_argument("--limit", type=int, default=None)
    parser.add_argument("--top-k", type=int, default=14)
    parser.add_argument("--no-cache", action="store_true")
    parser.add_argument(
        "--summary-out",
        default=str(RESULTS_DIR / "hybrid-selector-prototype-summary.json"),
    )
    parser.add_argument(
        "--rows-out",
        default=str(RESULTS_DIR / "hybrid-selector-prototype-rows.jsonl"),
    )
    return parser.parse_args()


def split_alnum(text: str) -> list[str]:
    parts: list[str] = []
    buf: list[str] = []
    for ch in text.lower():
        if ch.isalnum():
            buf.append(ch)
        elif buf:
            token = "".join(buf)
            if len(token) > 1:
                parts.append(token)
            buf.clear()
    if buf:
        token = "".join(buf)
        if len(token) > 1:
            parts.append(token)
    return parts


def haystack_for_scoring(skill: Skill) -> str:
    bits = [skill.name, skill.description]
    if skill.when_to_use:
        bits.append(skill.when_to_use)
    if skill.category:
        bits.append(skill.category)
    bits.extend(skill.tags)
    bits.extend(skill.triggers)
    bits.extend(skill.aliases)
    return " ".join(bits).lower()


def lexical_score(skill: Skill, query: str, query_tokens: list[str]) -> float:
    score = 0.0
    hay = haystack_for_scoring(skill)
    name_l = skill.name.lower()
    query_lower = query.lower().strip()

    if query_lower:
        if name_l == query_lower:
            score += 12.0
        elif query_lower in hay:
            score += 6.0
        if query_lower.find(name_l) >= 0 or name_l.find(query_lower) >= 0:
            score += 4.0

    aliases_lower = [alias.lower() for alias in skill.aliases]
    triggers_lower = [trigger.lower() for trigger in skill.triggers]
    for token in query_tokens:
        if name_l == token or token in aliases_lower:
            score += 5.0
        elif any(token in trigger for trigger in triggers_lower):
            score += 4.0
        elif token in hay:
            score += 1.5

    return score


def current_selector_shortlist(skills: list[Skill], query: str, surface_cap: int = 14) -> tuple[list[int], np.ndarray]:
    tokens = split_alnum(query)
    scores = np.array([lexical_score(skill, query, tokens) for skill in skills], dtype=np.float32)
    ranked = np.argsort(-scores, kind="stable")
    top_score = float(scores[ranked[0]]) if len(ranked) else 0.0
    weak = top_score < 0.8

    picked: list[int] = []
    if not weak:
        for idx in ranked:
            if len(picked) >= surface_cap:
                break
            if float(scores[idx]) >= 0.8:
                picked.append(int(idx))

    if weak or len(picked) < 3:
        picked = list(range(min(surface_cap, len(skills))))

    return picked, scores


def build_selector_doc(frontmatter: dict[str, Any], body: str) -> str:
    fields = [
        f"name: {frontmatter.get('name', '')}",
        f"description: {frontmatter.get('description', '')}",
        f"when_to_use: {frontmatter.get('when_to_use', '')}",
        f"category: {frontmatter.get('category', '')}",
    ]
    for key in ("aliases", "tags", "triggers"):
        value = frontmatter.get(key) or []
        if not isinstance(value, list):
            value = [value]
        if value:
            fields.append(f"{key}: {', '.join(str(v) for v in value if str(v).strip())}")
    body = body.strip()
    if body:
        summary_lines = [line.strip() for line in body.splitlines() if line.strip()][:8]
        if summary_lines:
            fields.append("instructions: " + " ".join(summary_lines))
    return "\n".join(fields).strip()


def build_intent_doc(frontmatter: dict[str, Any]) -> str:
    fields = [
        f"name: {frontmatter.get('name', '')}",
        f"description: {frontmatter.get('description', '')}",
        f"when_to_use: {frontmatter.get('when_to_use', '')}",
        f"category: {frontmatter.get('category', '')}",
    ]
    return "\n".join(x for x in fields if x and not x.endswith(": ")).strip()


def build_keyword_doc(frontmatter: dict[str, Any]) -> str:
    aliases = frontmatter.get("aliases") or []
    if not isinstance(aliases, list):
        aliases = [aliases]
    tags = frontmatter.get("tags") or []
    if not isinstance(tags, list):
        tags = [tags]
    triggers = frontmatter.get("triggers") or []
    if not isinstance(triggers, list):
        triggers = [triggers]
    fields = [
        f"name: {frontmatter.get('name', '')}",
        f"aliases: {', '.join(str(a) for a in aliases if str(a).strip())}",
        f"tags: {', '.join(str(t) for t in tags if str(t).strip())}",
        f"triggers: {', '.join(str(t) for t in triggers if str(t).strip())}",
    ]
    return "\n".join(x for x in fields if x and not x.endswith(": ")).strip()


def load_skill(path: Path) -> Skill:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    end = next(i for i, line in enumerate(lines[1:], 1) if line.strip() == "---")
    frontmatter = yaml.safe_load("\n".join(lines[1:end])) or {}
    body = "\n".join(lines[end + 1 :]).strip()
    aliases = frontmatter.get("aliases") or []
    if not isinstance(aliases, list):
        aliases = [aliases]
    tags = frontmatter.get("tags") or []
    if not isinstance(tags, list):
        tags = [tags]
    triggers = frontmatter.get("triggers") or []
    if not isinstance(triggers, list):
        triggers = [triggers]

    return Skill(
        name=str(frontmatter["name"]),
        description=str(frontmatter.get("description") or ""),
        when_to_use=(str(frontmatter.get("when_to_use")) if frontmatter.get("when_to_use") else None),
        category=(str(frontmatter.get("category")) if frontmatter.get("category") else None),
        tags=[str(tag) for tag in tags if str(tag).strip()],
        triggers=[str(trigger) for trigger in triggers if str(trigger).strip()],
        aliases=[str(alias) for alias in aliases if str(alias).strip()],
        intent_doc=build_intent_doc(frontmatter),
        keyword_doc=build_keyword_doc(frontmatter),
        selector_doc=build_selector_doc(frontmatter, body),
        full_text=text.strip(),
    )


def load_catalog() -> list[Skill]:
    skills = []
    for entry in SAMPLE_DIR.iterdir():
        if not entry.is_dir():
            continue
        skill_md = entry / "SKILL.md"
        if skill_md.is_file():
            skills.append(load_skill(skill_md))
    return skills


def chunk_text(text: str, max_chars: int = 2000) -> list[str]:
    lines = [line.rstrip() for line in text.splitlines()]
    chunks: list[str] = []
    cur: list[str] = []
    cur_len = 0
    for line in lines:
        extra = len(line) + 1
        if cur and cur_len + extra > max_chars:
            chunks.append("\n".join(cur).strip())
            cur = []
            cur_len = 0
        if len(line) > max_chars:
            for start in range(0, len(line), max_chars):
                part = line[start : start + max_chars].strip()
                if part:
                    chunks.append(part)
            continue
        cur.append(line)
        cur_len += extra
    if cur:
        chunks.append("\n".join(cur).strip())
    return [chunk for chunk in chunks if chunk]


def load_latest_primary(limit: int | None = None) -> list[Record]:
    latest: OrderedDict[str, dict[str, Any]] = OrderedDict()
    for line in PRIMARY_PATH.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        latest[row["target_skill"]] = row
    records = [
        Record(
            record_id=row["record_id"],
            prompt_id=row["prompt_id"],
            target_skill=row["target_skill"],
            prompt=row["prompt"],
            difficulty=row.get("difficulty"),
        )
        for row in latest.values()
        if row.get("passes", False)
    ]
    if limit is not None:
        records = records[:limit]
    return records


def embed_batch(api_base: str, api_key: str, model: str, texts: list[str]) -> np.ndarray:
    payload = json.dumps({"model": model, "input": texts}).encode("utf-8")
    req = urllib.request.Request(
        api_base.rstrip("/") + "/embeddings",
        data=payload,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {api_key}",
        },
        method="POST",
    )
    last_error: Exception | None = None
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                data = json.load(resp)
                rows = data.get("data", [])
                rows.sort(key=lambda item: item.get("index", 0))
                return np.asarray([row["embedding"] for row in rows], dtype=np.float32)
        except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, KeyError, ValueError) as exc:
            last_error = exc
            time.sleep(2 + attempt * 2)
    raise RuntimeError(f"embedding request failed: {last_error}")


def cache_paths(kind: str, model: str) -> tuple[Path, Path]:
    safe_model = model.replace("/", "__")
    return (
        CACHE_DIR / f"{kind}-{safe_model}.npy",
        CACHE_DIR / f"{kind}-{safe_model}.json",
    )


def load_or_embed(
    kind: str,
    model: str,
    texts: list[str],
    ids: list[str],
    api_base: str,
    api_key: str,
    batch_size: int,
    use_cache: bool,
) -> np.ndarray:
    vec_path, meta_path = cache_paths(kind, model)
    if use_cache and vec_path.exists() and meta_path.exists():
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
        if meta.get("ids") == ids and meta.get("model") == model:
            return np.load(vec_path)

    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    chunks = []
    for start in range(0, len(texts), batch_size):
        batch = texts[start : start + batch_size]
        chunks.append(embed_batch(api_base, api_key, model, batch))
    matrix = np.vstack(chunks) if chunks else np.zeros((0, 0), dtype=np.float32)
    if use_cache:
        np.save(vec_path, matrix)
        meta_path.write_text(json.dumps({"ids": ids, "model": model}, ensure_ascii=False, indent=2), encoding="utf-8")
    return matrix


def l2_normalize(matrix: np.ndarray) -> np.ndarray:
    norms = np.linalg.norm(matrix, axis=1, keepdims=True)
    norms[norms == 0] = 1.0
    return matrix / norms


def rank_to_positions(order: np.ndarray) -> np.ndarray:
    positions = np.empty_like(order)
    positions[order] = np.arange(1, len(order) + 1, dtype=order.dtype)
    return positions


def best_rank(target_idx: int, picked: list[int]) -> int | None:
    try:
        return picked.index(target_idx) + 1
    except ValueError:
        return None


def metric_from_rank(rank: int | None, top_k: int) -> dict[str, Any]:
    metric = {
        "best_rank": rank,
        "hit_at_1": rank is not None and rank <= 1,
        "hit_at_3": rank is not None and rank <= 3,
        "hit_at_5": rank is not None and rank <= 5,
        "hit_at_14": rank is not None and rank <= 14,
    }
    for cutoff in (10, 30, top_k):
        metric[f"hit_at_{cutoff}"] = rank is not None and rank <= cutoff
    return metric


def summarize(rows: list[dict[str, Any]], field: str, top_k: int) -> dict[str, Any]:
    total = len(rows)
    ranks = [row[field]["best_rank"] for row in rows if row[field]["best_rank"] is not None]
    summary = {
        "evaluated_records": total,
        "hit_at_1_rate": sum(1 for row in rows if row[field]["hit_at_1"]) / total,
        "hit_at_3_rate": sum(1 for row in rows if row[field]["hit_at_3"]) / total,
        "hit_at_5_rate": sum(1 for row in rows if row[field]["hit_at_5"]) / total,
        "hit_at_14_rate": sum(1 for row in rows if row[field]["hit_at_14"]) / total,
        "avg_best_rank_on_hit": (sum(ranks) / len(ranks)) if ranks else None,
        "misses_not_shortlisted": sum(1 for row in rows if row[field]["best_rank"] is None),
    }
    for cutoff in (10, 30, top_k):
        summary[f"hit_at_{cutoff}_rate"] = sum(1 for row in rows if row[field][f"hit_at_{cutoff}"]) / total
    return summary


def tail_mmr_select(
    order: np.ndarray,
    relevance: np.ndarray,
    diversity_vectors: np.ndarray,
    top_k: int,
    prefix_keep: int = 5,
    mmr_lambda: float = 0.9,
    candidate_limit: int = 200,
) -> list[int]:
    prefix = [int(i) for i in order[: min(prefix_keep, top_k)]]
    if len(prefix) >= top_k:
        return prefix[:top_k]

    candidate_pool = [int(i) for i in order[:candidate_limit]]
    selected = prefix[:]
    selected_set = set(selected)
    remaining = [idx for idx in candidate_pool if idx not in selected_set]
    while len(selected) < top_k and remaining:
        best_idx = None
        best_score = None
        for cand in remaining:
            max_sim = max(float(diversity_vectors[cand] @ diversity_vectors[sel]) for sel in selected) if selected else 0.0
            score = mmr_lambda * float(relevance[cand]) - (1.0 - mmr_lambda) * max_sim
            if best_score is None or score > best_score:
                best_idx = cand
                best_score = score
        selected.append(best_idx)
        selected_set.add(best_idx)
        remaining.remove(best_idx)

    if len(selected) < top_k:
        for idx in order:
            idx = int(idx)
            if idx not in selected_set:
                selected.append(idx)
                selected_set.add(idx)
                if len(selected) >= top_k:
                    break
    return selected[:top_k]


def main() -> None:
    args = parse_args()
    if not args.api_key:
        raise SystemExit("missing --api-key / EMBEDDING_API_KEY")

    skills = load_catalog()
    records = load_latest_primary(args.limit)
    skill_index = {skill.name: idx for idx, skill in enumerate(skills)}

    skill_texts = [skill.selector_doc for skill in skills]
    skill_intent_texts = [skill.intent_doc for skill in skills]
    skill_keyword_texts = [skill.keyword_doc for skill in skills]
    prompt_texts = [record.prompt for record in records]
    fulltext_chunk_texts: list[str] = []
    fulltext_chunk_ids: list[str] = []
    fulltext_chunk_skill_indices: list[int] = []
    for idx, skill in enumerate(skills):
        for chunk_idx, chunk in enumerate(chunk_text(skill.full_text)):
            fulltext_chunk_texts.append(chunk)
            fulltext_chunk_ids.append(f"{skill.name}#{chunk_idx}")
            fulltext_chunk_skill_indices.append(idx)

    skill_emb = load_or_embed(
        "skills",
        args.model,
        skill_texts,
        [skill.name for skill in skills],
        args.api_base,
        args.api_key,
        args.batch_size,
        not args.no_cache,
    )
    skill_intent_emb = load_or_embed(
        "skills-intent",
        args.model,
        skill_intent_texts,
        [skill.name for skill in skills],
        args.api_base,
        args.api_key,
        args.batch_size,
        not args.no_cache,
    )
    skill_keyword_emb = load_or_embed(
        "skills-keyword",
        args.model,
        skill_keyword_texts,
        [skill.name for skill in skills],
        args.api_base,
        args.api_key,
        args.batch_size,
        not args.no_cache,
    )
    skill_full_emb = load_or_embed(
        "skills-fulltext-chunks",
        args.model,
        fulltext_chunk_texts,
        fulltext_chunk_ids,
        args.api_base,
        args.api_key,
        args.batch_size,
        not args.no_cache,
    )
    prompt_emb = load_or_embed(
        "prompts",
        args.model,
        prompt_texts,
        [record.prompt_id for record in records],
        args.api_base,
        args.api_key,
        args.batch_size,
        not args.no_cache,
    )

    skill_emb = l2_normalize(skill_emb)
    skill_intent_emb = l2_normalize(skill_intent_emb)
    skill_keyword_emb = l2_normalize(skill_keyword_emb)
    skill_full_emb = l2_normalize(skill_full_emb)
    prompt_emb = l2_normalize(prompt_emb)
    fulltext_chunk_skill_indices_np = np.asarray(fulltext_chunk_skill_indices, dtype=np.int32)

    rows: list[dict[str, Any]] = []
    for record, p_emb in zip(records, prompt_emb):
        target_idx = skill_index[record.target_skill]
        current_picked, lex_scores = current_selector_shortlist(skills, record.prompt, surface_cap=args.top_k)
        max_lex_score = float(np.max(lex_scores)) if len(lex_scores) else 0.0
        lex_order = np.argsort(-lex_scores, kind="stable")
        emb_scores = skill_emb @ p_emb
        intent_scores = skill_intent_emb @ p_emb
        keyword_scores = skill_keyword_emb @ p_emb
        fulltext_chunk_scores = skill_full_emb @ p_emb
        fulltext_scores = np.full(len(skills), -np.inf, dtype=np.float32)
        np.maximum.at(fulltext_scores, fulltext_chunk_skill_indices_np, fulltext_chunk_scores)
        multivector_scores = np.maximum.reduce([emb_scores, intent_scores, keyword_scores])
        selector_plus_fulltext_scores = np.maximum(emb_scores, fulltext_scores)
        emb_order = np.argsort(-emb_scores, kind="stable")
        multivector_order = np.argsort(-multivector_scores, kind="stable")
        fulltext_order = np.argsort(-fulltext_scores, kind="stable")
        selector_plus_fulltext_order = np.argsort(-selector_plus_fulltext_scores, kind="stable")

        row: dict[str, Any] = {
            "record_id": record.record_id,
            "prompt_id": record.prompt_id,
            "target_skill": record.target_skill,
            "current": metric_from_rank(best_rank(target_idx, current_picked), args.top_k),
            "embedding_only": metric_from_rank(best_rank(target_idx, emb_order[: args.top_k].tolist()), args.top_k),
            "fulltext_only": metric_from_rank(best_rank(target_idx, fulltext_order[: args.top_k].tolist()), args.top_k),
            "selector_plus_fulltext": metric_from_rank(
                best_rank(target_idx, selector_plus_fulltext_order[: args.top_k].tolist()),
                args.top_k,
            ),
            "multivector_only": metric_from_rank(best_rank(target_idx, multivector_order[: args.top_k].tolist()), args.top_k),
            "current_top_skill": skills[current_picked[0]].name if current_picked else None,
            "embedding_top_skill": skills[int(emb_order[0])].name,
            "fulltext_top_skill": skills[int(fulltext_order[0])].name,
            "selector_plus_fulltext_top_skill": skills[int(selector_plus_fulltext_order[0])].name,
            "multivector_top_skill": skills[int(multivector_order[0])].name,
        }

        multivector_mmr_085 = tail_mmr_select(multivector_order, multivector_scores, skill_emb, args.top_k, mmr_lambda=0.85)
        multivector_mmr_090 = tail_mmr_select(multivector_order, multivector_scores, skill_emb, args.top_k, mmr_lambda=0.90)
        row["multivector_tail_mmr_085"] = metric_from_rank(best_rank(target_idx, multivector_mmr_085), args.top_k)
        row["multivector_tail_mmr_090"] = metric_from_rank(best_rank(target_idx, multivector_mmr_090), args.top_k)
        row["multivector_tail_mmr_085_top_skill"] = skills[int(multivector_mmr_085[0])].name
        row["multivector_tail_mmr_090_top_skill"] = skills[int(multivector_mmr_090[0])].name

        lex_positions = rank_to_positions(lex_order)
        emb_positions = rank_to_positions(emb_order)
        for weight in RRF_WEIGHTS:
            rrf_scores = 1.0 / (RRF_K + lex_positions.astype(np.float32)) + weight / (
                RRF_K + emb_positions.astype(np.float32)
            )
            hybrid_order = np.argsort(-rrf_scores, kind="stable")
            field = f"hybrid_rrf_w{str(weight).replace('.', '_')}"
            row[field] = metric_from_rank(best_rank(target_idx, hybrid_order[: args.top_k].tolist()), args.top_k)
            row[f"{field}_top_skill"] = skills[int(hybrid_order[0])].name
            if weight in GUARDED_RRF_WEIGHTS:
                guarded_field = f"guarded_{field}"
                guarded_order = emb_order if max_lex_score < 0.8 else hybrid_order
                row[guarded_field] = metric_from_rank(best_rank(target_idx, guarded_order[: args.top_k].tolist()), args.top_k)
                row[f"{guarded_field}_top_skill"] = skills[int(guarded_order[0])].name
        rows.append(row)

    summary = {
        "model": args.model,
        "api_base": args.api_base,
        "catalog_size": len(skills),
        "record_count": len(rows),
        "top_k": args.top_k,
        "methods": {
            "current": summarize(rows, "current", args.top_k),
            "embedding_only": summarize(rows, "embedding_only", args.top_k),
            "fulltext_only": summarize(rows, "fulltext_only", args.top_k),
            "selector_plus_fulltext": summarize(rows, "selector_plus_fulltext", args.top_k),
            "multivector_only": summarize(rows, "multivector_only", args.top_k),
            "multivector_tail_mmr_085": summarize(rows, "multivector_tail_mmr_085", args.top_k),
            "multivector_tail_mmr_090": summarize(rows, "multivector_tail_mmr_090", args.top_k),
        },
    }
    for weight in RRF_WEIGHTS:
        field = f"hybrid_rrf_w{str(weight).replace('.', '_')}"
        summary["methods"][field] = summarize(rows, field, args.top_k)
        if weight in GUARDED_RRF_WEIGHTS:
            guarded_field = f"guarded_{field}"
            summary["methods"][guarded_field] = summarize(rows, guarded_field, args.top_k)

    results_path = Path(args.rows_out)
    results_path.parent.mkdir(parents=True, exist_ok=True)
    with results_path.open("w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row, ensure_ascii=False) + "\n")

    summary_path = Path(args.summary_out)
    summary_path.parent.mkdir(parents=True, exist_ok=True)
    summary_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
