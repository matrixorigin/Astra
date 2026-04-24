#!/usr/bin/env python3
from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import random
import re
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml


BASE_DIR = Path("/Users/ghs-mo/MOWorkSpace/mo-agent-engine-selector-metrics/tmp/selector-skill-libraries")
SAMPLE_DIR = BASE_DIR / "astra-benchmark-1000"
DATASET_DIR = BASE_DIR / "selector-benchmark-dataset"
CATALOG_PATH = DATASET_DIR / "catalog.jsonl"
PRIMARY_PATH = DATASET_DIR / "primary.jsonl"
HARD_PATH = DATASET_DIR / "hard.jsonl"
NOSKILL_PATH = DATASET_DIR / "no_skill.jsonl"
ERRORS_PATH = DATASET_DIR / "errors.jsonl"
PAIRS_PATH = DATASET_DIR / "hard_pairs.jsonl"
MANIFEST_PATH = DATASET_DIR / "manifest.json"
MODELS_YAML_PATH = Path("/Users/ghs-mo/MOWorkSpace/mo-agent-engine/.models.yaml")

DEFAULT_GEN_MODELS = ["glm-5.1", "qwen-plus", "qwen-max", "qwen3.6-plus"]
DEFAULT_REWRITE_MODELS = ["MiniMax-M2.5", "qwen-plus", "qwen-max"]
DEFAULT_JUDGE_MODELS = ["MiniMax-M2.5", "qwen-plus", "qwen-max"]


STOPWORDS = {
    "the",
    "and",
    "for",
    "with",
    "that",
    "this",
    "from",
    "when",
    "into",
    "your",
    "user",
    "using",
    "create",
    "build",
    "make",
    "use",
    "skill",
    "agent",
    "code",
    "file",
    "files",
    "data",
    "task",
}


@dataclass
class ModelConfig:
    model: str
    base_url: str
    api_key: str


@dataclass
class Skill:
    name: str
    description: str
    body: str
    aliases: list[str]
    category: str | None
    tags: list[str]
    path: str


def get_model_config(prefix: str, fallback_prefix: str | None = None) -> ModelConfig:
    model = os.environ.get(f"{prefix}_MODEL")
    base_url = os.environ.get(f"{prefix}_BASE_URL")
    api_key = os.environ.get(f"{prefix}_API_KEY")
    if not (model and base_url and api_key) and fallback_prefix:
        model = model or os.environ.get(f"{fallback_prefix}_MODEL")
        base_url = base_url or os.environ.get(f"{fallback_prefix}_BASE_URL")
        api_key = api_key or os.environ.get(f"{fallback_prefix}_API_KEY")
    if not (model and base_url and api_key):
        raise SystemExit(f"missing model env for {prefix}")
    return ModelConfig(model=model, base_url=base_url.rstrip("/"), api_key=api_key)


def load_model_registry(path: Path) -> dict[str, ModelConfig]:
    items = yaml.safe_load(path.read_text(encoding="utf-8")) or []
    registry: dict[str, ModelConfig] = {}
    for item in items:
        if not isinstance(item, dict):
            continue
        name = str(item.get("name") or "").strip()
        if not name:
            continue
        provider = str(item.get("provider") or "").strip().lower()
        if provider != "openai":
            continue
        api_key = str(item.get("api_key") or "").strip()
        base_url = str(item.get("base_url") or "").strip()
        if not api_key or not base_url:
            continue
        registry[name] = ModelConfig(model=name, base_url=base_url.rstrip("/"), api_key=api_key)
    return registry


def parse_model_names(raw: str | None) -> list[str]:
    if not raw:
        return []
    return [name.strip() for name in raw.split(",") if name.strip()]


