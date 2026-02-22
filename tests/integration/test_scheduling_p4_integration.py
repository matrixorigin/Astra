"""P4 Auto-Scheduling integration tests — Real database events, no blocking."""

import asyncio

import pytest
from sqlalchemy.orm import Session

from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from core.scheduling import (
    Condition,
    ConditionLogic,
    ConditionOperator,
    TaskScheduler,
    TaskStatus,
    TriggerRule,
    TriggerRuleRegistry,
    WorkflowDefinition,
    WorkflowEngine,
    WorkflowExecution,
    WorkflowStep,
    WorkflowStatus,
)


class TestAutoSchedulingIntegration:
    """Integration tests with real database events."""
    
    def test_trigger_rule_matches_database_event(self, db: Session):
        """Test trigger rule matching real database event.
        
        Real scenario: User sends message → Event logged → Trigger rule matches
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="alice")
        
        # Create trigger rule: Fire when user_query contains "urgent"
        rule = TriggerRule(
            rule_id="urgent_rule",
            name="Urgent Query Handler",
            description="Handle urgent queries",
            event_type="user_query",
            conditions=[
                Condition("data.content", ConditionOperator.CONTAINS, "urgent"),
            ],
        )
        
        # Log event that matches rule
        db_event = logger.create_user_query(
            user_id="alice",
            session_id=session.session_id,
            content="This is urgent!",
        )
        
        # Convert to dict for rule matching
        event_dict = {
            "event_type": "user_query",
            "data": {"content": db_event.content},
        }
        
        # Verify rule matches
        assert rule.matches(event_dict) is True
    
    def test_trigger_registry_with_database_events(self, db: Session):
        """Test trigger registry finding matching rules for database events.
        
        Real scenario: Multiple rules registered → Event occurs → Find matching rules
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="bob")
        
        # Create registry and rules
        registry = TriggerRuleRegistry()
        
        rule1 = TriggerRule(
            rule_id="rule1",
            name="High Priority",
            description="High priority queries",
            event_type="user_query",
            conditions=[
                Condition("data.priority", ConditionOperator.EQ, "high"),
            ],
        )
        
        rule2 = TriggerRule(
            rule_id="rule2",
            name="Error Handler",
            description="Handle errors",
            event_type="error",
        )
        
        registry.register_rule(rule1)
        registry.register_rule(rule2)
        
        # Log event
        db_event = logger.create_user_query(
            user_id="bob",
            session_id=session.session_id,
            content="High priority task",
        )
        
        # Create event dict
        event_dict = {
            "event_type": "user_query",
            "data": {"priority": "high"},
        }
        
        # Find matching rules
        matching = registry.find_matching_rules(event_dict)
        
        # Verify
        assert len(matching) == 1
        assert matching[0].rule_id == "rule1"
    
    @pytest.mark.asyncio
    async def test_task_scheduler_with_database_context(self, db: Session):
        """Test task scheduler executing action with database context.
        
        Real scenario: Event triggers → Task scheduled → Action executes with DB context
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="charlie")
        
        # Create scheduler
        scheduler = TaskScheduler(max_concurrent=1)
        
        # Track action execution
        action_executed = False
        action_event = None
        
        async def process_urgent_query(event):
            """Real action: Process urgent query."""
            nonlocal action_executed, action_event
            action_executed = True
            action_event = event
            return f"Processed: {event['data']['content']}"
        
        # Log event
        db_event = logger.create_user_query(
            user_id="charlie",
            session_id=session.session_id,
            content="Urgent: Fix the bug!",
        )
        
        # Create event dict
        event_dict = {
            "event_type": "user_query",
            "data": {"content": db_event.content},
        }
        
        # Schedule task
        task_id = await scheduler.schedule_task(
            rule_id="urgent_rule",
            event=event_dict,
            action=process_urgent_query,
        )
        
        # Execute task
        task = await scheduler.pending_tasks.get()
        await scheduler._execute_task(task)
        
        # Verify
        assert action_executed is True
        assert action_event["data"]["content"] == "Urgent: Fix the bug!"
        assert task.status == TaskStatus.COMPLETED
        assert "Processed" in task.result
    
    @pytest.mark.asyncio
    async def test_workflow_with_database_session_context(self, db: Session):
        """Test workflow execution with real database session context.
        
        Real scenario: Multi-step workflow → Each step uses session data → Results chain
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="diana")
        
        # Create workflow
        workflow = WorkflowDefinition(
            workflow_id="analysis_workflow",
            name="Query Analysis Workflow",
            description="Analyze user query and generate response",
        )
        
        # Step 1: Extract query intent
        async def extract_intent(input_data):
            context = input_data["context"]
            return {"intent": "question", "domain": "technical"}
        
        # Step 2: Find relevant sessions (depends on step 1)
        async def find_relevant_sessions(input_data):
            results = input_data["results"]
            intent = results.get("extract_intent", {}).get("intent")
            return {"relevant_sessions": 3, "intent": intent}
        
        # Step 3: Generate response (depends on step 2)
        async def generate_response(input_data):
            results = input_data["results"]
            sessions = results.get("find_relevant_sessions", {}).get("relevant_sessions", 0)
            return {"response": f"Based on {sessions} relevant sessions..."}
        
        step1 = WorkflowStep(
            step_id="extract_intent",
            name="Extract Intent",
            action=extract_intent,
        )
        
        step2 = WorkflowStep(
            step_id="find_relevant_sessions",
            name="Find Relevant Sessions",
            action=find_relevant_sessions,
            depends_on=["extract_intent"],
        )
        
        step3 = WorkflowStep(
            step_id="generate_response",
            name="Generate Response",
            action=generate_response,
            depends_on=["find_relevant_sessions"],
        )
        
        workflow.add_step(step1)
        workflow.add_step(step2)
        workflow.add_step(step3)
        
        # Execute workflow with real session context
        execution = WorkflowExecution(
            workflow,
            context={"session_id": session.session_id, "user_id": "diana"},
        )
        
        result = await execution.execute()
        
        # Verify
        assert result is True
        assert execution.status == WorkflowStatus.COMPLETED
        assert "extract_intent" in execution.step_results
        assert "find_relevant_sessions" in execution.step_results
        assert "generate_response" in execution.step_results
        
        # Verify dependency chain
        assert execution.step_results["extract_intent"]["intent"] == "question"
        assert execution.step_results["find_relevant_sessions"]["intent"] == "question"
        assert "relevant sessions" in execution.step_results["generate_response"]["response"]


