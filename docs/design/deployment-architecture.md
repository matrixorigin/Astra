# Deployment Architecture

> **Last Updated**: 2026-02-21  
> **Scope**: All deployment topologies from single-machine to multi-node K8s cluster

---

## 1. System Components

mo-agent-engine consists of these runtime components:

| Component | Process | Stateless? | Scalable? | Description |
|-----------|---------|-----------|-----------|-------------|
| **API Server** | `uvicorn api.main:app` | ✅ Yes | Horizontal | REST API, JWT auth, rate limiting |
| **CLI (mo-agent)** | `mo-agent chat` | ✅ Yes | Per-user | Interactive chat, skill execution |
| **CLI (mo-admin)** | `mo-admin init/prompt/...` | ✅ Yes | Single | Admin operations |
| **MatrixOne** | `mo-service` | ❌ Stateful | Cluster | HTAP database, time-travel, branching |
| **Redis** | `redis-server` | ❌ Stateful | Cluster/Sentinel | Cache, rate limiting, pub/sub |
| **Skill Workers** | Skill execution processes | ✅ Yes | Horizontal | Heavy skill execution (training, etc.) |
| **Model Server** | `mo-agent model serve` | ✅ Yes | Horizontal | Shared inference for platform-trained small models (NOT LLMs) |

### Component Dependencies

```
                    ┌──────────────┐
                    │   Clients    │
                    │ (CLI / SDK)  │
                    └──────┬───────┘
                           │
                    ┌──────▼───────┐
                    │  API Server  │──────────────────┐
                    │  (FastAPI)   │                   │
                    └──────┬───────┘                   │
                           │                           │
              ┌────────────┼────────────┐              │
              │            │            │              │
       ┌──────▼──────┐ ┌──▼───┐ ┌──────▼──────┐ ┌────▼─────┐
       │  MatrixOne   │ │Redis │ │Skill Workers│ │Model Srv │
       │  (Database)  │ │      │ │ (Optional)  │ │(Optional)│
       └─────────────┘ └──────┘ └─────────────┘ └──────────┘
```

---

## 2. Deployment Topologies

### Topology 1: Single Machine (Development)

```
┌─────────────────────────────────────────────────────┐
│                   Single Machine                     │
│                                                      │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │
│  │MatrixOne │  │  Redis   │  │  mo-agent chat   │  │
│  │ (Docker) │  │ (Docker) │  │  (conda env)     │  │
│  │ :6001    │  │ :6379    │  │                   │  │
│  └──────────┘  └──────────┘  │  API Server       │  │
│                               │  (optional, :8000)│  │
│                               └──────────────────┘  │
└─────────────────────────────────────────────────────┘
```

**How to run**:
```bash
conda activate agent-engine
make dev-up                          # MatrixOne + Redis in Docker
mo-admin init                        # Init DB
mo-agent chat                        # Direct CLI usage
# OR
uvicorn api.main:app --port 8000     # API server (optional)
```

**Skill execution**: In-process, same Python process as CLI/API.  
**ML inference**: In-process, model loaded into same process.  
**GPU**: Local GPU if available, CPU fallback.

---

### Topology 2: Docker Compose (All-in-One)

一键拉起所有服务，零配置。

```
┌─────────────────────────────────────────────────────────────┐
│                    docker-compose up -d                       │
│                                                              │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────────┐  │
│  │MatrixOne │  │  Redis   │  │   Init   │→ │ API Server │  │
│  │ :6001    │  │ :6379    │  │ (run     │  │ :8000      │  │
│  │          │  │          │  │  once)   │  │ workers: 2 │  │
│  └──────────┘  └──────────┘  └──────────┘  └────────────┘  │
│                                                              │
│  ┌──────────────────────────┐  ┌─────────────────────────┐  │
│  │  Skill Worker [opt:gpu]  │  │ Model Server [opt:model]│  │
│  │  GPU training tasks      │  │ Shared ONNX inference   │  │
│  │  nvidia runtime          │  │ :9527                   │  │
│  └──────────────────────────┘  └─────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

**Usage**:
```bash
# Core only (MatrixOne + Redis + Init + API)
docker-compose up -d

# + GPU training worker
docker-compose --profile gpu up -d

# + Model inference server
docker-compose --profile model up -d