def get_model_pool(
    prefix: str,
    defaults: list[str],
    registry: dict[str, ModelConfig],
    fallback_prefix: str | None = None,
) -> list[ModelConfig]:
    explicit = parse_model_names(os.environ.get(f"{prefix}_MODELS"))
    if explicit:
        return [registry[name] for name in explicit if name in registry]

    single_model = os.environ.get(f"{prefix}_MODEL")
    if single_model and os.environ.get(f"{prefix}_BASE_URL") and os.environ.get(f"{prefix}_API_KEY"):
        return [get_model_config(prefix, fallback_prefix)]

    if fallback_prefix:
        fallback_explicit = parse_model_names(os.environ.get(f"{fallback_prefix}_MODELS"))
        if fallback_explicit:
            return [registry[name] for name in fallback_explicit if name in registry]

    pool = [registry[name] for name in defaults if name in registry]
    if not pool:
        raise SystemExit(f"no model pool available for {prefix}")
    return pool


def stable_pool_pick(pool: list[ModelConfig], record_key: str, stage: str, attempt: int) -> ModelConfig:
    digest = hashlib.sha256(f"{record_key}:{stage}:{attempt}".encode("utf-8")).hexdigest()
    idx = int(digest[:8], 16) % len(pool)
    return pool[idx]


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


def ensure_object(data: Any, label: str) -> dict[str, Any]:
    if isinstance(data, dict):
        return data
    if isinstance(data, list):
        for item in data:
            if isinstance(item, dict):
                return item
    raise RuntimeError(f"{label} returned non-object JSON: {type(data).__name__}")


def chat_completion(config: ModelConfig, messages: list[dict[str, str]], temperature: float = 0.7) -> str:
    payload = {
        "model": config.model,
        "messages": messages,
        "temperature": temperature,
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
            with urllib.request.urlopen(req, timeout=90) as resp:
                data = json.loads(resp.read().decode("utf-8"))
                return data["choices"][0]["message"]["content"]
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError, KeyError, json.JSONDecodeError) as exc:
            last_error = exc
            time.sleep(2 + attempt * 2)
    raise RuntimeError(f"chat completion failed: {last_error}")


def tokenize(text: str) -> set[str]:
    return {
        tok
        for tok in re.findall(r"[a-zA-Z0-9]{3,}", text.lower())
        if tok not in STOPWORDS
    }


def load_skill(path: Path) -> Skill:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    end = next(i for i, line in enumerate(lines[1:], 1) if line.strip() == "---")
    frontmatter = yaml.safe_load("\n".join(lines[1:end])) or {}
    body = "\n".join(lines[end + 1 :]).strip()
    aliases = frontmatter.get("aliases") or []
    if not isinstance(aliases, list):
        aliases = [str(aliases)]
    tags = frontmatter.get("tags") or []
    if not isinstance(tags, list):
        tags = [str(tags)]
    return Skill(
        name=str(frontmatter["name"]),
        description=str(frontmatter.get("description") or ""),
        body=body,
        aliases=[str(alias) for alias in aliases if str(alias).strip()],
        category=frontmatter.get("category"),
        tags=[str(tag) for tag in tags if str(tag).strip()],
        path=str(path),
    )


def load_catalog() -> list[Skill]:
    skills = []
    for entry in sorted(SAMPLE_DIR.iterdir()):
        skill_md = entry / "SKILL.md"
        if skill_md.is_file():
            skills.append(load_skill(skill_md))
    DATASET_DIR.mkdir(parents=True, exist_ok=True)
    with CATALOG_PATH.open("w", encoding="utf-8") as fh:
        for skill in skills:
            fh.write(
                json.dumps(
                    {
                        "name": skill.name,
                        "description": skill.description,
                        "aliases": skill.aliases,
                        "category": skill.category,
                        "tags": skill.tags,
                        "path": skill.path,
                    },
                    ensure_ascii=False,
                )
                + "\n"
            )
    return skills


def read_existing(path: Path, require_pass: bool = False) -> set[str]:
    if not path.exists():
        return set()
    seen = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if require_pass and item.get("passes") is not True:
            continue
        record_id = item.get("record_id") or item.get("name") or item.get("pair_id") or item.get("prompt_id")
        if record_id:
            seen.add(str(record_id))
    return seen


def append_jsonl(path: Path, item: dict[str, Any]) -> None:
    with path.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(item, ensure_ascii=False) + "\n")


def banned_terms(skill: Skill) -> list[str]:
    terms = {skill.name.lower()}
    terms.update(alias.lower() for alias in skill.aliases)
    return sorted(term for term in terms if term)


