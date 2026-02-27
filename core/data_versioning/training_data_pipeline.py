"""Training data extraction pipeline — P2 Data Versioning.

Branch agent_events, extract with full lineage and contamination detection.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from typing import Optional

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.sandbox import Branch
from core.db_consumer import DbConsumer, DbFactory


class DatasetStatus(str, Enum):
    """Dataset lifecycle status."""
    DRAFT = "draft"
    EXTRACTING = "extracting"
    READY = "ready"
    ARCHIVED = "archived"


@dataclass
class DatasetLineage:
    """Full lineage for training dataset."""
    dataset_id: str
    source_branch: str
    source_table: str
    extraction_query: str
    extracted_at: datetime
    row_count: int
    metadata: dict = field(default_factory=dict)


@dataclass
class TrainingExample:
    """Single training example with full lineage."""
    example_id: str
    session_id: str
    event_id: str
    input_text: str
    output_text: str
    skill_name: str
    quality_score: float
    lineage: DatasetLineage


@dataclass
class DatasetConfig:
    """Configuration for training dataset extraction."""
    dataset_id: str
    name: str
    description: str
    source_table: str = "agent_events"
    filters: dict = field(default_factory=dict)  # e.g., {"skill_name": "sql_generator"}
    quality_threshold: float = 0.75
    sample_size: Optional[int] = None
    metadata: dict = field(default_factory=dict)


class TrainingDataPipeline(DbConsumer):
    """Extract training data with full lineage tracking and contamination detection."""
    
    def __init__(self, db_factory: DbFactory, source_db: str = "dev_agent"):
        """Initialize training data pipeline.
        
        Args:
            db_factory: Callable that returns a DB session.
            source_db: Source database for branching
        """
        super().__init__(db_factory)
        self.branch = Branch(db_factory, database=source_db)
        self.source_db = source_db
    
    def create_dataset(self, config: DatasetConfig) -> str:
        """Create training dataset with branched agent_events.
        
        Args:
            config: Dataset configuration
            
        Returns:
            Dataset ID
        """
        with self._db() as db:
            dataset_id = config.dataset_id
            branch_name = f"dataset_{dataset_id}"
        
            # 1. Create branch for dataset (zero-copy)
            self.branch.create(
                name=f"{branch_name}.{config.source_table}",
                source=f"{self.source_db}.{config.source_table}",
            )
        
            # 2. Create dataset_config table in branch
            db.execute(text(f"""
                CREATE TABLE {branch_name}.dataset_config (
                    dataset_id VARCHAR(255) PRIMARY KEY,
                    name VARCHAR(255) NOT NULL,
                    description TEXT,
                    source_table VARCHAR(255) NOT NULL,
                    filters JSON,
                    quality_threshold DECIMAL(3,2) DEFAULT 0.75,
                    sample_size INT NULL,
                    status VARCHAR(50) DEFAULT 'draft',
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    extracted_at TIMESTAMP NULL,
                    exported_at TIMESTAMP NULL,
                    archived_at TIMESTAMP NULL,
                    export_path VARCHAR(1024) NULL,
                    metadata JSON
                )
            """))
        
            # 3. Create lineage tracking table
            db.execute(text(f"""
                CREATE TABLE {branch_name}.extraction_lineage (
                    lineage_id INT AUTO_INCREMENT PRIMARY KEY,
                    dataset_id VARCHAR(255) NOT NULL,
                    example_id VARCHAR(255) NOT NULL,
                    session_id VARCHAR(255) NOT NULL,
                    event_id VARCHAR(255) NOT NULL,
                    source_event_id VARCHAR(255) NOT NULL,
                    extraction_timestamp TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    metadata JSON,
                    INDEX idx_dataset (dataset_id),
                    INDEX idx_example (example_id),
                    INDEX idx_session (session_id)
                )
            """))
        
            # 4. Store config
            db.execute(text(f"""
                INSERT INTO {branch_name}.dataset_config
                (dataset_id, name, description, source_table, filters, quality_threshold, sample_size, status, metadata)
                VALUES (:dataset_id, :name, :desc, :source_table, :filters, :quality_threshold, :sample_size, :status, :metadata)
            """), {
                "dataset_id": dataset_id,
                "name": config.name,
                "desc": config.description,
                "source_table": config.source_table,
                "filters": json.dumps(config.filters),
                "quality_threshold": config.quality_threshold,
                "sample_size": config.sample_size,
                "status": DatasetStatus.DRAFT.value,
                "metadata": json.dumps(config.metadata),
            })
            db.commit()
        
            return dataset_id
    
    def extract_examples(
        self,
        dataset_id: str,
        quality_threshold: float = 0.75,
        limit: Optional[int] = None,
    ) -> list[TrainingExample]:
        """Extract training examples from dataset with full lineage.
        
        Args:
            dataset_id: Dataset ID
            quality_threshold: Minimum quality score
            limit: Max examples to extract
            
        Returns:
            List of TrainingExample with lineage
        """
        with self._db() as db:
            branch_name = f"dataset_{dataset_id}"
        
            # Build query with quality filter
            query = f"""
                SELECT 
                    event_id,
                    session_id,
                    JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.skill_name')) as skill_name,
                    content,
                    CAST(JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.quality_score')) AS DECIMAL(3,2)) as quality_score,
                    created_at
                FROM {branch_name}.agent_events
                WHERE event_type = 'llm_response'
                AND CAST(JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.quality_score')) AS DECIMAL(3,2)) >= :quality_threshold
            """
        
            if limit:
                query += f" LIMIT {limit}"
        
            rows = db.execute(text(query), {"quality_threshold": quality_threshold}).fetchall()
        
            examples = []
            for row in rows:
                event_id, session_id, skill_name, content, quality_score, created_at = row
            
                # Get input from previous user event
                input_row = db.execute(text(f"""
                    SELECT content
                    FROM {branch_name}.agent_events
                    WHERE session_id = :session_id
                    AND event_type = 'user_query'
                    AND created_at < :created_at
                    ORDER BY created_at DESC
                    LIMIT 1
                """), {"session_id": session_id, "created_at": created_at}).fetchone()
            
                input_text = input_row[0] if input_row else ""
            
                # Record lineage
                db.execute(text(f"""
                    INSERT INTO {branch_name}.extraction_lineage
                    (dataset_id, example_id, session_id, event_id, source_event_id, metadata)
                    VALUES (:dataset_id, :example_id, :session_id, :event_id, :source_event_id, :metadata)
                """), {
                    "dataset_id": dataset_id,
                    "example_id": f"{dataset_id}_{event_id}",
                    "session_id": session_id,
                    "event_id": event_id,
                    "source_event_id": event_id,
                    "metadata": json.dumps({
                        "created_at": created_at.isoformat(),
                        "quality_score": float(quality_score or 0.0),
                    }),
                })
            
                lineage = DatasetLineage(
                    dataset_id=dataset_id,
                    source_branch=branch_name,
                    source_table="agent_events",
                    extraction_query=query,
                    extracted_at=datetime.utcnow(),
                    row_count=len(rows),
                    metadata={
                        "event_id": event_id,
                        "session_id": session_id,
                        "created_at": created_at.isoformat(),
                    },
                )
            
                examples.append(TrainingExample(
                    example_id=f"{dataset_id}_{event_id}",
                    session_id=session_id,
                    event_id=event_id,
                    input_text=input_text,
                    output_text=content,
                    skill_name=skill_name or "unknown",
                    quality_score=float(quality_score or 0.0),
                    lineage=lineage,
                ))
        
            db.commit()
            return examples
    
    def export_dataset(
        self,
        dataset_id: str,
        format: str = "jsonl",
        output_path: Optional[str] = None,
    ) -> str:
        """Export dataset to file with lineage metadata.
        
        Args:
            dataset_id: Dataset ID
            format: Export format (jsonl, csv, parquet)
            output_path: Output file path
            
        Returns:
            Path to exported file
        """
        with self._db() as db:
            branch_name = f"dataset_{dataset_id}"
        
            if output_path is None:
                output_path = f"/tmp/{dataset_id}.{format}"
        
            # Export based on format
            if format == "jsonl":
                examples = self.extract_examples(dataset_id)
                with open(output_path, "w") as f:
                    for ex in examples:
                        record = {
                            "example_id": ex.example_id,
                            "input": ex.input_text,
                            "output": ex.output_text,
                            "skill": ex.skill_name,
                            "quality": ex.quality_score,
                            "lineage": {
                                "dataset_id": ex.lineage.dataset_id,
                                "session_id": ex.session_id,
                                "event_id": ex.event_id,
                                "extracted_at": ex.lineage.extracted_at.isoformat(),
                            },
                        }
                        f.write(json.dumps(record) + "\n")
        
            # Update status
            db.execute(text(f"""
                UPDATE {branch_name}.dataset_config
                SET status = :status, exported_at = CURRENT_TIMESTAMP, export_path = :export_path
                WHERE dataset_id = :dataset_id
            """), {
                "status": DatasetStatus.READY.value,
                "export_path": output_path,
                "dataset_id": dataset_id,
            })
            db.commit()
        
            return output_path
    
    def detect_contamination(
        self,
        dataset_id: str,
        test_session_ids: list[str],
    ) -> dict[str, bool]:
        """Detect if test sessions are in training data (data leakage).
        
        Args:
            dataset_id: Dataset ID
            test_session_ids: Session IDs to check
            
        Returns:
            Dict mapping session_id to contamination status
        """
        with self._db() as db:
            branch_name = f"dataset_{dataset_id}"
        
            contamination = {}
            for session_id in test_session_ids:
                result = db.execute(text(f"""
                    SELECT COUNT(*) as count
                    FROM {branch_name}.agent_events
                    WHERE session_id = :session_id
                """), {"session_id": session_id}).fetchone()
            
                contamination[session_id] = (result[0] if result else 0) > 0
        
            return contamination
    
    def get_lineage_chain(
        self,
        dataset_id: str,
        example_id: str,
    ) -> list[dict]:
        """Get full lineage chain for example (source → extraction → export).
        
        Args:
            dataset_id: Dataset ID
            example_id: Example ID
            
        Returns:
            List of lineage events
        """
        with self._db() as db:
            branch_name = f"dataset_{dataset_id}"
        
            rows = db.execute(text(f"""
                SELECT lineage_id, example_id, session_id, event_id, source_event_id, 
                       extraction_timestamp, metadata
                FROM {branch_name}.extraction_lineage
                WHERE dataset_id = :dataset_id
                AND example_id = :example_id
                ORDER BY extraction_timestamp
            """), {"dataset_id": dataset_id, "example_id": example_id}).fetchall()
        
            return [
                {
                    "lineage_id": row[0],
                    "example_id": row[1],
                    "session_id": row[2],
                    "event_id": row[3],
                    "source_event_id": row[4],
                    "extraction_timestamp": row[5].isoformat() if row[5] else None,
                    "metadata": json.loads(row[6]) if isinstance(row[6], str) else row[6] or {},
                }
                for row in rows
            ]
    
    def cleanup_dataset(self, dataset_id: str) -> None:
        """Archive dataset and cleanup branch.
        
        Args:
            dataset_id: Dataset ID
        """
        with self._db() as db:
            branch_name = f"dataset_{dataset_id}"
        
            # Update status
            db.execute(text(f"""
                UPDATE {branch_name}.dataset_config
                SET status = :status, archived_at = CURRENT_TIMESTAMP
                WHERE dataset_id = :dataset_id
            """), {"status": DatasetStatus.ARCHIVED.value, "dataset_id": dataset_id})
            db.commit()
        
            # Delete branch
            self.branch.delete(f"{branch_name}.agent_events")
