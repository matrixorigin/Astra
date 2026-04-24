#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

import jieba
import yaml

from prototype_hybrid_selector import RESULTS_DIR, SAMPLE_DIR, Skill, best_rank, load_catalog, load_latest_primary


TOKEN_CACHE_PATH = RESULTS_DIR / "tokenized-selector-skill-cache.json"
STOPWORDS = {
    "a",
    "an",
    "and",
    "are",
    "at",
    "by",
    "do",
    "does",
    "for",
    "from",
    "help",
    "how",
    "in",
    "is",
    "me",
    "my",
    "need",
    "of",
    "on",
    "or",
    "our",
    "please",
    "the",
    "to",
    "use",
    "using",
    "want",
    "we",
    "what",
    "with",
    "you",
    "your",
    "一下",
    "一个",
    "一些",
    "不一定",
    "为",
    "了",
    "从",
    "你",
    "做",
    "到",
    "前",
    "后",
    "和",
    "在",
    "如何",
    "将",
    "帮",
    "我们",
    "我",
    "把",
    "是",
    "有",
    "用",
    "给",
    "请",
    "这",
    "这个",
    "进行",
    "那个",
    "那",
    "需要",
}
CANONICAL_GROUPS = {
    "accessibility": ["无障碍", "视障", "屏幕阅读器", "a11y", "accessibility"],
    "ads": ["广告", "营销", "投放", "campaign", "ads", "meta", "googleads"],
    "api": ["接口", "api", "endpoint"],
    "auth": ["认证", "鉴权", "授权", "登录", "auth", "authentication", "authorize", "login", "oauth", "jwt"],
    "billing": ["账单", "发票", "billing", "invoice", "payment", "支付"],
    "build": ["构建", "编译", "打包", "build", "compile", "package"],
    "ci": ["持续集成", "持续部署", "流水线", "workflow", "pipeline", "ci", "cd"],
    "crm": ["客户", "线索", "crm", "lead", "sales"],
    "csv": ["csv", "表格", "excel", "xlsx"],
    "db": ["数据库", "数仓", "db", "database", "sql", "mysql", "postgres", "postgresql"],
    "debug": ["排查", "调试", "诊断", "debug", "troubleshoot", "investigate"],
    "deploy": ["部署", "发布", "上线", "发版", "deploy", "deployment", "release", "ship"],
    "docker": ["容器", "docker"],
    "email": ["邮件", "邮箱", "email", "mail"],
    "embedding": ["向量", "embedding", "embeddings"],
    "file": ["文件", "文档", "pdf", "word", "ppt", "image", "图片", "图像"],
    "finance": ["金融", "合规", "风险", "fintech", "compliance", "risk"],
    "git": ["代码库", "仓库", "git", "github", "repo", "repository", "pr"],
    "http": ["请求", "响应", "http", "https", "rest"],
    "image": ["图片", "图像", "ocr", "vision", "image"],
    "k8s": ["k8s", "kubernetes"],
    "linux": ["服务器", "终端", "命令行", "linux", "shell", "bash", "ssh", "server"],
    "log": ["日志", "log", "logging", "trace"],
    "monitor": ["监控", "告警", "metrics", "monitor", "monitoring", "alert"],
    "node": ["node", "nodejs", "npm", "pnpm", "yarn"],
    "python": ["python", "py"],
    "redis": ["缓存", "cache", "redis"],
    "rollback": ["回滚", "撤回", "rollback", "revert"],
    "rust": ["rust"],
    "search": ["搜索", "检索", "search", "find"],
    "security": ["安全", "漏洞", "攻击", "防护", "security", "xss", "csrf", "sqli"],
    "skill": ["技能", "模块", "插件", "tool", "tools", "skill"],
    "test": ["测试", "验证", "test", "testing", "qa"],
    "vue": ["vue", "vue3"],
    "web": ["网页", "网站", "浏览器", "web", "html", "css", "seo"],
}
ASCII_TOKEN_RE = re.compile(r"[a-z0-9][a-z0-9_./:-]*")
CJK_RE = re.compile(r"[\u4e00-\u9fff]")
VERSION = 2
CANONICAL_MAP = {variant: canonical for canonical, variants in CANONICAL_GROUPS.items() for variant in variants}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Prototype a simple tokenized lexical selector.")
    parser.add_argument("--limit", type=int, default=None)
    parser.add_argument("--top-k", type=int, default=50)
    parser.add_argument(
        "--summary-out",
        default=str(RESULTS_DIR / "tokenized-selector-summary.json"),
    )
    parser.add_argument(
        "--rows-out",
        default=str(RESULTS_DIR / "tokenized-selector-rows.jsonl"),
    )
    return parser.parse_args()


