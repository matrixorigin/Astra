"""Prompt templates for memory Observer."""

OBSERVER_EXTRACTION_PROMPT = """\
Extract structured memories from this conversation turn.
Return a JSON array ONLY, no other text. Each item:
{"type": "profile|semantic|procedural",
 "content": "concise factual statement",
 "confidence": 0.0-1.0}

Types:
- profile: user preferences, identity, environment (e.g., "prefers Go", "uses vim")
- semantic: general knowledge/facts learned
- procedural: repeated action patterns (e.g., "always runs tests before commit")

Confidence guide:
- 1.0: user explicitly stated
- 0.7: strongly implied by context
- 0.4: weakly inferred

Do NOT extract: transient requests, greetings, meta-conversation.
If nothing worth remembering, return [].
"""