def generate_primary_record(
    skill: Skill,
    gen_pool: list[ModelConfig],
    rewrite_pool: list[ModelConfig],
    judge_pool: list[ModelConfig],
    attempt: int = 0,
) -> dict[str, Any]:
    gen_cfg = stable_pool_pick(gen_pool, skill.name, "generate", attempt)
    rewrite_cfg = stable_pool_pick(rewrite_pool, skill.name, "rewrite", attempt)
    judge_cfg = stable_pool_pick(judge_pool, skill.name, "judge", attempt)
    banned = banned_terms(skill)
    skill_context = {
        "name": skill.name,
        "description": skill.description,
        "aliases": skill.aliases,
        "category": skill.category,
        "tags": skill.tags,
        "body_excerpt": skill.body[:2200],
    }
    generator_messages = [
        {
            "role": "system",
            "content": (
                "You generate benchmark prompts for a skill selector. "
                "Return JSON only with keys: prompt, rationale, leaked_terms. "
                "Write a natural Chinese user request. The request should strongly imply the skill semantically, "
                "but must not mention the exact skill name, aliases, or obvious plugin / repo identifiers."
            ),
        },
        {
            "role": "user",
            "content": json.dumps(
                {
                    "task": "Create one primary benchmark prompt",
                    "skill": skill_context,
                    "banned_terms": banned,
                },
                ensure_ascii=False,
            ),
        },
    ]
    generator_raw = chat_completion(gen_cfg, generator_messages, temperature=0.8)
    generated = ensure_object(extract_json(generator_raw), "generator")

    rewrite_messages = [
        {
            "role": "system",
            "content": (
                "Rewrite benchmark prompts. Return JSON only with keys: prompt, rewrite_notes. "
                "Keep the request in Chinese, make it sound like a real user, slightly indirect, and avoid leakage."
            ),
        },
        {
            "role": "user",
            "content": json.dumps(
                {
                    "original_prompt": generated["prompt"],
                    "skill_name": skill.name,
                    "aliases": skill.aliases,
                    "banned_terms": banned,
                },
                ensure_ascii=False,
            ),
        },
    ]
    rewrite_raw = chat_completion(rewrite_cfg, rewrite_messages, temperature=0.6)
    rewritten = ensure_object(extract_json(rewrite_raw), "rewriter")

    judge_messages = [
        {
            "role": "system",
            "content": (
                "Judge a benchmark prompt for skill selection. Return JSON only with keys: "
                "semantic_fit, leakage, leaked_terms, notes. "
                "semantic_fit is an integer 1-5. leakage is true if the prompt directly exposes the answer."
            ),
        },
        {
            "role": "user",
            "content": json.dumps(
                {
                    "prompt": rewritten["prompt"],
                    "skill": {
                        "name": skill.name,
                        "description": skill.description,
                        "aliases": skill.aliases,
                    },
                    "banned_terms": banned,
                },
                ensure_ascii=False,
            ),
        },
    ]
    judge_raw = chat_completion(judge_cfg, judge_messages, temperature=0.2)
    judged = ensure_object(extract_json(judge_raw), "judge")

    prompt = str(rewritten["prompt"]).strip()
    lower_prompt = prompt.lower()
    local_leaks = [term for term in banned if term and term in lower_prompt]
    leakage = bool(judged.get("leakage")) or bool(local_leaks)
    fit = int(judged.get("semantic_fit") or 0)
    return {
        "record_id": skill.name,
        "prompt_id": str(uuid.uuid4()),
        "difficulty": "easy",
        "target_skill": skill.name,
        "allowed_skills": [skill.name],
        "prompt": prompt,
        "skill_path": skill.path,
        "generator_model": gen_cfg.model,
        "rewriter_model": rewrite_cfg.model,
        "judge_model": judge_cfg.model,
        "generator_rationale": generated.get("rationale"),
        "rewrite_notes": rewritten.get("rewrite_notes"),
        "judge": judged,
        "local_leaked_terms": local_leaks,
        "passes": fit >= 4 and not leakage,
    }