def normalize_text(text: str) -> str:
    return " ".join(text.lower().split())


def tokenize_text(text: str) -> list[str]:
    tokens: list[str] = []
    for raw in jieba.lcut(text, cut_all=False):
        token = raw.strip().lower()
        if not token:
            continue
        if CJK_RE.search(token):
            if len(token) >= 2 and token not in STOPWORDS:
                tokens.append(token)
            continue
        for part in ASCII_TOKEN_RE.findall(token):
            if len(part) >= 2 and part not in STOPWORDS:
                tokens.append(part)
    seen = set()
    out: list[str] = []
    for token in tokens:
        token = CANONICAL_MAP.get(token, token)
        if token in seen:
            continue
        seen.add(token)
        out.append(token)
    return out


def body_excerpt(text: str, max_nonempty_lines: int = 24) -> str:
    lines = text.splitlines()
    end = next(i for i, line in enumerate(lines[1:], 1) if line.strip() == "---")
    body = "\n".join(lines[end + 1 :]).strip()
    selected = [line.strip() for line in body.splitlines() if line.strip()][:max_nonempty_lines]
    return "\n".join(selected)


def skill_cache_key() -> list[str]:
    ids = []
    for entry in SAMPLE_DIR.iterdir():
        if not entry.is_dir():
            continue
        skill_md = entry / "SKILL.md"
        if skill_md.is_file():
            ids.append(entry.name)
    return sorted(ids)


def build_skill_index(skills: list[Skill]) -> list[dict[str, Any]]:
    out = []
    for skill in skills:
        skill_path = SAMPLE_DIR / skill.name / "SKILL.md"
        text = skill_path.read_text(encoding="utf-8")
        desc_text = " ".join(
            piece for piece in [skill.description, skill.when_to_use or "", skill.category or ""] if piece.strip()
        )
        body_text = body_excerpt(text)
        exact_aliases = [normalize_text(x) for x in skill.aliases if x.strip()]
        exact_triggers = [normalize_text(x) for x in skill.triggers if x.strip()]
        out.append(
            {
                "name": skill.name,
                "name_exact": normalize_text(skill.name),
                "alias_exact": exact_aliases,
                "trigger_exact": exact_triggers,
                "name_tokens": tokenize_text(skill.name),
                "alias_tokens": tokenize_text(" ".join(skill.aliases)),
                "trigger_tokens": tokenize_text(" ".join(skill.triggers)),
                "tag_tokens": tokenize_text(" ".join(skill.tags + ([skill.category] if skill.category else []))),
                "desc_tokens": tokenize_text(desc_text),
                "body_tokens": tokenize_text(body_text),
            }
        )
    return out


def load_or_build_skill_index(skills: list[Skill]) -> list[dict[str, Any]]:
    cache_key = skill_cache_key()
    if TOKEN_CACHE_PATH.exists():
        cached = json.loads(TOKEN_CACHE_PATH.read_text(encoding="utf-8"))
        if cached.get("version") == VERSION and cached.get("skill_ids") == cache_key:
            return cached["skills"]
    skills_index = build_skill_index(skills)
    TOKEN_CACHE_PATH.parent.mkdir(parents=True, exist_ok=True)
    TOKEN_CACHE_PATH.write_text(
        json.dumps({"version": VERSION, "skill_ids": cache_key, "skills": skills_index}, ensure_ascii=False, indent=2),
        encoding="utf-8",
    )
    return skills_index


def overlap_count(query_tokens: set[str], field_tokens: list[str]) -> int:
    if not query_tokens or not field_tokens:
        return 0
    return sum(1 for token in field_tokens if token in query_tokens)


