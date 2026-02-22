"""Task scheduler for auto-triggered workflows.

Task queue, execution engine, retry logic.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from enum import Enum
from typing import Any, Callable, Optional

logger = logging.getLogger(__name__)


class TaskStatus(str, Enum):
    """Task execution status."""
    PENDING = "pending"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    RETRYING = "retrying"
    CANCELLED = "cancelled"


@dataclass
class Task:
    """Scheduled task."""
    task_id: str
    rule_id: str
    event: dict
    action: Callable
    status: TaskStatus = TaskStatus.PENDING
    retry_count: int = 0
    max_retries: int = 3
    created_at: datetime = field(default_factory=datetime.now)
    started_at: Optional[datetime] = None
    completed_at: Optional[datetime] = None
    error: Optional[str] = None
    result: Any = None
    
    def to_dict(self) -> dict:
        """Serialize to dict."""
        return {
            "task_id": self.task_id,
            "rule_id": self.rule_id,
            "status": self.status.value,
            "retry_count": self.retry_count,
            "created_at": self.created_at.isoformat(),
            "started_at": self.started_at.isoformat() if self.started_at else None,
            "completed_at": self.completed_at.isoformat() if self.completed_at else None,
            "error": self.error,
        }


class TaskScheduler:
    """Schedule and execute tasks from triggered rules."""
    
    def __init__(self, max_concurrent: int = 10):
        """Initialize scheduler.
        
        Args:
            max_concurrent: Maximum concurrent tasks
        """
        self.max_concurrent = max_concurrent
        self.pending_tasks: asyncio.Queue = asyncio.Queue()
        self.active_tasks: dict[str, Task] = {}
        self.completed_tasks: dict[str, Task] = {}
        self.running = False
    
    async def schedule_task(
        self,
        rule_id: str,
        event: dict,
        action: Callable,
        task_id: Optional[str] = None,
    ) -> str:
        """Schedule task for execution.
        
        Args:
            rule_id: Trigger rule ID
            event: Triggering event
            action: Async callable to execute
            task_id: Optional task ID (auto-generated if not provided)
            
        Returns:
            Task ID
        """
        if not task_id:
            task_id = f"task_{datetime.now().timestamp()}"
        
        task = Task(
            task_id=task_id,
            rule_id=rule_id,
            event=event,
            action=action,
        )
        
        await self.pending_tasks.put(task)
        logger.info(f"Scheduled task: {task_id} (rule: {rule_id})")
        return task_id
    
    async def start(self) -> None:
        """Start scheduler (runs worker tasks).
        
        This should be called once at startup.
        """
        self.running = True
        logger.info("Scheduler started")
        
        # Start worker tasks
        workers = [
            asyncio.create_task(self._worker())
            for _ in range(self.max_concurrent)
        ]
        
        try:
            await asyncio.gather(*workers)
        except asyncio.CancelledError:
            logger.info("Scheduler stopped")
        finally:
            self.running = False
    
    async def stop(self) -> None:
        """Stop scheduler gracefully."""
        self.running = False
        logger.info("Stopping scheduler")
    
    async def _worker(self) -> None:
        """Worker task that processes pending tasks."""
        while self.running:
            try:
                # Get next task with timeout
                task = await asyncio.wait_for(
                    self.pending_tasks.get(),
                    timeout=1.0
                )
                
                await self._execute_task(task)
            
            except asyncio.TimeoutError:
                continue
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Worker error: {e}")
    
    async def _execute_task(self, task: Task) -> None:
        """Execute single task with retry logic.
        
        Args:
            task: Task to execute
        """
        self.active_tasks[task.task_id] = task
        
        try:
            task.status = TaskStatus.RUNNING
            task.started_at = datetime.now()
            
            # Execute action
            result = await task.action(task.event)
            
            task.status = TaskStatus.COMPLETED
            task.result = result
            task.completed_at = datetime.now()
            
            logger.info(f"Task completed: {task.task_id}")
        
        except Exception as e:
            logger.error(f"Task failed: {task.task_id}, error: {e}")
            
            if task.retry_count < task.max_retries:
                task.retry_count += 1
                task.status = TaskStatus.RETRYING
                task.error = str(e)
                
                # Re-queue for retry
                await self.pending_tasks.put(task)
                logger.info(f"Task retrying: {task.task_id} (attempt {task.retry_count})")
            else:
                task.status = TaskStatus.FAILED
                task.error = str(e)
                task.completed_at = datetime.now()
                logger.error(f"Task failed permanently: {task.task_id}")
        
        finally:
            # Move to completed if done
            if task.status in (TaskStatus.COMPLETED, TaskStatus.FAILED):
                self.completed_tasks[task.task_id] = task
                self.active_tasks.pop(task.task_id, None)
    
    def get_task(self, task_id: str) -> Optional[Task]:
        """Get task by ID.
        
        Args:
            task_id: Task ID
            
        Returns:
            Task or None
        """
        return self.active_tasks.get(task_id) or self.completed_tasks.get(task_id)
    
    def get_stats(self) -> dict:
        """Get scheduler statistics.
        
        Returns:
            Stats dict
        """
        return {
            "running": self.running,
            "pending_tasks": self.pending_tasks.qsize(),
            "active_tasks": len(self.active_tasks),
            "completed_tasks": len(self.completed_tasks),
            "max_concurrent": self.max_concurrent,
        }