def select_hard_pairs(skills: list[Skill], limit: int) -> list[tuple[Skill, Skill, float]]:
    token_cache = {skill.name: tokenize(f"{skill.description} {' '.join(skill.tags)}") for skill in skills}
    pairs: list[tuple[Skill, Skill, float]] = []
    for idx, left in enumerate(skills):
        best: tuple[Skill, float] | None = None
        left_tokens = token_cache[left.name]
        for right in skills[idx + 1 :]:
            right_tokens = token_cache[right.name]
            if not left_tokens or not right_tokens:
                continue
            overlap = len(left_tokens & right_tokens)
            union = len(left_tokens | right_tokens)
            if union == 0:
                continue
            score = overlap / union
            if score < 0.12:
                continue
            if best is None or score > best[1]:
                best = (right, score)
        if best is not None:
            pairs.append((left, best[0], best[1]))
    pairs.sort(key=lambda item: item[2], reverse=True)
    return pairs[:limit]


def generate_hard_record(
    target: Skill,
    distractor: Skill,
    gen_pool: list[ModelConfig],
    rewrite_pool: list[ModelConfig],
    judge_pool: list[ModelConfig],
    attempt: int = 0,
) -> dict[str, Any]:
    pair_id = f"{target.name}__vs__{distractor.name}"
    gen_cfg = stable_pool_pick(gen_pool, pair_id, "generate", attempt)
    rewrite_cfg = stable_pool_pick(rewrite_pool, pair_id, "rewrite", attempt)
    judge_cfg = stable_pool_pick(judge_pool, pair_id, "judge", attempt)
    generator_messages = [
        {
            "role": "system",
            "content": (
                "Create hard benchmark prompts for skill routing. Return JSON only with keys: prompt, rationale. "
                "Write a natural Chinese user request that should prefer target_skill over distractor_skill, "
                "without naming either skill explicitly."
            ),
        },
        {
            "role": "user",
            "content": json.dumps(
                {
                    "target_skill": {
                        "name": target.name,
                        "description": target.description,
                        "aliases": target.aliases,
                    },
                    "distractor_skill": {
                        "name": distractor.name,
                        "description": distractor.description,
                        "aliases": distractor.aliases,
                    },
                },
                ensure_ascii=False,
            ),
        },
    ]
    generated = ensure_object(extract_json(chat_completion(gen_cfg, generator_messages, temperature=0.85)), "hard-generator")
    rewritten = ensure_object(extract_json(
        chat_completion(
            rewrite_cfg,
            [
                {
                    "role": "system",
                    "content": "Rewrite to sound natural and indirect. Return JSON only with keys: prompt, rewrite_notes.",
                },
                {
                    "role": "user",
                    "content": json.dumps(
                        {
                            "original_prompt": generated["prompt"],
                            "target_skill": target.name,
                            "distractor_skill": distractor.name,
                        },
                        ensure_ascii=False,
                    ),
                },
            ],
            temperature=0.6,
        )
    ), "hard-rewriter")
    judged = ensure_object(extract_json(
        chat_completion(
            judge_cfg,
            [
                {
                    "role": "system",
                    "content": (
                        "Judge hard benchmark prompts for skill routing. Return JSON only with keys: "
                        "semantic_fit, leakage, leaked_terms, notes."
                    ),
                },
                {
                    "role": "user",
                    "content": json.dumps(
                        {
                            "prompt": rewritten["prompt"],
                            "target_skill": {"name": target.name, "description": target.description},
                            "distractor_skill": {"name": distractor.name, "description": distractor.description},
                        },
                        ensure_ascii=False,
                    ),
                },
            ],
            temperature=0.2,
        )
    ), "hard-judge")
    fit = int(judged.get("semantic_fit") or 0)
    leakage = bool(judged.get("leakage"))
    return {
        "record_id": pair_id,
        "pair_id": pair_id,
        "difficulty": "hard",
        "target_skill": target.name,
        "allowed_skills": [target.name],
        "distractor_skill": distractor.name,
        "prompt": str(rewritten["prompt"]).strip(),
        "generator_model": gen_cfg.model,
        "rewriter_model": rewrite_cfg.model,
        "judge_model": judge_cfg.model,
        "generator_rationale": generated.get("rationale"),
        "rewrite_notes": rewritten.get("rewrite_notes"),
        "judge": judged,
        "passes": fit >= 4 and not leakage,
    }