# Everything
docker-compose --profile full up -d
```

**Startup sequence**: MatrixOne + Redis → healthcheck pass → Init (schema + prompts) → API + optional services.

**Database connections**: The platform DB (MatrixOne) stores platform state. User BYOD connections are managed at runtime — each user's DB connection is pooled separately via `UserDBPool`. See [skill-as-package.md](skill-as-package.md) for the BYOD architecture.

**Skill execution**: Lightweight skills run in-process inside API. Heavy skills (training) dispatched to skill-worker container via Redis queue.  
**ML inference**: In-process by default. With `--profile model`, shared Model Server at `:9527`.  
**GPU**: `--profile gpu` enables nvidia runtime for skill-worker.

---

### Topology 3: Kubernetes (Production)

MatrixOne, Redis, Ray, GPU 全部可选 — 可以用集群外已有的实例。

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Kubernetes Cluster                            │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │  Namespace: mo-agent                                         │    │
│  │                                                               │    │
│  │  ┌─────────────────────┐                                     │    │
│  │  │ Deployment: api     │    Required                         │    │
│  │  │ Replicas: 2-10      │                                     │    │
│  │  │ HPA: CPU/RPS        │                                     │    │
│  │  └─────────────────────┘                                     │    │
│  │                                                               │    │
│  │  ┌─────────────────────┐    ┌──────────────────────────┐    │    │
│  │  │ StatefulSet:        │    │ StatefulSet: redis       │    │    │
│  │  │   matrixone [opt]   │    │ [opt]                    │    │    │
│  │  │ OR: external DB     │    │ OR: external Redis       │    │    │
│  │  └─────────────────────┘    └──────────────────────────┘    │    │
│  │                                                               │    │
│  │  ┌─────────────────────┐    ┌──────────────────────────┐    │    │
│  │  │ Deployment:         │    │ Job: skill-worker [opt]  │    │    │
│  │  │   model-server [opt]│    │ GPU node selector        │    │    │
│  │  └─────────────────────┘    └──────────────────────────┘    │    │
│  │                                                               │    │
│  │  ┌─────────────────────┐                                     │    │
│  │  │ RayCluster [opt]    │                                     │    │
│  │  │ GPU + CPU workers   │                                     │    │
│  │  └─────────────────────┘                                     │    │
│  └─────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────┘
```

**Helm values 控制可选组件**:
```yaml
# values.yaml
api:
  replicas: 2
  hpa:
    enabled: true
    maxReplicas: 10

# [opt] 内置 MatrixOne — 关闭则用外部 DB
matrixone:
  enabled: false  # Use external MatrixOne cluster
  external:
    host: matrixone.prod.internal
    port: 6001

# [opt] 内置 Redis — 关闭则用外部 Redis
redis:
  enabled: false
  external:
    url: redis://redis.prod.internal:6379

# [opt] Model Server — 关闭则 API 进程内推理
modelServer:
  enabled: true
  replicas: 1
  resources:
    requests:
      cpu: "1"
      memory: "1Gi"

# [opt] GPU Skill Worker — 关闭则无训练能力
skillWorker:
  enabled: true
  gpu:
    enabled: true
    nodeSelector:
      accelerator: nvidia-gpu

# [opt] Ray Cluster — 关闭则用 K8s Job 做分布式
ray:
  enabled: false
  workers:
    gpu:
      replicas: 2
      resources:
        nvidia.com/gpu: "1"
    cpu:
      minReplicas: 1
      maxReplicas: 8
```

**Usage**:
```bash
# Minimal: API only (external DB + Redis)
helm install mo-agent ./charts/mo-agent-engine \
  --set matrixone.enabled=false \
  --set matrixone.external.host=db.prod.internal \
  --set redis.enabled=false \
  --set redis.external.url=redis://redis.prod.internal:6379

# Full: everything in-cluster
helm install mo-agent ./charts/mo-agent-engine \
  --set matrixone.enabled=true \
  --set redis.enabled=true \
  --set modelServer.enabled=true \
  --set skillWorker.enabled=true \
  --set ray.enabled=true
```

---

## 3. Execution Model: Tools vs Background Jobs

### Two Distinct Execution Contexts

The system has two fundamentally different execution contexts that must NOT be conflated:

#### Context 1: Agent Tool Execution (Synchronous, In-Loop)

Agent tools are called during the ChatLoop decision cycle. They must be fast and return results
for the LLM to continue reasoning.

