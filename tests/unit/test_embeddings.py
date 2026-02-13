"""Tests for embedding service."""

import pytest

from core.context.embeddings import EmbeddingService


@pytest.fixture
def db(request):
    """Database fixture with cleanup."""
    from api.database import get_db_session
    from sqlalchemy import text
    
    database = next(get_db_session())

    yield database

    # Cleanup
    database.execute(text("DELETE FROM event_embeddings WHERE event_id LIKE 'test_%'"))
    database.commit()


def test_mock_embedding_deterministic(db):
    """Test mock embeddings are deterministic."""
    service = EmbeddingService(db, provider="mock")

    text = "Hello world"
    emb1 = service.embed_text(text)
    emb2 = service.embed_text(text)

    assert emb1 == emb2
    assert len(emb1) == 1024


def test_mock_embedding_different_texts(db):
    """Test different texts produce different embeddings."""
    service = EmbeddingService(db, provider="mock")

    emb1 = service.embed_text("Hello world")
    emb2 = service.embed_text("Goodbye world")

    assert emb1 != emb2


def test_store_and_retrieve_embedding(db):
    """Test storing and retrieving embeddings."""
    service = EmbeddingService(db, provider="mock")

    event_id = "test_event_001"
    text = "Test content"
    embedding = service.embed_text(text)

    # Store
    service.store_embedding(event_id, embedding, metadata={"test": True})

    # Retrieve
    from sqlalchemy import text
    row = db.execute(
        text("SELECT model_name, model_version, metadata FROM event_embeddings WHERE event_id = :event_id"),
        {"event_id": event_id},
    ).fetchone()

    assert row is not None
    assert row.model_name == "text-embedding-3-small"

    import json

    metadata = json.loads(row.metadata)
    assert metadata["test"] is True


def test_cosine_similarity(db):
    """Test L2 distance calculation."""
    service = EmbeddingService(db, provider="mock")

    from uuid import uuid4
    from api.database import get_db_session
    from api.repositories.session_repository import SessionRepository
    from api.repositories.event_repository import EventRepository

    db_session = next(get_db_session())
    session_repo = SessionRepository(db_session)
    event_repo = EventRepository(db_session)

    user_id = str(uuid4())
    session_id = str(uuid4())
    
    session = session_repo.create({
        "session_id": session_id,
        "user_id": user_id
    })

    # Create events
    event1_id = str(uuid4())
    event2_id = str(uuid4())
    
    event1 = event_repo.create({
        "event_id": event1_id,
        "user_id": user_id,
        "session_id": session_id,
        "event_type": "user_query",
        "content": "Hello world",
        "causal_chain_id": str(uuid4())
    })
    
    event2 = event_repo.create({
        "event_id": event2_id,
        "user_id": user_id,
        "session_id": session_id,
        "event_type": "user_query",
        "content": "Hello world",
        "causal_chain_id": str(uuid4())
    })

    # Same text should have distance = 0
    emb1 = service.embed_text("Hello world")
    emb2 = service.embed_text("Hello world")

    # Store both
    service.store_embedding(event1.event_id, emb1)
    service.store_embedding(event2.event_id, emb2)

    # Query with same embedding
    results = service.search_similar(emb1, limit=2, session_id=session.session_id)

    # Distance should be very small (near 0)
    assert len(results) == 2
    assert results[0]["distance"] < 0.001  # Nearly identical


def test_search_similar(db):
    """Test semantic search using L2_DISTANCE."""
    service = EmbeddingService(db, provider="mock")

    from uuid import uuid4
    from api.database import get_db_session
    from api.repositories.session_repository import SessionRepository
    from api.repositories.event_repository import EventRepository

    db_session = next(get_db_session())
    session_repo = SessionRepository(db_session)
    event_repo = EventRepository(db_session)

    user_id = str(uuid4())
    session_id = str(uuid4())
    
    session = session_repo.create({
        "session_id": session_id,
        "user_id": user_id
    })

    # Create events with different content
    contents = [
        "How to implement authentication in Python",
        "Database schema design best practices",
        "Python authentication tutorial",
    ]

    for content in contents:
        event_id = str(uuid4())
        event = event_repo.create({
            "event_id": event_id,
            "user_id": user_id,
            "session_id": session_id,
            "event_type": "user_query",
            "content": content,
            "causal_chain_id": str(uuid4())
        })

        # Generate and store embedding
        embedding = service.embed_text(content)
        service.store_embedding(event.event_id, embedding)

    # Search for similar events
    query = "authentication in Python"
    query_embedding = service.embed_text(query)
    results = service.search_similar(query_embedding, limit=3, session_id=session.session_id)

    assert len(results) == 3

    # First result should be most similar (smallest distance)
    assert "authentication" in results[0]["content"].lower()
    assert "python" in results[0]["content"].lower()

    # Results should be sorted by distance (ascending)
    assert results[0]["distance"] <= results[1]["distance"]
    assert results[1]["distance"] <= results[2]["distance"]


def test_search_with_json_extract_filter(db):
    """Test semantic search with JSON_EXTRACT metadata filtering."""
    service = EmbeddingService(db, provider="mock")

    from uuid import uuid4
    from api.database import get_db_session
    from api.repositories.session_repository import SessionRepository
    from api.repositories.event_repository import EventRepository

    db_session = next(get_db_session())
    session_repo = SessionRepository(db_session)
    event_repo = EventRepository(db_session)

    user_id = str(uuid4())
    session_id = str(uuid4())
    
    session = session_repo.create({
        "session_id": session_id,
        "user_id": user_id
    })

    # Create events with metadata
    event1_id = str(uuid4())
    event1 = event_repo.create({
        "event_id": event1_id,
        "user_id": user_id,
        "session_id": session_id,
        "event_type": "user_query",
        "content": "How to implement auth?",
        "causal_chain_id": str(uuid4())
    })

    event2_id = str(uuid4())
    event2 = event_repo.create({
        "event_id": event2_id,
        "user_id": user_id,
        "session_id": session_id,
        "event_type": "user_query",
        "content": "What's the weather?",
        "causal_chain_id": str(uuid4())
    })

    # Store embeddings with metadata
    emb1 = service.embed_text("How to implement auth?")
    service.store_embedding(event1.event_id, emb1, metadata={"category": "code"})

    emb2 = service.embed_text("What's the weather?")
    service.store_embedding(event2.event_id, emb2, metadata={"category": "general"})

    # Test: Filter by metadata using JSON_EXTRACT
    query_embedding = service.embed_text("authentication")
    results = service.search_similar(
        query_embedding, limit=10, session_id=session.session_id, filters={"category": "code"}
    )

    # Should only return event1 (code category)
    assert len(results) == 1
    assert results[0]["event_id"] == event1.event_id