def generate_no_skill_record(
    index: int,
    gen_pool: list[ModelConfig],
    judge_pool: list[ModelConfig],
    attempt: int = 0,
) -> dict[str, Any]:
    topics = [
        "普通闲聊",
        "通用编程建议",
        "项目状态确认",
        "简单解释概念",
        "问候和寒暄",
        "不需要专门工具的重写请求",
    ]
    topic = topics[index % len(topics)]
    record_key = f"no-skill-{index:04d}"
    gen_cfg = stable_pool_pick(gen_pool, record_key, "generate", attempt)
    judge_cfg = stable_pool_pick(judge_pool, record_key, "judge", attempt)
    generated = ensure_object(extract_json(
        chat_completion(
            gen_cfg,
            [
                {
                    "role": "system",
                    "content": (
                        "Generate no-skill benchmark prompts. Return JSON only with keys: prompt, rationale. "
                        "Write a natural Chinese user request that should not require any specialized skill."
                    ),
                },
                {
                    "role": "user",
                    "content": json.dumps({"topic": topic}, ensure_ascii=False),
                },
            ],
            temperature=0.9,
        )
    ), "no-skill-generator")
    judged = ensure_object(extract_json(
        chat_completion(
            judge_cfg,
            [
                {
                    "role": "system",
                    "content": (
                        "Judge no-skill prompts. Return JSON only with keys: should_trigger_skill, notes. "
                        "should_trigger_skill must be false for a good no-skill prompt."
                    ),
                },
                {
                    "role": "user",
                    "content": json.dumps({"prompt": generated["prompt"]}, ensure_ascii=False),
                },
            ],
            temperature=0.2,
        )
    ), "no-skill-judge")
    return {
        "record_id": f"no-skill-{index:04d}",
        "difficulty": "no-skill",
        "target_skill": None,
        "allowed_skills": [],
        "prompt": str(generated["prompt"]).strip(),
        "generator_model": gen_cfg.model,
        "judge_model": judge_cfg.model,
        "generator_rationale": generated.get("rationale"),
        "judge": judged,
        "passes": judged.get("should_trigger_skill") is False,
    }


def generate_with_attempts(fn, record_key: str, *args, attempts: int = 3):
    last_record = None
    last_error = None
    for attempt in range(attempts):
        try:
            record = fn(*args, attempt=attempt)
            last_record = record
            if record.get("passes") is True:
                return record
        except Exception as exc:  # noqa: BLE001
            last_error = exc
            time.sleep(1)
    if last_record is not None:
        return last_record
    raise RuntimeError(f"{record_key} failed after {attempts} attempts: {last_error}")


def run_primary(
    skills: list[Skill],
    limit: int | None,
    concurrency: int,
    gen_pool: list[ModelConfig],
    rewrite_pool: list[ModelConfig],
    judge_pool: list[ModelConfig],
) -> None:
    existing = read_existing(PRIMARY_PATH, require_pass=True)
    pending = [skill for skill in skills if skill.name not in existing]
    if limit is not None:
        pending = pending[:limit]
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        futures = {
            pool.submit(generate_with_attempts, generate_primary_record, skill.name, skill, gen_pool, rewrite_pool, judge_pool): skill
            for skill in pending
        }
        for future in concurrent.futures.as_completed(futures):
            skill = futures[future]
            try:
                record = future.result()
                append_jsonl(PRIMARY_PATH, record)
            except Exception as exc:
                append_jsonl(
                    ERRORS_PATH,
                    {"phase": "primary", "name": skill.name, "error": str(exc), "path": skill.path},
                )