| Execution Path | Isolation | Latency | Example |
|---------------|-----------|---------|---------|
| **Built-in Skill** | None (in-process function call) | <1s | `code_review`, `search_code` |
| **MCP Tool** | Process-level (MCP server is a separate process via stdio/HTTP) | <5s | filesystem, database, SaaS tools |
| **Scratchpad** | None (in-memory) | <1ms | `scratchpad_write`, `scratchpad_read` |

**No tool-level containerization needed.** This matches industry practice:
- Claude Code / Kiro CLI: in-process function calls, permission-based safety
- Cursor: in-process, no isolation
- LangChain / CrewAI: in-process
- MCP: process-level isolation (separate server process), not security sandbox

Safety is handled by:
1. `SideEffectCategory` (READ/WRITE/DESTRUCTIVE) → approval gates
2. `ToolMockingLayer` → replay mode blocks destructive ops, records results
3. MCP tools → naturally isolated in separate process

#### Context 2: Background Jobs (Asynchronous, Out-of-Loop)

Heavy workloads that run outside the chat loop. These are NOT agent tools — they are
scheduled tasks triggered by API calls, cron, or events.

| Job | CPU | GPU | Memory | Duration | Trigger |
|-----|-----|-----|--------|----------|---------|
| `feedback_trainer` | 4 cores | ✅ 1 GPU | 8GB | 30min-2hr | Weekly / on-demand |
| `corpus_collector` | 0.5 core | ❌ | 500MB | 5-30min | On-demand |
| `drift_detection` | 1 core | ❌ | 1GB | 1-5min | Hourly |
| `model_evaluation` | 2 cores | ✅ | 4GB | 10-60min | On-demand |

These need a job execution backend for scheduling, progress tracking, and resource allocation.

### Background Job Backend (Pluggable)

Only applies to Context 2 (background jobs). Agent tool execution stays in-process.

```python
# core/jobs/backend.py
from abc import ABC, abstractmethod
from dataclasses import dataclass
from enum import Enum

class JobStatus(Enum):
    PENDING = "pending"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"

@dataclass
class JobResult:
    job_id: str
    status: JobStatus
    result: dict | None = None
    error: str | None = None
    progress: float = 0.0  # 0.0 - 1.0

@dataclass
class JobRequirements:
    """Resource requirements for a background job."""
    gpu_required: bool = False
    min_cpus: int = 1
    min_memory_gb: float = 2.0
    timeout_seconds: int = 3600
    conda_env: str | None = None

class JobBackend(ABC):
    """Abstract backend for background job execution.
    
    NOT for agent tool execution (which is always in-process).
    This is for heavy async workloads: training, data collection, evaluation.
    """
    
    @abstractmethod
    async def submit(self, job_type: str, inputs: dict, requirements: JobRequirements) -> str:
        """Submit job, return job_id"""
    
    @abstractmethod
    async def get_status(self, job_id: str) -> JobResult:
        """Get job status and result"""
    
    @abstractmethod
    async def cancel(self, job_id: str) -> bool:
        """Cancel running job"""
    
    @abstractmethod
    async def wait(self, job_id: str, timeout: float = None) -> JobResult:
        """Wait for job completion"""
```

### Job Backend Implementations

#### LocalJobBackend (Single Machine)

```python
# core/jobs/local.py
class LocalJobBackend(JobBackend):
    """Subprocess execution for background jobs on single machine"""
    
    async def submit(self, job_type, inputs, requirements):
        job_id = str(uuid7())
        
        if requirements.conda_env:
            self._jobs[job_id] = asyncio.create_task(
                self._run_in_env(job_type, inputs, requirements.conda_env)
            )
        else:
            self._jobs[job_id] = asyncio.create_task(
                self._run_subprocess(job_type, inputs)
            )
        
        return job_id
    
    async def _run_in_env(self, job_type, inputs, conda_env):
        """Execute job in a subprocess with different conda environment"""
        cmd = [
            "conda", "run", "-n", conda_env, "--no-capture-output",
            "python", "-m", "core.jobs.runner",
            "--job-type", job_type,
            "--inputs", json.dumps(inputs)
        ]
        proc = await asyncio.create_subprocess_exec(
            *cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
        )
        stdout, stderr = await proc.communicate()
        
        if proc.returncode != 0:
            raise JobExecutionError(f"Job failed: {stderr.decode()}")
        
        return json.loads(stdout.decode())
```

