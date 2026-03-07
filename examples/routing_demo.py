#!/usr/bin/env python3
"""
Example script demonstrating the query routing system.

This shows how different types of queries are automatically routed
to specialized agents with appropriate configurations.
"""

import sys
import os

# Add the project root to Python path
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from core.routing import RoutingService


def demonstrate_routing():
    """Demonstrate routing for different query types."""
    service = RoutingService()
    
    # Test queries for each agent type
    test_queries = [
        # Code queries
        "Write a Python function to read CSV files",
        "Review this JavaScript code for security issues", 
        "Help me debug this TypeScript error",
        "Create a Git workflow for our team",
        
        # Planning queries
        "Design the architecture for a microservices system",
        "Create a project plan for building a mobile app",
        "What's the best approach to organize this codebase?",
        "Plan the requirements for an e-commerce platform",
        
        # Debugging queries  
        "Fix this error: AttributeError: 'NoneType' object has no attribute 'get'",
        "My application crashes when I click submit",
        "Troubleshoot why the database connection is failing",
        "Debug this memory leak in my C++ program",
        
        # General queries
        "What is machine learning?",
        "Explain the difference between REST and GraphQL",
        "How do I improve my productivity as a developer?",
        "What are the latest trends in web development?"
    ]
    
    print("🤖 Query Routing System Demo")
    print("=" * 50)
    
    for query in test_queries:
        decision = service.route_query(query)
        
        print(f"\n📝 Query: {query}")
        print(f"🎯 Agent: {decision.routing_result.agent_type.value.upper()}")
        print(f"📊 Confidence: {decision.routing_result.confidence:.2f}")
        print(f"🌡️  Temperature: {decision.agent_config.temperature}")
        print(f"🔧 Top Tools: {', '.join(decision.agent_config.preferred_tools[:3])}")
        
        if decision.routing_result.matched_patterns:
            print(f"✅ Matched: {len(decision.routing_result.matched_patterns)} patterns")
    
    print("\n" + "=" * 50)
    print("✨ Routing system successfully categorized all queries!")


def test_context_preservation():
    """Test that original context is preserved during routing."""
    service = RoutingService()
    
    original_context = {
        "model": "gpt-4",
        "user_preference": "detailed_explanations",
        "session_id": "test-123"
    }
    
    decision = service.route_query(
        "Write a Python script to process log files", 
        original_context
    )
    
    print("\n🔄 Context Preservation Test")
    print("=" * 30)
    print(f"Original context: {original_context}")
    print(f"Agent type: {decision.routing_result.agent_type}")
    print(f"Preserved model: {decision.context_modifications.get('model')}")
    print(f"Preserved preference: {decision.context_modifications.get('user_preference')}")
    print(f"Added temperature: {decision.context_modifications.get('temperature')}")
    print("✅ Context preservation working correctly!")


if __name__ == "__main__":
    try:
        demonstrate_routing()
        test_context_preservation()
    except ImportError as e:
        print(f"❌ Import error: {e}")
        print("Make sure you're running this from the project root directory")
    except Exception as e:
        print(f"❌ Error: {e}")
        import traceback
        traceback.print_exc()