def run_hard(
    skills: list[Skill],
    limit: int,
    concurrency: int,
    gen_pool: list[ModelConfig],
    rewrite_pool: list[ModelConfig],
    judge_pool: list[ModelConfig],
) -> None:
    existing = read_existing(HARD_PATH, require_pass=True)
    pairs = select_hard_pairs(skills, limit)
    with PAIRS_PATH.open("w", encoding="utf-8") as fh:
        for left, right, score in pairs:
            fh.write(
                json.dumps({"target_skill": left.name, "distractor_skill": right.name, "similarity": score}, ensure_ascii=False)
                + "\n"
            )
    pending = [(left, right) for left, right, _ in pairs if f"{left.name}__vs__{right.name}" not in existing]
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        futures = {
            pool.submit(generate_with_attempts, generate_hard_record, f"{left.name}__vs__{right.name}", left, right, gen_pool, rewrite_pool, judge_pool): (left, right)
            for left, right in pending
        }
        for future in concurrent.futures.as_completed(futures):
            left, right = futures[future]
            try:
                record = future.result()
                append_jsonl(HARD_PATH, record)
            except Exception as exc:
                append_jsonl(
                    ERRORS_PATH,
                    {
                        "phase": "hard",
                        "name": f"{left.name}__vs__{right.name}",
                        "error": str(exc),
                    },
                )


def run_no_skill(count: int, concurrency: int, gen_pool: list[ModelConfig], judge_pool: list[ModelConfig]) -> None:
    existing = read_existing(NOSKILL_PATH, require_pass=True)
    pending = [idx for idx in range(count) if f"no-skill-{idx:04d}" not in existing]
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        futures = {
            pool.submit(generate_with_attempts, generate_no_skill_record, f"no-skill-{idx:04d}", idx, gen_pool, judge_pool): idx
            for idx in pending
        }
        for future in concurrent.futures.as_completed(futures):
            idx = futures[future]
            try:
                record = future.result()
                append_jsonl(NOSKILL_PATH, record)
            except Exception as exc:
                append_jsonl(
                    ERRORS_PATH,
                    {"phase": "no-skill", "name": f"no-skill-{idx:04d}", "error": str(exc)},
                )


def write_manifest(skills: list[Skill]) -> None:
    manifest = {
        "sample_dir": str(SAMPLE_DIR),
        "catalog_path": str(CATALOG_PATH),
        "primary_path": str(PRIMARY_PATH),
        "hard_path": str(HARD_PATH),
        "no_skill_path": str(NOSKILL_PATH),
        "errors_path": str(ERRORS_PATH),
        "skill_count": len(skills),
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    MANIFEST_PATH.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--phase", choices=["primary", "hard", "no-skill", "all"], default="all")
    parser.add_argument("--primary-limit", type=int, default=None)
    parser.add_argument("--hard-limit", type=int, default=200)
    parser.add_argument("--no-skill-count", type=int, default=100)
    parser.add_argument("--concurrency", type=int, default=8)
    args = parser.parse_args()

    random.seed(42)
    skills = load_catalog()
    write_manifest(skills)

    registry = load_model_registry(MODELS_YAML_PATH)
    gen_pool = get_model_pool("SELECTOR_GEN", DEFAULT_GEN_MODELS, registry)
    rewrite_pool = get_model_pool("SELECTOR_REWRITE", DEFAULT_REWRITE_MODELS, registry, fallback_prefix="SELECTOR_GEN")
    judge_pool = get_model_pool("SELECTOR_JUDGE", DEFAULT_JUDGE_MODELS, registry, fallback_prefix="SELECTOR_REWRITE")

    if args.phase in {"primary", "all"}:
        run_primary(skills, args.primary_limit, args.concurrency, gen_pool, rewrite_pool, judge_pool)
    if args.phase in {"hard", "all"}:
        run_hard(skills, args.hard_limit, args.concurrency, gen_pool, rewrite_pool, judge_pool)
    if args.phase in {"no-skill", "all"}:
        run_no_skill(args.no_skill_count, args.concurrency, gen_pool, judge_pool)

    summary = {
        "catalog_count": len(skills),
        "primary_records": len(read_existing(PRIMARY_PATH)),
        "hard_records": len(read_existing(HARD_PATH)),
        "no_skill_records": len(read_existing(NOSKILL_PATH)),
        "error_records": len(read_existing(ERRORS_PATH)),
    }
    print(json.dumps(summary, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