#### RayJobBackend (Distributed Compute)

```python
# core/jobs/ray_backend.py
class RayJobBackend(JobBackend):
    """Ray cluster execution for distributed/GPU workloads"""
    
    def __init__(self, address: str = "auto"):
        import ray
        if not ray.is_initialized():
            ray.init(address=address)
        self.ray = ray
    
    async def submit(self, skill_id, inputs, requirements):
        resources = {
            "num_cpus": requirements.min_cpus or 1,
            "num_gpus": 1 if requirements.gpu_required else 0,
        }
        if requirements.min_memory_gb:
            resources["memory"] = requirements.min_memory_gb * 1024**3
        
        # Runtime env for conda isolation
        runtime_env = {}
        if requirements.conda_env:
            runtime_env["conda"] = requirements.conda_env
        if requirements.optional_packages:
            runtime_env["pip"] = requirements.optional_packages
        
        @self.ray.remote(**resources)
        def run_skill(sid, inp):
            from core.skills.runner import load_and_execute
            return load_and_execute(sid, inp)
        
        ref = run_skill.options(runtime_env=runtime_env).remote(skill_id, inputs)
        job_id = ref.task_id().hex()
        self._refs[job_id] = ref
        return job_id
    
    async def wait(self, job_id, timeout=None):
        ref = self._refs[job_id]
        try:
            result = await asyncio.wait_for(
                asyncio.wrap_future(ref.future()),
                timeout=timeout
            )
            return JobResult(job_id=job_id, status=JobStatus.COMPLETED, result=result)
        except asyncio.TimeoutError:
            return JobResult(job_id=job_id, status=JobStatus.RUNNING)
```

#### K8sJobBackend (Cloud Native)

```python
# core/jobs/k8s_backend.py
class K8sJobBackend(JobBackend):
    """Kubernetes Job execution for cloud-native deployments"""
    
    def __init__(self, namespace: str = "mo-agent", image_registry: str = ""):
        from kubernetes import client, config
        
        # Auto-detect: in-cluster or kubeconfig
        try:
            config.load_incluster_config()
        except config.ConfigException:
            config.load_kube_config()
        
        self.batch_api = client.BatchV1Api()
        self.core_api = client.CoreV1Api()
        self.namespace = namespace
        self.image_registry = image_registry
    
    async def submit(self, skill_id, inputs, requirements):
        job_name = f"skill-{skill_id}-{uuid7().hex[:8]}"
        
        # Select image based on environment needs
        image = self._select_image(requirements)
        
        # Build resource requirements
        resources = {"requests": {}, "limits": {}}
        resources["requests"]["cpu"] = str(requirements.min_cpus or 1)
        resources["requests"]["memory"] = f"{requirements.min_memory_gb or 2}Gi"
        if requirements.gpu_required:
            resources["limits"]["nvidia.com/gpu"] = "1"
        
        # Build Job spec
        job = {
            "apiVersion": "batch/v1",
            "kind": "Job",
            "metadata": {
                "name": job_name,
                "namespace": self.namespace,
                "labels": {
                    "app": "mo-agent",
                    "component": "skill-worker",
                    "skill": skill_id
                }
            },
            "spec": {
                "backoffLimit": requirements.max_retries,
                "activeDeadlineSeconds": requirements.timeout_seconds or 3600,
                "template": {
                    "metadata": {
                        "labels": {"app": "mo-agent", "skill": skill_id}
                    },
                    "spec": {
                        "restartPolicy": "Never",
                        "containers": [{
                            "name": "skill-runner",
                            "image": image,
                            "command": ["python", "-m", "core.skills.runner"],
                            "args": ["--skill-id", skill_id, "--inputs", json.dumps(inputs)],
                            "resources": resources,
                            "envFrom": [{"configMapRef": {"name": "mo-agent-config"}}],
                            "env": [
                                {"name": "MATRIXONE_HOST", "value": "matrixone.mo-agent.svc"},
                                {"name": "REDIS_URL", "value": "redis://redis.mo-agent.svc:6379"}
                            ]
                        }],
                        # GPU node selector
                        **({"nodeSelector": requirements.node_selector}
                           if requirements.node_selector else {}),
                        # Tolerations for GPU nodes
                        **({"tolerations": [{"key": "nvidia.com/gpu", "operator": "Exists"}]}
                           if requirements.gpu_required else {})
                    }
                }
            }
        }
        
        self.batch_api.create_namespaced_job(namespace=self.namespace, body=job)
        return job_name
    
    def _select_image(self, requirements):
        """Select Docker image based on skill requirements"""
        base = self.image_registry or "mo-agent-engine"
        
        if requirements.gpu_required or requirements.conda_env == "agent-engine-train":
            return f"{base}:train-gpu"  # Image with PyTorch + CUDA
        else:
            return f"{base}:latest"     # Base image
    
    async def get_status(self, job_id):
        job = self.batch_api.read_namespaced_job(name=job_id, namespace=self.namespace)
        
        if job.status.succeeded:
            # Read result from job's output (stored in ConfigMap or S3)
            result = await self._read_job_output(job_id)
            return JobResult(job_id=job_id, status=JobStatus.COMPLETED, result=result)
        elif job.status.failed:
            logs = self._get_pod_logs(job_id)
            return JobResult(job_id=job_id, status=JobStatus.FAILED, error=logs)
        else:
            return JobResult(job_id=job_id, status=JobStatus.RUNNING)
    
    async def cancel(self, job_id):
        self.batch_api.delete_namespaced_job(
            name=job_id, namespace=self.namespace,
            propagation_policy="Background"
        )
        return True
```

