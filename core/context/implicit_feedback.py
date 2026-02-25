"""Implicit feedback mining from conversation history.

Two layers:
  1. Lightweight heuristic detector (runs inline, zero LLM cost)
  2. Deep async analyzer (batch LLM analysis of conversation pairs)

Taxonomy follows Don-Yehiya et al. (2024) / Liu et al. (2025):
  - rephrasing: user restates the same request differently
  - correction: user points out the answer was wrong
  - clarification: user asks for missing details
  - frustration: emotional dissatisfaction signals
  - positive: explicit praise or acceptance
  - neutral: new topic or follow-up (no feedback signal)
"""

import logging
import re
from dataclasses import dataclass, field
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session
from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)

# ── Heuristic patterns (Chinese + English) ──────────────────────

_NEGATIVE_PATTERNS = [
    # Correction signals
    re.compile(r"不对|错了|不是这样|你搞错|不正确|wrong|incorrect|that'?s not", re.I),
    # Frustration signals
    re.compile(r"没用|废话|能不能好好|别废话|太啰嗦|太长了|说重点|useless|terrible|awful|wtf|seriously\?", re.I),
    # Rephrasing signals (user re-asks)
    re.compile(r"我(再|重新)说一(遍|次)|let me rephrase|i('?m| am) asking|i meant|what i want", re.I),
    # Clarification demand
    re.compile(r"具体(一点|点)|详细(说|讲)|举个例子|比如呢|能展开|be more specific|give.+example|elaborate", re.I),
]

_POSITIVE_PATTERNS = [
    re.compile(r"^(谢谢|感谢|太好了|完美|不错|很好|棒|thanks|thank you|perfect|great|awesome|nice|good job|well done)", re.I),
]


@dataclass
class ImplicitSignal:
    """A detected implicit feedback signal."""
    signal_type: str  # rephrasing | correction | frustration | clarification | positive | neutral
    confidence: float  # 0.0 - 1.0
    evidence: str = ""  # matched pattern or reason


@dataclass
class ConversationPair:
    """A (user_query, agent_response, user_followup) triple for analysis."""
    event_id: str  # event_id of the agent response being evaluated
    user_query: str
    agent_response: str
    user_followup: str
    session_id: str = ""


class ImplicitFeedbackDetector:
    """Lightweight heuristic detector — runs inline with zero LLM cost."""

    @staticmethod
    def detect(user_input: str, prev_agent_response: str | None = None) -> ImplicitSignal:
        """Detect implicit feedback signal from user's new input.

        Args:
            user_input: Current user message
            prev_agent_response: Previous agent response (for context)

        Returns:
            ImplicitSignal with type and confidence
        """
        text_lower = user_input.strip().lower()

        # Short inputs after a response are often corrections
        if prev_agent_response and len(text_lower) < 10:
            for pat in _NEGATIVE_PATTERNS[:2]:  # correction patterns
                if pat.search(text_lower):
                    return ImplicitSignal("correction", 0.9, pat.pattern)

        # Check negative patterns
        for i, pat in enumerate(_NEGATIVE_PATTERNS):
            if pat.search(user_input):
                types = ["correction", "frustration", "rephrasing", "clarification"]
                return ImplicitSignal(types[min(i, 3)], 0.7, pat.pattern)

        # Check positive patterns
        for pat in _POSITIVE_PATTERNS:
            if pat.search(user_input):
                return ImplicitSignal("positive", 0.6, pat.pattern)

        return ImplicitSignal("neutral", 0.3)


