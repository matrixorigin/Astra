"""Unit tests for ReplayService

测试策略：
1. 测试正常流程（Happy Path）
2. 测试边界条件（Edge Cases）
3. 测试错误处理（Error Handling）
4. 测试权限控制（Permission Control）
5. 测试数据完整性（Data Integrity）

测试覆盖：
- replay_session: 会话重放的各种场景
- compare_outputs: 输出对比的各种情况
- _replay_event: 单个事件重放的逻辑
"""

import pytest
from datetime import datetime, timezone
from uuid import uuid4

from api.services.replay_service import ReplayService
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError
from api.repositories.user_repository import UserRepository


@pytest.fixture
def test_user(db_session):
    """测试用户 fixture — worker-isolated"""
    repo = UserRepository(lambda: db_session)
    
    # Use unique username per test run
    uid = uuid4().hex
    username = f"replaytest_{uid}"
    
    # 创建
    from core.auth.password import hash_password
    user_data = {
        "user_id": str(uuid4()),
        "username": username,
        "email": f"replaytest_{uid}@example.com",
        "password_hash": hash_password("password123"),
        "is_active": 1,
    }
    user = repo.create(user_data)
    db_session.commit()
    
    yield user
    
    # 清理
    try:
        repo.delete(user.user_id)
        db_session.commit()
    except Exception:
        db_session.rollback()


@pytest.fixture
def replay_service(db_session):
    """ReplayService fixture"""
    return ReplayService(lambda: db_session)


@pytest.fixture
def test_session_with_events(db_session, test_user):
    """创建包含事件的测试会话"""
    from api.repositories import SessionRepository, EventRepository
    
    session_repo = SessionRepository(lambda: db_session)
    event_repo = EventRepository(lambda: db_session)
    
    # 创建会话
    session_data = {
        "session_id": str(uuid4()),
        "user_id": test_user.user_id,
        "status": "active",
        "event_count": 0
    }
    session = session_repo.create(session_data)
    
    # 创建事件
    events = []
    for i in range(3):
        event_data = {
            "event_id": str(uuid4()),
            "user_id": test_user.user_id,
            "session_id": session.session_id,
            "event_type": "user_query" if i % 2 == 0 else "llm_response",
            "content": f"Test content {i}",
            "causal_chain_id": str(uuid4())
        }
        event = event_repo.create(event_data)
        events.append(event)
    
    db_session.commit()
    
    yield {"session": session, "events": events}
    
    # 清理
    for event in events:
        event_repo.delete(event.event_id)
    session_repo.delete(session.session_id)
    db_session.commit()


class TestReplaySession:
    """测试 replay_session 方法"""
    
    def test_replay_session_success_mock_mode(self, replay_service, test_user, test_session_with_events):
        """测试成功重放会话（mock 模式）
        
        验证点：
        1. 返回正确的重放ID和会话ID
        2. 重放的事件数量正确
        3. 所有事件都成功重放
        4. Mock 模式标记正确
        """
        session = test_session_with_events["session"]
        
        result = replay_service.replay_session(
            session_id=session.session_id,
            user_id=test_user.user_id,
            mock_mode=True
        )
        
        # 验证基本信息
        assert "replay_id" in result
        assert result["session_id"] == session.session_id
        assert result["status"] == "completed"
        assert result["mock_mode"] is True
        
        # 验证事件数量
        assert result["events_replayed"] == 3
        assert result["result"]["total"] == 3
        assert result["result"]["successful"] == 3
        assert result["result"]["failed"] == 0
        
        # 验证事件详情
        events = result["result"]["events"]
        assert len(events) == 3
        for event in events:
            assert event["success"] is True
            assert "content" in event
    
    def test_replay_session_not_found(self, replay_service, test_user):
        """测试重放不存在的会话
        
        验证点：
        1. 抛出 ResourceNotFoundError
        2. 错误消息包含会话ID
        """
        with pytest.raises(ResourceNotFoundError) as exc_info:
            replay_service.replay_session(
                session_id="nonexistent",
                user_id=test_user.user_id
            )
        
        assert "nonexistent" in str(exc_info.value)
    
    def test_replay_session_permission_denied(self, replay_service, test_session_with_events):
        """测试无权限重放会话
        
        验证点：
        1. 抛出 PermissionDeniedError
        2. 错误消息包含会话ID
        """
        session = test_session_with_events["session"]
        other_user_id = str(uuid4())
        
        with pytest.raises(PermissionDeniedError) as exc_info:
            replay_service.replay_session(
                session_id=session.session_id,
                user_id=other_user_id
            )
        
        assert session.session_id in str(exc_info.value)
    
    def test_replay_session_with_sandbox(self, replay_service, test_user, test_session_with_events):
        """测试在沙箱中重放会话
        
        验证点：
        1. 沙箱名称正确记录
        2. 重放成功完成
        """
        session = test_session_with_events["session"]
        
        result = replay_service.replay_session(
            session_id=session.session_id,
            user_id=test_user.user_id,
            sandbox_name="test_sandbox",
            mock_mode=True
        )
        
        assert result["sandbox_name"] == "test_sandbox"
        assert result["status"] == "completed"
    
    def test_replay_session_empty_session(self, replay_service, test_user, db_session):
        """测试重放空会话（无事件）
        
        验证点：
        1. 重放成功完成
        2. 事件数量为0
        3. 成功和失败数量都为0
        """
        from api.repositories import SessionRepository
        
        session_repo = SessionRepository(lambda: db_session)
        session_data = {
            "session_id": str(uuid4()),
            "user_id": test_user.user_id,
            "status": "active",
            "event_count": 0
        }
        session = session_repo.create(session_data)
        db_session.commit()
        
        try:
            result = replay_service.replay_session(
                session_id=session.session_id,
                user_id=test_user.user_id
            )
            
            assert result["events_replayed"] == 0
            assert result["result"]["total"] == 0
            assert result["result"]["successful"] == 0
            assert result["result"]["failed"] == 0
        finally:
            session_repo.delete(session.session_id)
            db_session.commit()