---

## 4. Job Router

```python
# core/jobs/router.py
class JobRouter:
    """Selects the best backend based on environment and job requirements."""
    
    def __init__(self, config: dict = None):
        self.config = config or {}
        self.backends: dict[str, JobBackend] = {}
        self._detect_backends()
    
    def _detect_backends(self):
        self.backends["local"] = LocalJobBackend()
        
        # Ray: optional
        ray_addr = os.getenv("RAY_ADDRESS")
        if ray_addr:
            self.backends["ray"] = RayJobBackend(address=ray_addr)
        
        # K8s: auto-detect in-cluster or kubeconfig
        if os.getenv("KUBERNETES_SERVICE_HOST") or Path("~/.kube/config").expanduser().exists():
            self.backends["k8s"] = K8sJobBackend()
    
    def select(self, requirements: JobRequirements) -> JobBackend:
        if requirements.gpu_required:
            for name in ["ray", "k8s", "local"]:
                if name in self.backends:
                    return self.backends[name]
        return self.backends["local"]
```

### Relationship to AgentExecutor

**AgentExecutor is NOT changed.** It continues to execute tools in-process via `ToolMockingLayer`.

JobRouter is used by **API endpoints** or **scheduled tasks** that submit background jobs:

```python
# api/routers/jobs.py
@router.post("/jobs")
async def submit_job(request: JobRequest, ...):
    router = JobRouter()
    job_id = await router.select(request.requirements).submit(
        job_type=request.job_type,
        inputs=request.inputs,
        requirements=request.requirements,
    )
    return {"job_id": job_id}

@router.get("/jobs/{job_id}")
async def get_job_status(job_id: str, ...):
    ...
```

---

## 5. Docker Images

### Multi-Stage Build

```dockerfile
# Dockerfile.base — Lightweight (API + inference)
FROM python:3.11-slim AS base
WORKDIR /app
COPY pyproject.toml ./
RUN pip install --no-cache-dir -e .
COPY . .
RUN useradd -m -u 1000 appuser && chown -R appuser:appuser /app
USER appuser
EXPOSE 8000

# Dockerfile.train — Heavy (training + GPU)
FROM nvidia/cuda:12.1.0-runtime-ubuntu22.04 AS train
RUN apt-get update && apt-get install -y python3.11 python3-pip
WORKDIR /app
COPY pyproject.toml ./
RUN pip install --no-cache-dir -e ".[train]"
# train extras: torch, transformers, accelerate, datasets
COPY . .
```

### Image Matrix

| Image Tag | Base | Size | GPU | Use Case |
|-----------|------|------|-----|----------|
| `mo-agent:latest` | python:3.11-slim | ~500MB | ❌ | API, CLI, inference |
| `mo-agent:infer` | python:3.11-slim | ~600MB | ❌ | + ONNX Runtime |
| `mo-agent:train-gpu` | nvidia/cuda:12.1 | ~8GB | ✅ | Training workloads |
| `mo-agent:train-cpu` | python:3.11-slim | ~4GB | ❌ | Training (no GPU) |

