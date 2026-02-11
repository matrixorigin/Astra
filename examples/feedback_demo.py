#!/usr/bin/env python3
"""Demo: Prompt feedback collection and analysis."""

from sdk import Database
from core.context.prompts import PromptManager, PromptFeedback


def demo_feedback_collection():
    """Demonstrate feedback collection and analysis."""
    db = Database()
    manager = PromptManager(db)
    feedback = PromptFeedback(db)

    print("=" * 60)
    print("Demo: Prompt Feedback Collection")
    print("=" * 60)

    # 0. Ensure prompt exists
    print("\n0. Ensuring system_code_review prompt exists...")
    print("-" * 60)

    existing = db.fetchone("""
        SELECT * FROM prompt_templates 
        WHERE template_id = 'system_code_review' AND version = '1.0'
    """)

    if not existing:
        manager.register_prompt(
            template_id="system_code_review",
            version="1.0",
            content="You are an expert code reviewer. Focus on code quality, security, and best practices.",
            is_active=True,
        )
        print("✓ Registered system_code_review@1.0")
    else:
        print("✓ system_code_review@1.0 already exists")

    # 1. Simulate user feedback for v1.0
    print("\n1. Collecting feedback for system_code_review v1.0...")
    print("-" * 60)

    feedback_data_v1 = [
        (5, "Great review, very helpful!"),
        (4, "Good suggestions"),
        (3, "Okay but missed some issues"),
        (2, "Not detailed enough"),
        (4, "Helpful overall"),
    ]

    for rating, comment in feedback_data_v1:
        feedback.record_feedback(
            prompt_template_id="system_code_review",
            prompt_version="1.0",
            llm_request_id=f"demo_req_v1_{rating}",
            user_rating=rating,
            user_comment=comment,
        )

    print(f"✓ Recorded {len(feedback_data_v1)} feedback entries")

    # 2. Get statistics
    print("\n2. Feedback statistics for v1.0:")
    print("-" * 60)

    stats_v1 = feedback.get_feedback_stats("system_code_review", "1.0")
    print(f"Total feedback: {stats_v1['total_count']}")
    print(f"Average rating: {stats_v1['avg_rating']:.2f}")
    print(f"Rating range: {stats_v1['min_rating']} - {stats_v1['max_rating']}")
    print(f"Distribution: {stats_v1['distribution']}")

    # 3. Analyze low scores
    print("\n3. Low-score cases (rating <= 2):")
    print("-" * 60)

    low_scores = feedback.get_low_score_cases("system_code_review", threshold=2)
    for case in low_scores:
        print(f"  Rating {case['rating']}: {case['comment']}")

    # 4. Register improved version
    print("\n4. Registering improved version (v1.1)...")
    print("-" * 60)

    improved_prompt = """You are a senior code reviewer with 10+ years of experience.

Your review process:
1. Check for security vulnerabilities (OWASP Top 10)
2. Verify error handling and edge cases
3. Assess code readability and maintainability
4. Suggest performance optimizations
5. Ensure proper testing coverage

Be specific and provide code examples."""

    manager.register_prompt(
        template_id="system_code_review", version="1.1", content=improved_prompt, is_active=True
    )
    print("✓ Version 1.1 registered")

    # 5. Simulate feedback for v1.1 (better ratings)
    print("\n5. Collecting feedback for v1.1...")
    print("-" * 60)

    feedback_data_v1_1 = [
        (5, "Excellent! Very detailed"),
        (5, "Perfect review"),
        (4, "Much better than before"),
        (5, "Great improvements"),
        (4, "Very helpful"),
    ]

    for rating, comment in feedback_data_v1_1:
        feedback.record_feedback(
            prompt_template_id="system_code_review",
            prompt_version="1.1",
            llm_request_id=f"demo_req_v1.1_{rating}",
            user_rating=rating,
            user_comment=comment,
        )

    print(f"✓ Recorded {len(feedback_data_v1_1)} feedback entries")

    # 6. Compare versions
    print("\n6. Version comparison:")
    print("-" * 60)

    comparison = feedback.compare_versions("system_code_review", "1.0", "1.1")

    print(f"Version {comparison['version_a']}:")
    print(f"  Average rating: {comparison['stats_a']['avg_rating']:.2f}")
    print(f"  Total feedback: {comparison['stats_a']['total_count']}")

    print(f"\nVersion {comparison['version_b']}:")
    print(f"  Average rating: {comparison['stats_b']['avg_rating']:.2f}")
    print(f"  Total feedback: {comparison['stats_b']['total_count']}")

    if comparison["improvement"]:
        print(f"\n✓ Improvement: +{comparison['improvement']:.2f} points")

    print("\n" + "=" * 60)
    print("✓ Demo complete! Feedback system is working.")
    print("=" * 60)


if __name__ == "__main__":
    demo_feedback_collection()