class TestAutoSchedulingRealWorldIntegration:
    """Real-world auto-scheduling integration scenarios."""
    
    def test_multi_turn_conversation_triggers(self, db: Session):
        """Test multiple triggers across multi-turn conversation.
        
        Real scenario: 3-turn conversation → Different rules trigger → Different actions
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="eve")
        
        # Create registry with multiple rules
        registry = TriggerRuleRegistry()
        
        rule_urgent = TriggerRule(
            rule_id="urgent_rule",
            name="Urgent Handler",
            description="Handle urgent",
            event_type="user_query",
            conditions=[
                Condition("data.content", ConditionOperator.CONTAINS, "urgent"),
            ],
        )
        
        rule_analysis = TriggerRule(
            rule_id="analysis_rule",
            name="Analysis Handler",
            description="Handle analysis",
            event_type="user_query",
            conditions=[
                Condition("data.content", ConditionOperator.CONTAINS, "analyze"),
            ],
        )
        
        registry.register_rule(rule_urgent)
        registry.register_rule(rule_analysis)
        
        # Simulate 3-turn conversation
        turns = [
            "urgent: Fix the bug!",
            "Please analyze this data",
            "Normal question",
        ]
        
        triggered_rules = []
        
        for msg in turns:
            # Log event
            db_event = logger.create_user_query(
                user_id="eve",
                session_id=session.session_id,
                content=msg,
            )
            
            # Create event dict
            event_dict = {
                "event_type": "user_query",
                "data": {"content": db_event.content},
            }
            
            # Find matching rules
            matching = registry.find_matching_rules(event_dict)
            triggered_rules.append(len(matching))
        
        # Verify
        assert triggered_rules[0] == 1  # "urgent" matches urgent_rule
        assert triggered_rules[1] == 1  # "analyze" matches analysis_rule
        assert triggered_rules[2] == 0  # "Normal" matches no rules
    
    @pytest.mark.asyncio
    async def test_end_to_end_event_to_task_execution(self, db: Session):
        """Test complete flow: Event → Trigger → Task → Execution.
        
        Real scenario: User sends query → Triggers rule → Schedules task → Executes
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="frank")
        
        # Create trigger rule
        registry = TriggerRuleRegistry()
        rule = TriggerRule(
            rule_id="analysis_rule",
            name="Analysis Trigger",
            description="Trigger analysis",
            event_type="user_query",
            conditions=[
                Condition("data.content", ConditionOperator.CONTAINS, "analyze"),
            ],
        )
        registry.register_rule(rule)
        
        # Create scheduler
        scheduler = TaskScheduler(max_concurrent=1)
        
        # Track execution
        executed_actions = []
        
        async def analyze_action(event):
            executed_actions.append(event["data"]["content"])
            return "Analysis complete"
        
        # Log event
        db_event = logger.create_user_query(
            user_id="frank",
            session_id=session.session_id,
            content="Please analyze this data",
        )
        
        # Create event dict
        event_dict = {
            "event_type": "user_query",
            "data": {"content": db_event.content},
        }
        
        # Check if rule matches
        assert rule.matches(event_dict) is True
        
        # Schedule task
        task_id = await scheduler.schedule_task(
            rule_id="analysis_rule",
            event=event_dict,
            action=analyze_action,
        )
        
        # Execute task
        task = await scheduler.pending_tasks.get()
        await scheduler._execute_task(task)
        
        # Verify
        assert len(executed_actions) == 1
        assert "analyze" in executed_actions[0]
        assert task.status == TaskStatus.COMPLETED
    
    @pytest.mark.asyncio
    async def test_workflow_dependency_chain_with_database_data(self, db: Session):
        """Test workflow dependency chain using real database data.
        
        Real scenario: Step 1 queries DB → Step 2 uses result → Step 3 finalizes
        """
        # Setup
        session_mgr = SessionManager(db)
        logger = EventLogger(db)
        
        session = session_mgr.create_session(user_id="grace")
        
        # Create database events
        event1 = logger.create_user_query(
            user_id="grace",
            session_id=session.session_id,
            content="Query 1",
        )
        
        event2 = logger.create_user_query(
            user_id="grace",
            session_id=session.session_id,
            content="Query 2",
        )
        
        # Create workflow
        workflow = WorkflowDefinition(
            workflow_id="data_workflow",
            name="Data Processing Workflow",
            description="Process database data",
        )
        
        # Step 1: Load data
        async def load_data(input_data):
            return {"events_count": 2, "session_id": input_data["context"]["session_id"]}
        
        # Step 2: Process data (depends on step 1)
        async def process_data(input_data):
            results = input_data["results"]
            count = results.get("load_data", {}).get("events_count", 0)
            return {"processed_count": count * 2}
        
        # Step 3: Finalize (depends on step 2)
        async def finalize(input_data):
            results = input_data["results"]
            processed = results.get("process_data", {}).get("processed_count", 0)
            return {"final_result": f"Processed {processed} items"}
        
        step1 = WorkflowStep(
            step_id="load_data",
            name="Load Data",
            action=load_data,
        )
        
        step2 = WorkflowStep(
            step_id="process_data",
            name="Process Data",
            action=process_data,
            depends_on=["load_data"],
        )
        
        step3 = WorkflowStep(
            step_id="finalize",
            name="Finalize",
            action=finalize,
            depends_on=["process_data"],
        )
        
        workflow.add_step(step1)
        workflow.add_step(step2)
        workflow.add_step(step3)
        
        # Execute workflow
        execution = WorkflowExecution(
            workflow,
            context={"session_id": session.session_id},
        )
        
        result = await execution.execute()
        
        # Verify
        assert result is True
        assert execution.status == WorkflowStatus.COMPLETED
        assert execution.step_results["load_data"]["events_count"] == 2
        assert execution.step_results["process_data"]["processed_count"] == 4
        assert "4 items" in execution.step_results["finalize"]["final_result"]