---

## 6. Kubernetes Manifests

All components except API are optional. Helm values control what gets deployed.

### API Server Deployment (Required)

```yaml
# k8s/api-deployment.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: mo-agent-api
  namespace: mo-agent
spec:
  replicas: 2
  selector:
    matchLabels:
      app: mo-agent
      component: api
  template:
    metadata:
      labels:
        app: mo-agent
        component: api
    spec:
      containers:
      - name: api
        image: mo-agent:latest
        command: ["uvicorn", "api.main:app", "--host", "0.0.0.0", "--port", "8000"]
        ports:
        - containerPort: 8000
        resources:
          requests:
            cpu: "500m"
            memory: "512Mi"
          limits:
            cpu: "2"
            memory: "2Gi"
        env:
        - name: MATRIXONE_HOST
          value: "matrixone.mo-agent.svc.cluster.local"
        - name: REDIS_URL
          value: "redis://redis.mo-agent.svc.cluster.local:6379"
        envFrom:
        - secretRef:
            name: mo-agent-secrets
        readinessProbe:
          httpGet:
            path: /health
            port: 8000
          initialDelaySeconds: 5
          periodSeconds: 10
        livenessProbe:
          httpGet:
            path: /health
            port: 8000
          initialDelaySeconds: 15
          periodSeconds: 30
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: mo-agent-api-hpa
  namespace: mo-agent
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: mo-agent-api
  minReplicas: 2
  maxReplicas: 10
  metrics:
  - type: Resource
    resource:
      name: cpu
      target:
        type: Utilization
        averageUtilization: 70
```

### Model Server Deployment [opt]

```yaml
# k8s/model-server-deployment.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: mo-agent-model-server
  namespace: mo-agent
spec:
  replicas: 1
  selector:
    matchLabels:
      app: mo-agent
      component: model-server
  template:
    spec:
      containers:
      - name: model-server
        image: mo-agent:infer
        command: ["python", "-m", "core.models.model_server", "--port", "9527"]
        ports:
        - containerPort: 9527
        resources:
          requests:
            cpu: "1"
            memory: "1Gi"
          limits:
            cpu: "4"
            memory: "4Gi"
        volumeMounts:
        - name: model-cache
          mountPath: /models
      volumes:
      - name: model-cache
        persistentVolumeClaim:
          claimName: model-cache-pvc
```

### GPU Training Job Template [opt]

```yaml
# k8s/training-job-template.yaml
apiVersion: batch/v1
kind: Job
metadata:
  generateName: skill-feedback-trainer-
  namespace: mo-agent
  labels:
    app: mo-agent
    component: skill-worker
    skill: feedback_trainer
spec:
  backoffLimit: 2
  activeDeadlineSeconds: 7200  # 2 hours max
  template:
    spec:
      restartPolicy: Never
      nodeSelector:
        accelerator: nvidia-gpu
      tolerations:
      - key: "nvidia.com/gpu"
        operator: "Exists"
        effect: "NoSchedule"
      containers:
      - name: trainer
        image: mo-agent:train-gpu
        command: ["python", "-m", "core.skills.runner"]
        args: ["--skill-id", "feedback_trainer", "--inputs-file", "/tmp/inputs.json"]
        resources:
          requests:
            cpu: "4"
            memory: "8Gi"
            nvidia.com/gpu: "1"
          limits:
            cpu: "8"
            memory: "16Gi"
            nvidia.com/gpu: "1"
        env:
        - name: MATRIXONE_HOST
          value: "matrixone.mo-agent.svc.cluster.local"
        volumeMounts:
        - name: model-output
          mountPath: /output
      volumes:
      - name: model-output
        persistentVolumeClaim:
          claimName: model-output-pvc
```

---

## 7. Ray Integration [opt]

### When to Use Ray vs Kubernetes

| Dimension | Ray | Kubernetes Jobs |
|-----------|-----|-----------------|
| Startup latency | <1s (worker pool) | 30-60s (pod scheduling) |
| GPU scheduling | Native, fine-grained | Node-level |
| Data locality | Shared object store | Need S3/PVC |
| Fault tolerance | Actor restart | Job retry |
| Best for | Iterative training, hyperparameter tuning | One-shot batch jobs |
| Overhead | Ray cluster always running | Pay per job |

### Ray Cluster Setup

