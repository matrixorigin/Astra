"""Summary quality evaluation for Tool Context Engine.

Measures whether summaries preserve enough information for LLM decision-making.
Key metric: Does LLM make same decision with summary vs full output?
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import datetime
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from core.llm.client import LLMClient


@dataclass
class SummaryQualityResult:
    """Result of summary quality evaluation."""
    tool_name: str
    original_size: int
    summary_size: int
    compression_ratio: float
    
    # Quality metrics
    decision_match: bool  # Did LLM make same decision?
    expand_needed: bool   # Did LLM need to expand reference?
    key_info_preserved: float  # 0-1 score
    
    # Details
    original_decision: str = ""
    summary_decision: str = ""
    missing_info: list[str] = field(default_factory=list)


class SummaryQualityEvaluator:
    """Evaluate summary quality by comparing LLM decisions."""
    
    def __init__(self, llm_client: "LLMClient"):
        self.llm = llm_client
        self.results: list[SummaryQualityResult] = []
    
    def evaluate(
        self,
        tool_name: str,
        original_output: str,
        summary: str,
        task_context: str,
    ) -> SummaryQualityResult:
        """Evaluate if summary preserves decision-relevant information.
        
        Args:
            tool_name: Name of the tool
            original_output: Full tool output
            summary: Generated summary
            task_context: What the LLM is trying to do
        
        Returns:
            SummaryQualityResult with metrics
        """
        # Get LLM decision with full output
        original_decision = self._get_decision(original_output, task_context)
        
        # Get LLM decision with summary only
        summary_decision = self._get_decision(summary, task_context)
        
        # Compare decisions
        decision_match = self._decisions_match(original_decision, summary_decision)
        
        # Check if key info preserved
        key_info_score, missing = self._check_key_info(
            tool_name, original_output, summary
        )
        
        result = SummaryQualityResult(
            tool_name=tool_name,
            original_size=len(original_output),
            summary_size=len(summary),
            compression_ratio=len(original_output) / max(len(summary), 1),
            decision_match=decision_match,
            expand_needed=not decision_match,
            key_info_preserved=key_info_score,
            original_decision=original_decision,
            summary_decision=summary_decision,
            missing_info=missing,
        )
        
        self.results.append(result)
        return result
    
    def _get_decision(self, content: str, task_context: str) -> str:
        """Get LLM's decision based on content."""
        prompt = f"""Based on this tool output, what is your next action?
Task: {task_context}

Tool output:
{content[:8000]}

Reply with ONE of:
- FOUND: <specific file/line to examine>
- NEED_MORE: <what additional info needed>
- DONE: <conclusion>
"""
        try:
            response = self.llm.chat([{"role": "user", "content": prompt}])
            return response.content[:200] if hasattr(response, 'content') else str(response)[:200]
        except Exception:
            return "ERROR"
    
    def _decisions_match(self, d1: str, d2: str) -> bool:
        """Check if two decisions are semantically equivalent."""
        # Simple heuristic: same action type and similar target
        d1_type = d1.split(":")[0] if ":" in d1 else d1[:10]
        d2_type = d2.split(":")[0] if ":" in d2 else d2[:10]
        return d1_type.strip().upper() == d2_type.strip().upper()
    
    def _check_key_info(
        self, tool_name: str, original: str, summary: str
    ) -> tuple[float, list[str]]:
        """Check if key information is preserved in summary."""
        missing = []
        score = 1.0
        
        if tool_name == "grep":
            # Key info: file count, match count, top files
            orig_lines = original.strip().split('\n')
            
            # Check match count preserved
            if str(len(orig_lines)) not in summary:
                missing.append(f"match_count ({len(orig_lines)})")
                score -= 0.2
            
            # Check file names preserved
            files = set()
            for line in orig_lines[:100]:
                if ':' in line:
                    files.add(line.split(':')[0])
            
            preserved_files = sum(1 for f in list(files)[:5] if f in summary)
            if preserved_files < min(3, len(files)):
                missing.append(f"top_files (only {preserved_files}/{min(5, len(files))})")
                score -= 0.3
        
        elif tool_name in ("shell", "execute_bash"):
            # Key info: exit status hint, error messages
            if "error" in original.lower() and "error" not in summary.lower():
                missing.append("error_messages")
                score -= 0.4
        
        return max(0, score), missing
    
    def get_aggregate_metrics(self) -> dict:
        """Get aggregate quality metrics."""
        if not self.results:
            return {"total_evaluations": 0}
        
        return {
            "total_evaluations": len(self.results),
            "decision_match_rate": sum(r.decision_match for r in self.results) / len(self.results),
            "expand_needed_rate": sum(r.expand_needed for r in self.results) / len(self.results),
            "avg_key_info_preserved": sum(r.key_info_preserved for r in self.results) / len(self.results),
            "avg_compression_ratio": sum(r.compression_ratio for r in self.results) / len(self.results),
            "by_tool": self._group_by_tool(),
        }
    
    def _group_by_tool(self) -> dict:
        """Group metrics by tool name."""
        by_tool: dict[str, list] = {}
        for r in self.results:
            if r.tool_name not in by_tool:
                by_tool[r.tool_name] = []
            by_tool[r.tool_name].append(r)
        
        return {
            tool: {
                "count": len(results),
                "decision_match_rate": sum(r.decision_match for r in results) / len(results),
                "avg_key_info": sum(r.key_info_preserved for r in results) / len(results),
            }
            for tool, results in by_tool.items()
        }
