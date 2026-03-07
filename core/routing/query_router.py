"""Minimal query router for agent specialization."""

import re
from enum import Enum
from typing import Dict, List, Optional

from pydantic import BaseModel


class AgentType(str, Enum):
    """Available agent types."""
    CODE = "code"
    PLANNING = "planning" 
    DEBUGGING = "debugging"
    GENERAL = "general"


class RoutingResult(BaseModel):
    """Result of query routing."""
    agent_type: AgentType
    confidence: float
    matched_patterns: List[str]


class QueryRouter:
    """Routes queries to appropriate agent types based on content analysis."""
    
    def __init__(self):
        # Code patterns - look for programming languages, file extensions, code blocks
        self.code_patterns = [
            r'\b(python|javascript|typescript|java|cpp|rust|go|php|ruby)\b',
            r'\.(py|js|ts|java|cpp|rs|go|php|rb|html|css|sql)\b',
            r'```[\w]*\n',  # code blocks
            r'\b(function|class|import|export|def|async|await)\b',
            r'\b(git|github|repository|repo|commit|branch|merge)\b',
        ]
        
        # Planning patterns - look for project management, architecture, design
        self.planning_patterns = [
            r'\b(plan|design|architect|strategy|roadmap|proposal)\b',
            r'\b(requirements|specification|scope|timeline|milestone)\b',
            r'\b(structure|organize|approach|methodology)\b',
            r'\b(project|system|application|solution)\b.*\b(design|plan|build)\b',
        ]
        
        # Debugging patterns - look for errors, issues, troubleshooting
        self.debugging_patterns = [
            r'\b(error|bug|issue|problem|fail|crash|exception)\b',
            r'\b(debug|troubleshoot|fix|solve|resolve)\b',
            r'\b(not working|broken|incorrect|wrong)\b',
            r'\b(traceback|stack trace|error message)\b',
        ]
    
    def route(self, query: str) -> RoutingResult:
        """Route query to appropriate agent type."""
        if not query:
            return RoutingResult(
                agent_type=AgentType.GENERAL,
                confidence=1.0,
                matched_patterns=[]
            )
        
        query_lower = query.lower()
        
        # Score each agent type
        scores = {
            AgentType.CODE: self._score_patterns(query_lower, self.code_patterns),
            AgentType.PLANNING: self._score_patterns(query_lower, self.planning_patterns), 
            AgentType.DEBUGGING: self._score_patterns(query_lower, self.debugging_patterns),
        }
        
        # Find best match
        best_type = max(scores.keys(), key=lambda k: scores[k]['score'])
        best_score = scores[best_type]['score']
        
        # If no strong match, default to general
        if best_score < 0.3:
            return RoutingResult(
                agent_type=AgentType.GENERAL,
                confidence=1.0 - best_score,
                matched_patterns=[]
            )
        
        return RoutingResult(
            agent_type=best_type,
            confidence=best_score,
            matched_patterns=scores[best_type]['patterns']
        )
    
    def _score_patterns(self, query: str, patterns: List[str]) -> Dict:
        """Score query against pattern list."""
        matched_patterns = []
        total_matches = 0
        
        for pattern in patterns:
            matches = re.findall(pattern, query, re.IGNORECASE)
            if matches:
                matched_patterns.append(pattern)
                total_matches += len(matches)
        
        # Normalize score based on pattern matches and boost for multiple matches
        if total_matches == 0:
            score = 0.0
        else:
            # Base score from match count, with diminishing returns
            base_score = min(total_matches * 0.3, 1.0)
            # Bonus for multiple different patterns matching
            pattern_bonus = min(len(matched_patterns) * 0.2, 0.6)
            score = min(base_score + pattern_bonus, 1.0)
        
        return {
            'score': score,
            'patterns': matched_patterns
        }