```yaml
# k8s/ray-cluster.yaml (KubeRay)
apiVersion: ray.io/v1
kind: RayCluster
metadata:
  name: mo-agent-ray
  namespace: mo-agent
spec:
  headGroupSpec:
    rayStartParams:
      dashboard-host: "0.0.0.0"
    template:
      spec:
        containers:
        - name: ray-head
          image: mo-agent:train-gpu
          resources:
            requests:
              cpu: "2"
              memory: "4Gi"
  workerGroupSpecs:
  - groupName: gpu-workers
    replicas: 2
    minReplicas: 0
    maxReplicas: 4
    rayStartParams: {}
    template:
      spec:
        nodeSelector:
          accelerator: nvidia-gpu
        containers:
        - name: ray-worker
          image: mo-agent:train-gpu
          resources:
            requests:
              cpu: "4"
              memory: "8Gi"
              nvidia.com/gpu: "1"
  - groupName: cpu-workers
    replicas: 2
    minReplicas: 1
    maxReplicas: 8
    template:
      spec:
        containers:
        - name: ray-worker
          image: mo-agent:latest
          resources:
            requests:
              cpu: "2"
              memory: "4Gi"
```

### Ray Usage in Skills

```python
# skills/feedback_trainer/distributed_trainer.py
import ray
from ray import train
from ray.train.huggingface import TransformersTrainer

class DistributedFeedbackTrainer:
    """Distributed training using Ray Train"""
    
    def train(self, dataset_path: str, num_workers: int = 2):
        trainer = TransformersTrainer(
            trainer_init_per_worker=self._trainer_init,
            trainer_init_config={
                "model_name": "bert-base-multilingual-cased",
                "dataset_path": dataset_path,
            },
            scaling_config=train.ScalingConfig(
                num_workers=num_workers,
                use_gpu=True,
                resources_per_worker={"CPU": 2, "GPU": 1}
            ),
            run_config=train.RunConfig(
                storage_path="s3://mo-agent-models/ray-results",
                checkpoint_config=train.CheckpointConfig(
                    num_to_keep=2
                )
            )
        )
        
        result = trainer.fit()
        return result.metrics, result.checkpoint
```

---

## 8. Topology Comparison

| Dimension | Single Machine | Docker Compose | Kubernetes |
|-----------|---------------|----------------|------------|
| **启动命令** | `conda + make dev-up` | `docker-compose up -d` | `helm install` |
| **MatrixOne** | Docker container | 内置 container | [opt] StatefulSet 或外部 |
| **Redis** | Docker container | 内置 container | [opt] StatefulSet 或外部 |
| **API scaling** | 1 process | 2-4 workers (uvicorn) | HPA 2-10 pods |
| **Skill execution** | In-process | In-process + [opt] skill-worker | In-process + [opt] K8s Job |
| **GPU training** | Local GPU | [opt] `--profile gpu` | [opt] GPU Job / Ray |
| **ML inference** | In-process | [opt] `--profile model` | [opt] Model Server pod |
| **Ray** | N/A | N/A | [opt] KubeRay cluster |
| **启动时间** | Instant | 30-60s | 2-5min |
| **适用规模** | 1-5 users | 10-50 users | 100-10K users |
| **容错** | None | Restart policy | Pod restart + HPA |
| **成本** | $0 | $0 (local) | ~$200/mo (3 nodes) |

---

## 9. Configuration

### Unified Config

```yaml
# config/deployment.yml
deployment:
  topology: auto  # auto, local, docker-compose, kubernetes

execution:
  default_backend: auto  # auto, local, ray, kubernetes
  
  local:
    max_concurrent_skills: 4
    conda_envs:
      train: agent-engine-train
      infer: agent-engine  # default
  
  ray:
    address: auto  # or ray://head:10001
    namespace: mo-agent
    runtime_env:
      working_dir: /app
  
  kubernetes:
    namespace: mo-agent
    image_registry: registry.example.com/mo-agent
    gpu_node_selector:
      accelerator: nvidia-gpu
    service_account: mo-agent-skill-runner

model_server:
  enabled: auto  # auto, true, false
  # auto: enable when multiple API replicas detected
  host: "0.0.0.0"
  port: 9527
  model_cache_dir: /models

storage:
  artifacts:
    backend: auto  # auto, local, s3
    local_dir: ~/.mo-agent/models
    s3_bucket: mo-agent-models
    s3_prefix: artifacts/
```