def score_skill(query: str, query_tokens: list[str], skill: dict[str, Any]) -> float:
    query_norm = normalize_text(query)
    query_set = set(query_tokens)
    name_hit = 1 if skill["name_exact"] and skill["name_exact"] in query_norm else 0
    alias_hit = 1 if any(alias and alias in query_norm for alias in skill["alias_exact"]) else 0
    trigger_hit = 1 if any(trigger and trigger in query_norm for trigger in skill["trigger_exact"]) else 0

    name_overlap = overlap_count(query_set, skill["name_tokens"])
    alias_overlap = overlap_count(query_set, skill["alias_tokens"])
    trigger_overlap = overlap_count(query_set, skill["trigger_tokens"])
    tag_overlap = overlap_count(query_set, skill["tag_tokens"])
    desc_overlap = overlap_count(query_set, skill["desc_tokens"])
    body_overlap = overlap_count(query_set, skill["body_tokens"])

    matched_any = set()
    for field in ("name_tokens", "alias_tokens", "trigger_tokens", "tag_tokens", "desc_tokens", "body_tokens"):
        matched_any.update(token for token in skill[field] if token in query_set)

    return (
        30.0 * name_hit
        + 24.0 * alias_hit
        + 18.0 * trigger_hit
        + 10.0 * name_overlap
        + 8.0 * alias_overlap
        + 6.0 * trigger_overlap
        + 3.0 * tag_overlap
        + 2.0 * desc_overlap
        + 1.0 * body_overlap
        + 2.0 * len(matched_any)
    )


def metric_from_rank(rank: int | None) -> dict[str, Any]:
    return {
        "best_rank": rank,
        "hit_at_1": rank is not None and rank <= 1,
        "hit_at_10": rank is not None and rank <= 10,
        "hit_at_30": rank is not None and rank <= 30,
        "hit_at_50": rank is not None and rank <= 50,
    }


def summarize(rows: list[dict[str, Any]], field: str) -> dict[str, Any]:
    total = len(rows)
    ranks = [row[field]["best_rank"] for row in rows if row[field]["best_rank"] is not None]
    return {
        "evaluated_records": total,
        "hit_at_1_rate": sum(1 for row in rows if row[field]["hit_at_1"]) / total,
        "hit_at_10_rate": sum(1 for row in rows if row[field]["hit_at_10"]) / total,
        "hit_at_30_rate": sum(1 for row in rows if row[field]["hit_at_30"]) / total,
        "hit_at_50_rate": sum(1 for row in rows if row[field]["hit_at_50"]) / total,
        "avg_best_rank_on_hit": (sum(ranks) / len(ranks)) if ranks else None,
        "misses_not_shortlisted": sum(1 for row in rows if row[field]["best_rank"] is None),
    }


def main() -> None:
    args = parse_args()
    skills = load_catalog()
    records = load_latest_primary(args.limit)
    skill_index = {skill.name: idx for idx, skill in enumerate(skills)}
    tokenized_skills = load_or_build_skill_index(skills)

    rows = []
    for record in records:
        query_tokens = tokenize_text(record.prompt)
        scored = [(score_skill(record.prompt, query_tokens, skill), idx) for idx, skill in enumerate(tokenized_skills)]
        scored.sort(key=lambda item: (-item[0], item[1]))
        ranked = [idx for _, idx in scored[: args.top_k]]
        target_idx = skill_index[record.target_skill]
        rows.append(
            {
                "prompt_id": record.prompt_id,
                "record_id": record.record_id,
                "target_skill": record.target_skill,
                "query_tokens": query_tokens,
                "tokenized_selector": metric_from_rank(best_rank(target_idx, ranked)),
                "top_candidates": [skills[idx].name for idx in ranked[:10]],
            }
        )

    summary = {
        "record_count": len(rows),
        "top_k": args.top_k,
        "tokenizer": "jieba",
        "methods": {
            "tokenized_selector": summarize(rows, "tokenized_selector"),
        },
    }

    rows_path = Path(args.rows_out)
    rows_path.parent.mkdir(parents=True, exist_ok=True)
    with rows_path.open("w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row, ensure_ascii=False) + "\n")

    summary_path = Path(args.summary_out)
    summary_path.parent.mkdir(parents=True, exist_ok=True)
    summary_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