class ImplicitFeedbackMiner(DbConsumer):
    """Deep async analyzer — batch LLM analysis of conversation pairs."""

    def __init__(self, db_factory: DbFactory, llm_client=None):
        super().__init__(db_factory)
        self.llm = llm_client

    def extract_pairs(self, session_id: str | None = None, limit: int = 50) -> list[ConversationPair]:
        """Extract (query, response, followup) triples from conversation history."""
        with self._db() as db:
            where = "WHERE e.event_type IN ('user_query', 'llm_response')"
            params: dict[str, Any] = {"limit": limit}
            if session_id:
                where += " AND e.session_id = :sid"
                params["sid"] = session_id

            rows = db.execute(
                text(f"""
                    SELECT e.event_id, e.session_id, e.event_type, e.content,
                           e.parent_event_id, e.created_at
                    FROM conversation_events e
                    {where}
                    ORDER BY e.session_id, e.created_at
                    LIMIT :limit
                """),
                params,
            ).fetchall()

            # Group by session, build triples
            sessions: dict[str, list] = {}
            for r in rows:
                sid = r[1]
                sessions.setdefault(sid, []).append({
                    "event_id": r[0], "event_type": r[2],
                    "content": r[3] or "", "parent_event_id": r[4],
                })

            pairs = []
            for sid, events in sessions.items():
                for i in range(len(events) - 2):
                    if (events[i]["event_type"] == "user_query"
                        and events[i+1]["event_type"] == "llm_response"
                        and events[i+2]["event_type"] == "user_query"):
                        pairs.append(ConversationPair(
                            event_id=events[i+1]["event_id"],
                            user_query=events[i]["content"],
                            agent_response=events[i+1]["content"],
                            user_followup=events[i+2]["content"],
                            session_id=sid,
                        ))
            return pairs

    def analyze_batch(self, pairs: list[ConversationPair] | None = None,
                      session_id: str | None = None) -> list[dict[str, Any]]:
        """Analyze conversation pairs for implicit feedback.

        Uses heuristic first, escalates ambiguous cases to LLM.
        Returns list of {pair, signal, rating} dicts.
        """
        if pairs is None:
            pairs = self.extract_pairs(session_id=session_id)

        results = []
        llm_batch = []

        for pair in pairs:
            signal = ImplicitFeedbackDetector.detect(pair.user_followup, pair.agent_response)
            if signal.confidence >= 0.7:
                results.append(self._signal_to_feedback(pair, signal))
            elif signal.signal_type != "neutral":
                llm_batch.append((pair, signal))

        # Escalate ambiguous cases to LLM if available
        if llm_batch and self.llm:
            llm_results = self._llm_classify(llm_batch)
            results.extend(llm_results)

        return results

    def analyze_and_store(self, session_id: str | None = None,
                          template_id: str = "system_general") -> int:
        """Full pipeline: extract → analyze → store as llm_feedback records.

        Returns number of feedback records created.
        """
        with self._db() as db:
            results = self.analyze_batch(session_id=session_id)
            count = 0
            from core.context.prompts import PromptFeedback
            pf = PromptFeedback(self._db_factory)
            for r in results:
                try:
                    pf.record_feedback(
                        prompt_template_id=template_id,
                        prompt_version="auto",
                        llm_request_id=r["event_id"],
                        user_rating=r["rating"],
                        user_comment=f"[implicit:{r['signal_type']}] {r['evidence']}",
                        metadata={"source": "implicit_mining", "confidence": str(r["confidence"])},
                    )
                    count += 1
                except Exception as e:
                    logger.debug(f"Failed to store implicit feedback: {e}")
            if count:
                db.commit()
                logger.info(f"Stored {count} implicit feedback records")
            return count

    def _signal_to_feedback(self, pair: ConversationPair, signal: ImplicitSignal) -> dict[str, Any]:
        """Convert signal to feedback dict with estimated rating."""
        rating_map = {
            "correction": 1, "frustration": 1,
            "rephrasing": 2, "clarification": 3,
            "positive": 5, "neutral": 3,
        }
        return {
            "event_id": pair.event_id,
            "session_id": pair.session_id,
            "signal_type": signal.signal_type,
            "confidence": signal.confidence,
            "evidence": signal.evidence or pair.user_followup[:100],
            "rating": rating_map.get(signal.signal_type, 3),
            "user_followup": pair.user_followup[:200],
        }

    def _llm_classify(self, batch: list[tuple[ConversationPair, ImplicitSignal]]) -> list[dict[str, Any]]:
        """Use LLM to classify ambiguous cases."""
        results = []
        # Build batch prompt for efficiency
        cases_text = ""
        for i, (pair, _) in enumerate(batch[:10]):  # Cap at 10 per batch
            cases_text += (
                f"\n--- Case {i+1} ---\n"
                f"User asked: {pair.user_query[:200]}\n"
                f"Agent replied: {pair.agent_response[:300]}\n"
                f"User then said: {pair.user_followup[:200]}\n"
            )

        prompt = f"""Classify each user follow-up as implicit feedback on the agent's response.

Categories:
- correction: user says the answer was wrong
- frustration: user expresses dissatisfaction or annoyance
- rephrasing: user restates the same question differently
- clarification: user asks for more detail or examples
- positive: user expresses satisfaction
- neutral: user moves to a new topic (no feedback)

For each case, respond with ONE line: "Case N: <category> <confidence 0.0-1.0>"
{cases_text}"""

        try:
            response = self.llm.chat(
                messages=[{"role": "user", "content": prompt}],
                user_id="system", temperature=0.1,
            )
            content = response.content if hasattr(response, "content") else str(response)

            for line in content.strip().split("\n"):
                match = re.match(r"Case\s+(\d+):\s*(\w+)\s+([\d.]+)", line)
                if match:
                    idx = int(match.group(1)) - 1
                    if 0 <= idx < len(batch):
                        pair, _ = batch[idx]
                        signal = ImplicitSignal(
                            signal_type=match.group(2).lower(),
                            confidence=min(float(match.group(3)), 1.0),
                            evidence=f"llm_classified: {line.strip()}",
                        )
                        results.append(self._signal_to_feedback(pair, signal))
        except Exception as e:
            logger.warning(f"LLM classification failed: {e}")

        return results
