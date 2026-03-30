/* Graph model types matching the Rust backend structures */

/** Mirrors TaskStatus from task_orchestrator.rs */
export type TaskStatus =
  | 'pending'
  | 'in_progress'
  | 'paused'
  | 'completed'
  | 'failed'
  | 'cancelled';

/** Mirrors SubtaskPlan from task_orchestrator.rs */
export interface SubtaskPlan {
  id: string;
  title: string;
  description?: string;
  dependsOn: string[];
  status: TaskStatus;
  effort?: 'small' | 'medium' | 'large';
  files: string[];
  acceptance?: string;
}

/** Mirrors TaskPlan from task_orchestrator.rs */
export interface TaskPlan {
  subtasks: SubtaskPlan[];
  notes?: string;
}

/** Mirrors TaskRecord from task_orchestrator.rs */
export interface TaskRecord {
  taskId: string;
  title: string;
  sessionId?: string;
  parentTaskId?: string;
  plan?: TaskPlan;
  status: TaskStatus;
  progressPct: number;
  itemsDone: number;
  itemsTotal: number;
  createdAt: string;
  updatedAt: string;
  completedAt?: string;
}

/** Delegation event extracted from the event stream */
export interface DelegationEvent {
  id: string;
  fromAgentId: string;
  toAgentId: string;
  taskDescription: string;
  status: 'delegated' | 'in_progress' | 'completed' | 'failed';
  timestamp: string;
}

/** Plan progress event from JournalEventType::PlanProgress */
export interface PlanProgressEvent {
  subtaskId: string;
  subtaskTitle: string;
  action: 'started' | 'completed' | 'skipped' | 'plan_complete' | 'plan_paused';
  progressPct: number;
  totalSubtasks: number;
  completedSubtasks: number;
  timestamp: string;
}

/** Combined graph data for visualization */
export interface PlanGraphData {
  task: TaskRecord;
  delegations: DelegationEvent[];
  progressEvents: PlanProgressEvent[];
}