### Environment Detection

```python
# config/settings.py
class DeploymentDetector:
    @staticmethod
    def detect() -> str:
        # Kubernetes
        if os.path.exists("/var/run/secrets/kubernetes.io"):
            return "kubernetes"
        
        # Docker
        if os.path.exists("/.dockerenv"):
            return "docker"
        
        # Local
        return "local"
```

---

## 10. Migration Path

### 单机 → Docker Compose

```bash
# Before: 手动启动各组件
make dev-up          # MatrixOne + Redis
mo-admin init        # Init DB
uvicorn api.main:app # API

# After: 一键全部拉起
docker-compose up -d
# 自动: MatrixOne → Redis → Init → API
```

### Docker Compose → Kubernetes

```bash
# Minimal: 只部署 API（DB/Redis 用已有的）
helm install mo-agent ./charts/mo-agent-engine \
  --set matrixone.enabled=false \
  --set matrixone.external.host=db.prod.internal \
  --set redis.enabled=false \
  --set redis.external.url=redis://redis.prod.internal:6379

# 逐步开启可选组件
helm upgrade mo-agent ./charts/mo-agent-engine \
  --set modelServer.enabled=true          # 加 Model Server
helm upgrade mo-agent ./charts/mo-agent-engine \
  --set skillWorker.enabled=true \
  --set skillWorker.gpu.enabled=true      # 加 GPU 训练
helm upgrade mo-agent ./charts/mo-agent-engine \
  --set ray.enabled=true                  # 加 Ray 集群
```

### Implementation Checklist

**Phase 1: Docker Compose All-in-One**
- [x] `deployment/all-in-one/docker-compose.yml` — all-in-one compose，profiles 控制可选组件
- [ ] `Dockerfile.train` — GPU 训练镜像
- [ ] `.env.example` — 环境变量模板
- [ ] `core/skills/worker.py` — Redis queue 消费者（skill-worker 容器入口）
- [ ] `core/models/model_server.py` — 共享推理服务

**Phase 2: Execution Backend**
- [ ] `core/agent/execution_backend.py` — ABC + BackendRouter
- [ ] `core/agent/backends/local.py` — LocalBackend
- [ ] `core/skills/runner.py` — 独立进程 skill 执行入口

**Phase 3: Kubernetes**
- [ ] `charts/mo-agent-engine/` — Helm chart
- [ ] `core/agent/backends/kubernetes_backend.py` — KubernetesBackend
- [ ] CI/CD: Docker image build + push

**Phase 4: Ray [opt]**
- [ ] `core/agent/backends/ray_backend.py` — RayBackend
- [ ] KubeRay cluster manifest

---

## 11. Job Runner (Standalone Process)

For K8s Jobs and subprocess execution, skills need a standalone entry point:

```python
# core/skills/runner.py
"""Standalone skill runner for out-of-process execution.

Used by:
  - KubernetesBackend: K8s Job containers
  - LocalBackend: subprocess with different conda env
  - RayBackend: Ray remote tasks
"""
import argparse
import json
import sys

def load_and_execute(skill_id: str, inputs: dict) -> dict:
    """Load skill from registry and execute"""
    from core.skills.registry import SkillRegistry
    from api.database import get_db_session
    
    db = next(get_db_session())
    registry = SkillRegistry(db)
    skill = registry.get(skill_id)
    
    if not skill:
        raise ValueError(f"Skill '{skill_id}' not found")
    
    # Execute (handle async)
    import asyncio
    validated = skill.validate_input(inputs)
    result = asyncio.run(skill.execute(validated))
    
    # Serialize result
    if hasattr(result, "model_dump"):
        return result.model_dump()
    return result

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--skill-id", required=True)
    parser.add_argument("--inputs", default="{}")
    parser.add_argument("--inputs-file")
    parser.add_argument("--output-file")
    args = parser.parse_args()
    
    inputs = json.loads(args.inputs)
    if args.inputs_file:
        with open(args.inputs_file) as f:
            inputs = json.load(f)
    
    result = load_and_execute(args.skill_id, inputs)
    
    output = json.dumps(result, default=str)
    if args.output_file:
        with open(args.output_file, "w") as f:
            f.write(output)
    else:
        print(output)

if __name__ == "__main__":
    main()
```
