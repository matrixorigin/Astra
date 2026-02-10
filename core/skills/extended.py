"""Additional useful skills for development tasks."""

from typing import Dict, Any
from pydantic import BaseModel
from core.skills.github_client import GitHubClient


class CodeReviewInput(BaseModel):
    """Input for code review skill."""
    repo_id: str
    pr_number: int
    focus: str = "all"  # all | security | performance | style


class CodeReviewOutput(BaseModel):
    """Output from code review skill."""
    success: bool
    review: Dict[str, Any]


class CodeReviewSkill:
    """Review code changes in a PR."""
    
    def __init__(self, github: GitHubClient):
        self.github = github
    
    def validate_input(self, input_data: dict) -> CodeReviewInput:
        return CodeReviewInput(**input_data)
    
    async def execute(self, input: CodeReviewInput) -> CodeReviewOutput:
        """Execute code review."""
        # Get PR diff
        diff = await self.github.get_pr_diff(input.repo_id, input.pr_number)
        
        # Simple analysis (can be enhanced with LLM)
        review = {
            "pr_number": input.pr_number,
            "files_changed": len(diff.get("files", [])),
            "additions": sum(f.get("additions", 0) for f in diff.get("files", [])),
            "deletions": sum(f.get("deletions", 0) for f in diff.get("files", [])),
            "focus": input.focus,
            "suggestions": []
        }
        
        # Add basic suggestions
        if review["additions"] > 500:
            review["suggestions"].append("Large PR - consider splitting into smaller changes")
        
        if review["files_changed"] > 20:
            review["suggestions"].append("Many files changed - review carefully")
        
        return CodeReviewOutput(success=True, review=review)


class SearchCodeInput(BaseModel):
    """Input for code search skill."""
    repo_id: str
    query: str
    file_pattern: str = "*"


class SearchCodeOutput(BaseModel):
    """Output from code search skill."""
    success: bool
    results: list


class SearchCodeSkill:
    """Search code in repository."""
    
    def __init__(self, github: GitHubClient):
        self.github = github
    
    def validate_input(self, input_data: dict) -> SearchCodeInput:
        return SearchCodeInput(**input_data)
    
    async def execute(self, input: SearchCodeInput) -> SearchCodeOutput:
        """Execute code search."""
        # This would use GitHub Code Search API
        # Simplified version for now
        results = [
            {
                "file": "example.py",
                "line": 42,
                "content": f"Match for: {input.query}",
                "url": f"https://github.com/repo/blob/main/example.py#L42"
            }
        ]
        
        return SearchCodeOutput(success=True, results=results)


class GenerateTestsInput(BaseModel):
    """Input for test generation skill."""
    repo_id: str
    file_path: str
    function_name: str


class GenerateTestsOutput(BaseModel):
    """Output from test generation skill."""
    success: bool
    test_code: str


class GenerateTestsSkill:
    """Generate unit tests for code."""
    
    def __init__(self, github: GitHubClient):
        self.github = github
    
    def validate_input(self, input_data: dict) -> GenerateTestsInput:
        return GenerateTestsInput(**input_data)
    
    async def execute(self, input: GenerateTestsInput) -> GenerateTestsOutput:
        """Generate tests."""
        # This would use LLM to generate tests
        # Simplified version for now
        test_code = f"""
def test_{input.function_name}():
    \"\"\"Test {input.function_name} function.\"\"\"
    # TODO: Implement test
    pass
"""
        
        return GenerateTestsOutput(success=True, test_code=test_code)


class AnalyzeBugInput(BaseModel):
    """Input for bug analysis skill."""
    repo_id: str
    issue_number: int


class AnalyzeBugOutput(BaseModel):
    """Output from bug analysis skill."""
    success: bool
    analysis: Dict[str, Any]


class AnalyzeBugSkill:
    """Analyze bug reports and suggest fixes."""
    
    def __init__(self, github: GitHubClient):
        self.github = github
    
    def validate_input(self, input_data: dict) -> AnalyzeBugInput:
        return AnalyzeBugInput(**input_data)
    
    async def execute(self, input: AnalyzeBugInput) -> AnalyzeBugOutput:
        """Analyze bug."""
        # This would fetch issue details and analyze with LLM
        analysis = {
            "issue_number": input.issue_number,
            "severity": "medium",
            "category": "bug",
            "suggested_files": [],
            "similar_issues": []
        }
        
        return AnalyzeBugOutput(success=True, analysis=analysis)


def register_extended_skills(registry, db):
    """Register extended skills."""
    from core.skills.github_client import GitHubClient
    
    github = GitHubClient(db)
    
    skills = [
        ("code_review", "1.0.0", "Review code changes in a PR", CodeReviewSkill(github)),
        ("search_code", "1.0.0", "Search code in repository", SearchCodeSkill(github)),
        ("generate_tests", "1.0.0", "Generate unit tests for code", GenerateTestsSkill(github)),
        ("analyze_bug", "1.0.0", "Analyze bug reports", AnalyzeBugSkill(github))
    ]
    
    for name, version, description, skill_instance in skills:
        try:
            registry.register(
                name=name,
                version=version,
                description=description,
                input_schema={"type": "object"},
                output_schema={"type": "object"},
                implementation=f"core.skills.extended.{skill_instance.__class__.__name__}"
            )
        except Exception:
            pass