class TestCompareOutputs:
    """测试 compare_outputs 方法"""
    
    def test_compare_outputs_perfect_match(self, replay_service, test_user, test_session_with_events):
        """测试完全匹配的对比
        
        验证点：
        1. match 为 True
        2. 事件数量相同
        3. 差异数量为0
        """
        session = test_session_with_events["session"]
        
        # 先重放
        replay_result = replay_service.replay_session(
            session_id=session.session_id,
            user_id=test_user.user_id,
            mock_mode=True
        )
        
        # 对比
        comparison = replay_service.compare_outputs(
            session_id=session.session_id,
            user_id=test_user.user_id,
            replay_result=replay_result["result"]
        )
        
        assert comparison["match"] is True
        assert comparison["original_event_count"] == 3
        assert comparison["replay_event_count"] == 3
        assert comparison["difference"] == 0
        assert comparison["mismatched_events"] == 0
    
    def test_compare_outputs_permission_denied(self, replay_service, test_session_with_events):
        """测试无权限对比输出
        
        验证点：
        1. 抛出 PermissionDeniedError
        """
        session = test_session_with_events["session"]
        other_user_id = str(uuid4())
        
        with pytest.raises(PermissionDeniedError):
            replay_service.compare_outputs(
                session_id=session.session_id,
                user_id=other_user_id,
                replay_result={"events": []}
            )
    
    def test_compare_outputs_with_differences(self, replay_service, test_user, test_session_with_events):
        """测试有差异的对比
        
        验证点：
        1. match 为 False
        2. 正确识别差异数量
        3. details 包含差异信息
        """
        session = test_session_with_events["session"]
        
        # 构造有差异的重放结果
        fake_replay_result = {
            "events": [
                {
                    "event_id": "fake1",
                    "event_type": "user_query",
                    "replayed_content": "Different content",
                    "success": True
                },
                {
                    "event_id": "fake2",
                    "event_type": "llm_response",
                    "replayed_content": "Different response",
                    "success": True
                },
                {
                    "event_id": "fake3",
                    "event_type": "user_query",
                    "replayed_content": "Another different content",
                    "success": True
                }
            ],
            "total": 3
        }
        
        comparison = replay_service.compare_outputs(
            session_id=session.session_id,
            user_id=test_user.user_id,
            replay_result=fake_replay_result
        )
        
        assert comparison["match"] is False
        assert comparison["mismatched_events"] > 0
        assert len(comparison["details"]) > 0
        
        # 验证 details 结构
        for detail in comparison["details"]:
            assert "event_index" in detail
            assert "event_id" in detail
            assert "event_type" in detail
            assert "original" in detail
            assert "replayed" in detail
            assert detail["match"] is False


class TestReplayEvent:
    """测试 _replay_event 方法（内部方法）"""
    
    def test_replay_event_mock_mode(self, replay_service):
        """测试 mock 模式下的事件重放
        
        验证点：
        1. 返回原始内容
        2. success 为 True
        """
        # 创建模拟事件对象
        class MockEvent:
            event_id = "test_event_1"
            event_type = "user_query"
            content = "Test content"
            session_id = "test_session"
            created_at = datetime.now(timezone.utc)
        
        event = MockEvent()
        
        result = replay_service._replay_event(
            event=event,
            mock_mode=True,
            skill_version_override=None
        )
        
        assert result["success"] is True
        assert result["content"] == "Test content"
    
    def test_replay_event_different_types(self, replay_service):
        """测试不同事件类型的重放
        
        验证点：
        1. user_query 正确处理
        2. llm_response 正确处理
        """
        class MockEvent:
            event_id = "test_event"
            content = "Test content"
            session_id = "test_session"
            created_at = datetime.now(timezone.utc)
        
        # 测试 user_query
        event = MockEvent()
        event.event_type = "user_query"
        result = replay_service._replay_event(event, True, None)
        assert result["success"] is True
        assert result["content"] == "Test content"
        
        # 测试 llm_response
        event.event_type = "llm_response"
        result = replay_service._replay_event(event, True, None)
        assert result["success"] is True
        assert result["content"] == "Test content"
