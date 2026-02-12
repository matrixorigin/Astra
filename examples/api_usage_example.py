"""Example usage of Agent Engine API."""

import requests

# Base URL
BASE_URL = "http://localhost:8000"


def example_complete_workflow():
    """Complete workflow example: register, login, create agent, session, and events."""
    
    # 1. Register user
    print("1. Registering user...")
    response = requests.post(
        f"{BASE_URL}/auth/register",
        json={
            "username": "alice",
            "email": "alice@example.com",
            "password": "secure_password",
            "display_name": "Alice"
        }
    )
    print(f"   Status: {response.status_code}")
    user = response.json()
    print(f"   User ID: {user['user_id']}")
    
    # 2. Login
    print("\n2. Logging in...")
    response = requests.post(
        f"{BASE_URL}/auth/login",
        json={
            "username": "alice",
            "password": "secure_password"
        }
    )
    tokens = response.json()
    access_token = tokens["access_token"]
    refresh_token = tokens["refresh_token"]
    print(f"   Access token: {access_token[:20]}...")
    
    # Headers for authenticated requests
    headers = {"Authorization": f"Bearer {access_token}"}
    
    # 3. Create agent
    print("\n3. Creating agent...")
    response = requests.post(
        f"{BASE_URL}/agents",
        headers=headers,
        json={
            "agent_name": "My Assistant",
            "agent_type": "chatbot",
            "config": {
                "model": "gpt-4",
                "temperature": 0.7
            }
        }
    )
    agent = response.json()
    agent_id = agent["agent_id"]
    print(f"   Agent ID: {agent_id}")
    print(f"   Agent Name: {agent['agent_name']}")
    
    # 4. Create session
    print("\n4. Creating session...")
    response = requests.post(
        f"{BASE_URL}/sessions",
        headers=headers,
        json={
            "metadata": {
                "context": "demo",
                "agent_id": agent_id
            }
        }
    )
    session = response.json()
    session_id = session["session_id"]
    print(f"   Session ID: {session_id}")
    
    # 5. Create user query event
    print("\n5. Creating user query event...")
    response = requests.post(
        f"{BASE_URL}/events",
        headers=headers,
        json={
            "session_id": session_id,
            "event_type": "user_query",
            "content": "What is the weather today?",
            "metadata": {
                "source": "api_example"
            }
        }
    )
    event1 = response.json()
    print(f"   Event ID: {event1['event_id']}")
    print(f"   Content: {event1['content']}")
    
    # 6. Create LLM response event
    print("\n6. Creating LLM response event...")
    response = requests.post(
        f"{BASE_URL}/events",
        headers=headers,
        json={
            "session_id": session_id,
            "event_type": "llm_response",
            "content": "The weather is sunny with a high of 75°F.",
            "metadata": {
                "agent_id": agent_id,
                "agent_version": "1.0.0",
                "model": "gpt-4"
            }
        }
    )
    event2 = response.json()
    print(f"   Event ID: {event2['event_id']}")
    print(f"   Content: {event2['content']}")
    
    # 7. List events for session
    print("\n7. Listing events for session...")
    response = requests.get(
        f"{BASE_URL}/events",
        headers=headers,
        params={"session_id": session_id}
    )
    events = response.json()
    print(f"   Total events: {events['total']}")
    for event in events['events']:
        print(f"   - {event['event_type']}: {event['content'][:50]}...")
    
    # 8. Get session details
    print("\n8. Getting session details...")
    response = requests.get(
        f"{BASE_URL}/sessions/{session_id}",
        headers=headers
    )
    session = response.json()
    print(f"   Session ID: {session['session_id']}")
    print(f"   Status: {session['status']}")
    print(f"   Event count: {session['event_count']}")
    
    # 9. List all agents
    print("\n9. Listing all agents...")
    response = requests.get(
        f"{BASE_URL}/agents",
        headers=headers
    )
    agents = response.json()
    print(f"   Total agents: {agents['total']}")
    for agent in agents['agents']:
        print(f"   - {agent['agent_name']} ({agent['agent_type']})")
    
    # 10. Close session
    print("\n10. Closing session...")
    response = requests.delete(
        f"{BASE_URL}/sessions/{session_id}",
        headers=headers
    )
    print(f"   Status: {response.status_code}")
    
    # 11. Refresh token
    print("\n11. Refreshing access token...")
    response = requests.post(
        f"{BASE_URL}/auth/refresh",
        json={"refresh_token": refresh_token}
    )
    new_tokens = response.json()
    print(f"   New access token: {new_tokens['access_token'][:20]}...")
    
    # 12. Logout
    print("\n12. Logging out...")
    response = requests.post(
        f"{BASE_URL}/auth/logout",
        headers=headers,
        json={"refresh_token": refresh_token}
    )
    print(f"   Status: {response.status_code}")
    
    print("\n✅ Complete workflow finished successfully!")


if __name__ == "__main__":
    try:
        example_complete_workflow()
    except requests.exceptions.ConnectionError:
        print("❌ Error: Could not connect to API server.")
        print("   Make sure the server is running: uvicorn api.main:app --reload")
    except Exception as e:
        print(f"❌ Error: {e}")
