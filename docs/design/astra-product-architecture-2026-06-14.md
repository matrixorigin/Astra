# Astra 产品架构白皮书

> 最终形态 · 产品介绍 · 架构概览 · 用户旅程 · 部署运维
>
> 2026-06-14 · v2.0

---

## 目录

- [目录](#目录)
- [一、执行摘要](#一执行摘要)
  - [一句话定义](#一句话定义)
  - [核心指标](#核心指标)
- [二、为什么是 Astra](#二为什么是-astra)
  - [2.1 现状：Agent 执行层的五大缺失](#21-现状agent-执行层的五大缺失)
  - [2.2 Astra 的方案](#22-astra-的方案)
  - [2.3 与现有方案的区别](#23-与现有方案的区别)
- [三、统一架构](#三统一架构)
  - [3.1 核心命题：一个 Runtime，N 种形态](#31-核心命题一个-runtimen-种形态)
  - [3.2 RuntimePipeline：统一的执行引擎](#32-runtimepipeline统一的执行引擎)
  - [3.3 架构分层](#33-架构分层)
  - [3.4 执行器模型：策略驱动的多执行器路由](#34-执行器模型策略驱动的多执行器路由)
  - [3.5 能力提供者模型](#35-能力提供者模型)
  - [3.6 部署架构拓扑](#36-部署架构拓扑)
- [四、Workspace：Agent 的执行上下文](#四workspaceagent-的执行上下文)
  - [4.1 Workspace 是什么](#41-workspace-是什么)
  - [4.2 Workspace 在架构中的位置](#42-workspace-在架构中的位置)
  - [4.3 Workspace 后端](#43-workspace-后端)
  - [4.4 Workspace 的统一抽象](#44-workspace-的统一抽象)
- [五、部署形态](#五部署形态)
  - [5.1 Server（团队云）](#51-server团队云)
  - [5.2 CLI（本地终端）](#52-cli本地终端)
  - [5.3 Web App（浏览器）](#53-web-app浏览器)
  - [5.4 三种形态配置对比](#54-三种形态配置对比)
- [六、架构总览](#六架构总览)
  - [6.1 三层架构](#61-三层架构)
  - [6.2 执行器模型](#62-执行器模型)
  - [6.3 能力提供者模型](#63-能力提供者模型)
  - [6.4 部署架构](#64-部署架构)
- [七、安全模型](#七安全模型)
  - [7.1 纵深防御](#71-纵深防御)
  - [7.2 隔离级别矩阵](#72-隔离级别矩阵)
  - [7.3 策略模型](#73-策略模型)
  - [7.4 证据链与不可否认性](#74-证据链与不可否认性)
- [八、能力维度](#八能力维度)
  - [8.1 工具能力全景](#81-工具能力全景)
  - [8.2 记忆系统](#82-记忆系统)
  - [8.3 多 Agent 编排](#83-多-agent-编排)
  - [8.4 持续学习与自我进化](#84-持续学习与自我进化)
  - [8.5 上下文工程](#85-上下文工程)
- [九、用户旅程](#九用户旅程)
  - [9.1 旅程 A：团队代码迁移（Server）](#91-旅程-a团队代码迁移server)
  - [9.2 旅程 B：个人日常开发（CLI）](#92-旅程-b个人日常开发cli)
  - [9.3 旅程 C：数据工程师快速原型（Web App）](#93-旅程-c数据工程师快速原型web-app)
  - [9.4 旅程 D：CI/CD 智能修复（Server + GitHub Integration）](#94-旅程-dcicd-智能修复server-github-integration)
- [十、OpenShell 集成](#十openshell-集成)
  - [10.1 分层看 OpenShell](#101-分层看-openshell)
  - [10.2 集成模式一：OpenShell 作为 Sandbox Manager（推荐）](#102-集成模式一openshell-作为-sandbox-manager推荐)
  - [10.3 集成模式二：Astra 原生 + OpenShell 策略增强](#103-集成模式二astra-原生-openshell-策略增强)
  - [10.4 集成模式三：Astra 原生 + OpenShell 审计聚合](#104-集成模式三astra-原生-openshell-审计聚合)
  - [10.5 集成模式四：混合模式（未来）](#105-集成模式四混合模式未来)
  - [10.6 集成模式选择指南](#106-集成模式选择指南)
- [十一、运维部署](#十一运维部署)
  - [11.1 Day 0：规划与准备](#111-day-0规划与准备)
  - [11.2 Day 1：部署](#112-day-1部署)
  - [11.3 Day 2：运维](#113-day-2运维)
  - [11.4 配置管理](#114-配置管理)
- [十二、性能与可扩展性](#十二性能与可扩展性)
  - [12.1 延迟分析](#121-延迟分析)
  - [12.2 扩展性](#122-扩展性)
  - [12.3 可用性](#123-可用性)
- [十三、竞品定位](#十三竞品定位)
  - [13.1 市场地图](#131-市场地图)
  - [13.2 差异化](#132-差异化)
  - [13.3 一句话定位](#133-一句话定位)
- [十四、路线图](#十四路线图)
  - [Phase 1：坚固的执行基础（当前）](#phase-1坚固的执行基础当前)
  - [Phase 2：完整的 Agent 生命周期](#phase-2完整的-agent-生命周期)
  - [Phase 3：企业级平台](#phase-3企业级平台)
  - [Phase 4：生态与平台](#phase-4生态与平台)
- [附录 A：术语表](#附录-a术语表)
- [附录 B：参考文档](#附录-b参考文档)


## 一、执行摘要

Astra 是一个**可编程的 AI Agent 执行运行时（Agentic Runtime）**。它解决的核心问题是：当 AI Agent 需要操作真实世界资源（文件系统、shell、网络、数据库、Git、第三方 API）时，如何保证**安全、可控、可审计、可复现**。

它不是又一个 AI 编程助手，而是一个位于 AI 模型和操作系统之间的**受控执行层**——就像 JVM 位于 Java 程序和操作系统之间一样：

```
┌──────────────────────────────────────────────┐
│  AI 模型 (Claude / GPT / Gemini / 开源模型)   │
│                                               │
│  发出工具调用意图                              │
│  "读文件 src/main.rs"                         │
│  "运行 cargo test"                            │
│  "搜索 PostgreSQL 迁移最佳实践"                │
└──────────────────┬───────────────────────────┘
                   │  工具调用
                   ▼
┌──────────────────────────────────────────────┐
│              Astra (Agentic Runtime)          │
│                                               │
│  · 工具发现 & 路由                             │
│  · 权限策略引擎                                │
│  · 多级隔离执行（进程 / 容器 / 微虚拟机）       │
│  · 审计 & 证据收集                             │
│  · 资源治理（配额、超时、预算）                  │
│  · 记忆 & 学习                                 │
└──────────────────┬───────────────────────────┘
                   │  受控操作
                   ▼
┌──────────────────────────────────────────────┐
│  真实世界资源                                  │
│  文件系统 / Shell / 网络 / 数据库 / Git / API  │
└──────────────────────────────────────────────┘
```

### 一句话定义

**Astra = AI Agent 的操作系统服务层**。Agent 开发者写 system prompt 和选择工具；Astra 负责执行时的每一层保障。

### 核心指标

| 指标                   | 目标值                   |
| ---------------------- | ------------------------ |
| 工具调用延迟（进程级） | <100ms p50               |
| 工具调用延迟（内核级） | <50ms p50（预热池）      |
| 沙箱冷启动             | <3s（含镜像拉取）        |
| 沙箱热启动（预热池）   | <50ms                    |
| 审计完整率             | 100%（每次调用不可绕过） |
| 单节点并发 Session     | 1,000+                   |
| 单节点并发工具执行     | 500+                     |
| 部署形态               | 3（Server / CLI / Web）  |

---

## 二、为什么是 Astra

### 2.1 现状：Agent 执行层的五大缺失

当前的 AI Agent 生态中，模型能力快速进步，但执行基础设施严重滞后：

| #   | 缺失               | 现状                                                             | 后果                                             |
| --- | ------------------ | ---------------------------------------------------------------- | ------------------------------------------------ |
| 1   | **没有执行边界**   | Agent 直接运行在用户操作系统上，与用户进程共享文件系统和网络     | 一次错误的 `rm -rf` 或 `DROP TABLE` 就是生产事故 |
| 2   | **没有权限模型**   | Agent 是全能的——能调用什么工具由 prompt 说了算，没有基础设施强制 | 供应链攻击、数据泄露、越权操作无法从系统层面阻止 |
| 3   | **没有审计追踪**   | Agent 做了什么、为什么这么做、结果如何——会话结束后全部消失       | 出了问题无法追溯、合规审计无法通过               |
| 4   | **没有资源治理**   | Agent 可以无限循环、无限消耗 token、无限占用 CPU                 | 一次失控的 Agent 运行可能烧掉数千美元            |
| 5   | **没有一致性保证** | 本地 CI、远程 Server、Web App 上 Agent 行为不同                  | 团队协作时结果不可复现，调试困难                 |

### 2.2 Astra 的方案

Astra 用**受控执行层**填补这些缺失——让 Agent 的能力被显式声明、强制执行、完整记录：

```
现状:
  AI Model → raw OS

Astra:
  AI Model → policy check → isolation boundary → execution → audit → result
```

**类比**：把 Agent 从"在宿主机上跑 root shell 脚本"升级为"在容器编排平台上跑声明式 workload"。

### 2.3 与现有方案的区别

| 方案                 | 本质                | 缺失                                                     |
| -------------------- | ------------------- | -------------------------------------------------------- |
| LangChain / CrewAI   | Agent 框架（库）    | 无运行时保障，开发者自己负责安全、审计、资源控制         |
| Claude Code / Cursor | 单机编程助手        | 无平台服务，无多租户，无团队管控                         |
| E2B / Daytona        | 远程沙箱服务        | 只管执行，不管理 Agent 生命周期、记忆、学习              |
| Docker / K8s         | 通用容器平台        | 没有 Agent 语义（工具路由、权限策略、审计链）            |
| **Astra**            | **Agentic Runtime** | **执行 + 治理 + 学习 + 审计，完整的 Agent 生命周期管理** |

---

## 三、统一架构

### 3.1 核心命题：一个 Runtime，N 种形态

CLI、Server、Web App 不是三个独立产品，而是**同一套 RuntimePipeline 的三种部署配置**。它们的差异仅在于：

- **工具可选择性不同**——CLI 可调用本地 shell，Server 强制沙箱
- **执行器目标不同**——CLI 直接操作本地文件，Server 操作远程 workspace
- **安全边界不同**——CLI 受用户 OS 权限约束，Server 强制隔离

同一套 RuntimePipeline 代码路径覆盖所有场景——ExecutionPlan → TaskQueue → Executor → Observer 四阶段流水线，在 CLI、Server、Web App 中**完全一致**。不存在"CLI 架构"和"Server 架构"——只有一种架构，三种配置。

```
┌───────────────────────────────────────────────────────────┐
│                   Astra RuntimePipeline                    │
│                                                           │
│  ExecutionPlan ──► TaskQueue ──► Executor ──► Observer    │
│       │                │            │            │        │
│       ▼                ▼            ▼            ▼        │
│  DeploymentProfile   Skill 池   策略路由    Journal/审计   │
│  (CLI | Server | Web)                                     │
└───────────────────────────────────────────────────────────┘
          │                          │
    ┌─────┴──────┐            ┌──────┴──────┐
    │    CLI     │            │   Server    │
    │  本地 Shell │            │  沙箱执行    │
    └────────────┘            └─────────────┘
```

### 3.2 RuntimePipeline：统一的执行引擎

所有 Astra 请求——无论来自 CLI 终端、Web App 还是 API——都经过同一流水线的四个阶段：

| 阶段              | 职责                           | 关键组件                               |
| ----------------- | ------------------------------ | -------------------------------------- |
| **ExecutionPlan** | 意图解析、任务分解、依赖分析   | Plan 引擎、Skill 路由、Context 注入    |
| **TaskQueue**     | 任务调度、并发控制、优先级管理 | DAG 调度器、限流器、重试策略           |
| **Executor**      | 策略驱动的多执行器路由         | Shell / Sandbox / API / Browser 执行器 |
| **Observer**      | 结果验证、副作用追踪、审计记录 | 验证器链、Journal、回滚管理            |

关键不变量（所有形态共享）：

- **Skill 定义统一**：`SKILL.md` 文件在 CLI/Server/Web 中解析规则相同，执行语义一致
- **工具协议统一**：所有工具（文件读写、shell、API 调用）通过同一 `ToolSchema` 接口
- **记忆系统统一**：`Semantic / Procedural / Episodic` 三层记忆语义一致，仅后端存储位置不同
- **Journal 格式统一**：审计日志、变更追踪使用同一 schema，CLI 和 Server 的日志可直接合并

### 3.3 架构分层

Astra 采用四层架构——每一层在自己的职责范围内对上层透明，接入层的切换不影响控制层和执行层：

```
┌──────────────────────────────────────────────────┐
│               接入层 (Access Layer)                │
│   CLI (PTY)  │  Web App (WebSocket)  │  API  │ IDE │
├──────────────────────────────────────────────────┤
│              控制层 (Control Plane)                │
│  Session Mgr │ Plan Engine │ Skill Router        │
│  Policy Engine │ Rate Limiter │ Auth             │
├──────────────────────────────────────────────────┤
│              执行层 (Execution Plane)              │
│  Shell Executor │ Sandbox Executor               │
│  API Executor   │ Browser Executor               │
│  Strategy Router │ CapabilityProvider            │
├──────────────────────────────────────────────────┤
│               数据层 (Data Plane)                  │
│  Journal │ Memory │ Workspace │ Snapshot         │
│  Audit Log │ Metrics │ Config Store              │
└──────────────────────────────────────────────────┘
```

四层架构的设计原则：

- **接入层**只负责通信协议（PTY、HTTP、WebSocket），不包含业务逻辑——切换接入形态不影响任何下游
- **控制层**是无状态的编排层——同一个 Plan Engine 同时服务 CLI 和 Server 请求
- **执行层**通过 Strategy 模式路由到不同执行器，执行器的选择由 `DeploymentProfile` 声明式决定，而非硬编码
- **数据层**提供统一持久化抽象——CLI 写入本地 MO 数据库，Server 写入远端 MO 集群，但调用的是同一个 trait

### 3.4 执行器模型：策略驱动的多执行器路由

Astra 不绑定单一执行方式。RuntimePipeline 根据任务特征和部署配置，自动选择最优执行器：

| 执行器      | 适用场景                       | CLI          | Server       |
| ----------- | ------------------------------ | ------------ | ------------ |
| **Shell**   | 本地文件操作、git、shell 命令  | ✅ 直接调用  | ❌ 禁止      |
| **Sandbox** | 隔离执行、不可信代码           | ✅ OpenShell | ✅ OpenShell |
| **API**     | GitHub、Slack、Jira 等外部集成 | ✅           | ✅           |
| **Browser** | Web 交互、文档查阅             | ✅           | ✅           |

策略路由的核心逻辑（所有形态共享同一路由引擎）：

```
if task.tool == "bash" && profile == CLI:
    route_to(ShellExecutor.local)       // 宿主机 shell
elif task.tool == "bash" && profile == Server:
    route_to(SandboxExecutor.openshell) // 容器沙箱
elif task.tool in ["github", "slack"]:
    route_to(APIExecutor.oauth)         // OAuth 集成
else:
    route_to(BuiltinExecutor)           // 内置工具
```

**同一个 `bash` 工具声明，不同配置下路由到不同执行器——工具定义不感知部署形态。**

### 3.5 能力提供者模型

工具能力通过 `CapabilityProvider` trait 注入，与运行形态解耦：

```rust
trait CapabilityProvider {
    fn tools(&self) -> Vec<ToolSchema>;      // 声明可用工具
    fn executor(&self) -> Box<dyn Executor>;  // 工具的执行器
    fn requirements(&self) -> Requirements;   // 前置条件（如需要 workspace）
}
```

不同部署配置激活不同的 Provider 集合：

| 配置        | 激活的 Provider                                             | 禁用的 Provider                          |
| ----------- | ----------------------------------------------------------- | ---------------------------------------- |
| **CLI**     | FileSystemProvider, ShellProvider, GitProvider, APIProvider | —                                        |
| **Server**  | SandboxProvider, APIProvider, GitProvider(remote)           | ShellProvider, FileSystemProvider(local) |
| **Web App** | APIProvider（经 Server 代理）                               | ShellProvider, FileSystemProvider        |

Provider 的激活/禁用由 `DeploymentProfile` 声明式控制，不需要修改核心 RuntimePipeline 代码。新增一个部署形态只需定义一个新的 Profile 配置，无需改动执行引擎。

### 3.6 部署架构拓扑

```
                      ┌──────────────┐
                      │   Web App    │
                      │ (Browser UI) │
                      └──────┬───────┘
                             │ WebSocket
                      ┌──────┴───────┐
                      │  API Server  │
                      │  (axum)      │
                      └──────┬───────┘
                             │
                ┌────────────┼────────────┐
                │            │            │
          ┌─────┴─────┐ ┌───┴────┐ ┌─────┴─────┐
          │   CLI     │ │ Server │ │ Scheduler │
          │ (本地 PTY) │ │ (远程)  │ │ (异步任务) │
          └─────┬─────┘ └───┬────┘ └─────┬─────┘
                │            │            │
                └────────────┼────────────┘
                             │
                ┌────────────┼────────────┐
                │            │            │
          ┌─────┴─────┐ ┌───┴────┐ ┌─────┴─────┐
          │  Journal  │ │ Memory │ │ Snapshot  │
          │  (MO DB)  │ │ (MO DB)│ │ (MO DB)   │
          └───────────┘ └────────┘ └───────────┘
```

CLI、Server、Web App 三种接入方式汇聚到同一 API Server，共享同一套数据层。CLI 的本地 MO 数据库与 Server 的远端 MO 集群在 schema 上完全兼容，支持数据同步。

---

## 四、Workspace：Agent 的执行上下文

### 4.1 Workspace 是什么

Workspace 是 Agent 执行任务时的**文件系统上下文**——工作目录、源代码、构建产物、依赖缓存。它是 Agent 从"对话机器人"升级为"能写代码、跑测试、修 bug 的工程师"的关键概念。

没有 Workspace，Agent 只能回答问题；有了 Workspace，Agent 能**操作真实的代码仓库**。

### 4.2 Workspace 在架构中的位置

Workspace 并非与 RuntimePipeline 平级的概念，而是**贯穿整个流水线**：

```
Skill 声明 ──► 意图解析 ──► 工具调用 ──► 结果持久化
    │              │             │             │
    ▼              ▼             ▼             ▼
"needs_workspace" Plan 引擎     文件工具      变更追踪
    : true        注入路径      read_file     git diff
                  上下文        write_file    Journal 记录
```

- **Skill 层**：Skill 声明 `needs_workspace: true`，告知 Plan 引擎该任务需要工作目录
- **Plan 层**：根据任务类型自动创建或挂载 workspace，将路径注入执行上下文
- **执行层**：文件工具（`read_file`、`write_file`、`str_replace`）操作的都是 workspace 内的路径——Executor 不感知 workspace 后端类型
- **Observer 层**：workspace 变更自动记录到 Journal，支持回滚和审计追溯

### 4.3 Workspace 后端

Astra 的 Workspace 是**后端无关的**——上层代码通过统一 trait 操作 workspace，不关心是本地目录还是远程沙箱：

| 后端                  | 适用场景                  | 隔离级别                   |
| --------------------- | ------------------------- | -------------------------- |
| **Local**             | CLI 本地开发              | 宿主机文件系统（当前目录） |
| **TempDir**           | 临时任务、CI 构建         | 进程级临时目录             |
| **OpenShell Sandbox** | Server 多租户、不可信代码 | 容器级隔离                 |
| **FUSE 挂载**（未来） | 大型 monorepo、远程开发   | 按需懒加载                 |

### 4.4 Workspace 的统一抽象

```rust
trait WorkspaceProvider {
    /// 准备 workspace（clone、mount、create temp）
    async fn prepare(&self, spec: &WorkspaceSpec) -> Result<Workspace>;

    /// workspace 的根路径（在 executor 看来是本地路径）
    fn root(&self) -> &Path;

    /// 获取变更摘要（git diff 或文件快照对比）
    async fn changes(&self) -> Result<ChangeSet>;

    /// 任务完成后清理
    async fn teardown(&self) -> Result<()>;
}
```

**同一个 RuntimePipeline 代码路径，Workspace 后端可热切换**：

- CLI 模式下：`workspace = /Users/xu/gh/myproject`（当前目录）
- Server 模式下：`workspace = /sandbox/task-abc123/`（OpenShell 自动创建）
- 对 Executor 来说，它只看到 `workspace.root()`，**不需要知道后端是什么**

这种抽象使得：

- 同一个 Skill 在 CLI 和 Server 中行为一致——文件操作的对象只是路径不同
- Server 端可以无缝切换到更强的隔离后端，而不改变任何上层代码
- Workspace 快照和回滚在所有后端上语义统一

---

## 五、部署形态

三种部署形态共享第三章描述的全部架构——同一个 RuntimePipeline、同一套 Skill 系统、同一套记忆模型。差异仅限于 `DeploymentProfile` 配置。

### 5.1 Server（团队云）

**定位**：团队共享的持久化 Agent 服务

```
用户 ──► Web App / API ──► Astra Server ──► Sandbox Executor
                              │
                              ├── Journal (远端 MO DB)
                              ├── Memory (远端 MO DB)
                              └── Workspace (OpenShell)
```

- 多租户隔离，每任务独立 workspace
- 持久化记忆跨会话共享——团队成员受益于彼此的使用积累
- 团队级配置、安全策略、操作审计

### 5.2 CLI（本地终端）

**定位**：个人开发者的本地 Agent

```
用户 ──► Terminal (PTY) ──► Astra CLI ──► Shell Executor (本地)
                              │
                              ├── Journal (本地 MO DB)
                              ├── Memory (本地 MO DB)
                              └── Workspace (当前目录)
```

- 零配置：`cd` 到项目目录直接使用
- 完整本地工具链访问（git、cargo、npm 等，无需沙箱限制）
- 本地 MO 数据库持久化记忆和 Journal，数据可同步到 Server

### 5.3 Web App（浏览器）

**定位**：轻量级入口，通过 WebSocket 连接 Server

```
浏览器 ──► Web App (React) ──► WebSocket ──► Astra Server
```

- 无需安装，浏览器即用
- 所有执行经 Server 代理——与 Server 共用完全相同的 RuntimePipeline
- 适合演示、协作评审、轻量任务

### 5.4 三种形态配置对比

| 维度                | CLI          | Server         | Web App                     |
| ------------------- | ------------ | -------------- | --------------------------- |
| **RuntimePipeline** | ✅ 相同代码  | ✅ 相同代码    | ✅ 相同代码                 |
| **Skill 系统**      | ✅ 相同解析  | ✅ 相同解析    | ✅ 相同解析                 |
| **Memory**          | 本地 MO      | 远端 MO        | 远端 MO（经 Server）        |
| **Workspace**       | 当前目录     | OpenShell 沙箱 | OpenShell 沙箱（经 Server） |
| **Shell 工具**      | ✅ 直接调用  | ❌ 沙箱代理    | ❌ 沙箱代理                 |
| **多租户**          | ❌           | ✅             | ✅                          |
| **持久化位置**      | 本地磁盘     | 云端           | 云端                        |
| **安装方式**        | brew / cargo | docker / k8s   | 浏览器打开                  |

**核心结论**：不存在"CLI 架构"和"Server 架构"——只有一种架构（统一 RuntimePipeline），三种配置（DeploymentProfile）。
┌──────────────────────────────────────────────────────────────┐
│ Astra Runtime (统一核心) │
│ │
│ Agent Loop → Tool Routing → Policy Check → Executor → Result │
│ 记忆 · 审计 · 上下文工程 · 任务管理 · 多 Agent 编排 │
│ │
│ ← DeploymentProfile 控制差异 → │
│ 工具集 · 隔离级别 · Workspace 存储 · 审计粒度 · 认证方式 │
└──────────────────────────────────────────────────────────────┘
▲ ▲ ▲
│ │ │
┌────┴────┐ ┌────┴────┐ ┌────┴────┐
│ Server │ │ CLI │ │Web App │
│ 团队云 │ │本地终端 │ │ 浏览器 │
│ │ │ │ │ │
│ gVisor │ │ 边车进程│ │ 边车容器│
│ 完整工具│ │ 核心工具│ │ 基础工具│
│ 持久卷 │ │ 本地磁盘│ │ Session │
└─────────┘ └─────────┘ └─────────┘

```

这不是三个产品，而是**同一个 Runtime 的三种部署配置**。当你从 CLI 切换到 Server，改变的不是 Runtime 本身，而是 DeploymentProfile 的几个参数。

## 六、架构总览

### 6.1 三层架构
Astra 的架构模型从内到外分为三层：

```

┌────────────────────────────────────────────────────────────┐
│ 接入层 (Access) │
│ │
│ Web Console · CLI TUI · REST API · WebSocket · Webhook │
│ GitHub App · CI/CD Plugin · IDE Extension │
└──────────────────────────┬─────────────────────────────────┘
│
┌──────────────────────────▼─────────────────────────────────┐
│ 控制面 (Control Plane) │
│ │
│ ┌───────────┐ ┌──────────┐ ┌─────────┐ ┌──────────────┐ │
│ │ 会话管理 │ │ 策略引擎 │ │ 能力注册 │ │ Agent 编排 │ │
│ │ │ │ │ │ │ │ │ │
│ │ Session │ │ 允许/拒绝 │ │ 工具注册 │ │ 委托·并行· │ │
│ │ 生命周期 │ │ 工具列表 │ │ 能力发现 │ │ 扇出·管道 │ │
│ └─────┬─────┘ └────┬─────┘ └────┬────┘ └──────┬───────┘ │
│ │ │ │ │ │
│ ┌─────▼────────────▼────────────▼──────────────▼───────┐ │
│ │ 执行路由器 (Execution Router) │ │
│ │ │ │
│ │ 工具名 → 策略匹配 → 执行器选择 → 隔离级别确定 │ │
│ │ bash → policy("allow") → LocalExecutor → Process │ │
│ │ cargo → policy("sandbox") → DockerExecutor → gVisor │ │
│ └──────────────────────────┬────────────────────────────┘ │
└─────────────────────────────┼──────────────────────────────┘
│
┌─────────────────────────────▼──────────────────────────────┐
│ 执行面 (Execution Plane) │
│ │
│ ┌──────────────┐ ┌───────────────┐ ┌────────────────┐ │
│ │ 直接执行器 │ │ 本地执行器 │ │ 沙箱执行器 │ │
│ │ │ │ │ │ │ │
│ │ 进程内执行 │ │ 边车子进程 │ │ gVisor microVM │ │
│ │ 隔离: 无 │ │ 隔离: 进程级 │ │ 隔离: 内核级 │ │
│ │ │ │ │ │ │ │
│ │ 适用: 记忆 │ │ 适用: 文件 │ │ 适用: 命令 │ │
│ │ 任务·会话 │ │ Git·搜索·LSP │ │ 编译·安装 │ │
│ └──────┬───────┘ └───────┬───────┘ └───────┬────────┘ │
│ │ │ │ │
│ ┌──────▼──────────────────▼───────────────────▼────────┐ │
│ │ 证据收集 (Evidence Collection) │ │
│ │ 退出码 · stdout/stderr · 文件修改 · 网络访问 · 耗时 │ │
│ └──────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────┘
│
┌─────────────────────────────▼──────────────────────────────┐
│ 数据面 (Data Plane) │
│ │
│ ┌───────────┐ ┌───────────┐ ┌──────────┐ ┌────────────┐ │
│ │ 记忆存储 │ │ 会话存储 │ │ 审计日志 │ │ 用户卷 │ │
│ │ │ │ │ │ │ │ │ │
│ │ Memoria │ │ 对话·事件 │ │ 操作记录 │ │ workspace │ │
│ │ 语义·情景 │ │ 决策·快照 │ │ 不可篡改 │ │ 缓存·构建 │ │
│ │ 程序记忆 │ │ 因果链 │ │ 合规就绪 │ │ 持久化 │ │
│ └───────────┘ └───────────┘ └──────────┘ └────────────┘ │
└────────────────────────────────────────────────────────────┘

```

### 6.2 执行器模型
Astra 采用**策略驱动的多执行器路由**——每个工具调用根据风险等级和部署配置自动路由到合适的执行器：

```

工具调用请求
│
▼
┌─────────────┐
│ 策略引擎 │ ← 查询: 该工具是否被禁用? 该用户的策略?
│ │ 工具: bash, 用户: alice, 上下文: workspace
│ │ → 允许, 需要沙箱隔离
└──────┬──────┘
│
▼
┌──────────────┐
│ 执行路由器 │ ← 选择执行器: 工具类型 + 部署配置 + 策略要求
│ │ bash → DockerExecutor(gVisor)
│ │ read_file → LocalExecutor
│ │ memory_recall → DirectExecutor
└──────┬───────┘
│
▼
┌──────────────────────────────────────────────────┐
│ 三级隔离执行器 │
│ │
│ DirectExecutor 无隔离, 进程内 │
│ ───────────────────────────────────────────── │
│ LocalExecutor 进程隔离, 边车子进程 │
│ ───────────────────────────────────────────── │
│ DockerExecutor OCI 容器, gVisor/runc │
│ ───────────────────────────────────────────── │
│ OpenShellExecutor OpenShell 管理, 策略增强 │
└──────────────────────────────────────────────────┘

````

### 6.3 能力提供者模型
工具不是平台硬编码的。每个工具都作为**能力提供者（Capability Provider）**注册：

```yaml
能力提供者: FileSystemProvider
  声明工具: read_file, write_file, str_replace, glob, list_dir
  隔离需求: Process
  网络访问: 否
  优先级: 1 (本地优先)

能力提供者: ShellProvider
  声明工具: bash
  隔离需求: Container | Sandbox
  网络访问: 按策略
  优先级: 2

能力提供者: NetworkProvider
  声明工具: web_search, web_fetch
  隔离需求: 无 (纯 API 调用)
  网络访问: 是
  优先级: 1

能力提供者: DatabaseProvider
  声明工具: mo, mo_query
  隔离需求: 无
  网络访问: 是 (数据库连接)
  优先级: 1
  存储访问: matrixone:// ...
````

**运行时解析**：

1. Agent 请求调用 `bash("cargo build")`
2. 能力注册表查找：谁提供 `bash`？→ `ShellProvider`
3. 策略检查：该 Agent / 用户是否可以调用？→ 是
4. 执行器选择：`ShellProvider` 要求 Container 隔离 → `DockerExecutor(gVisor)`
5. 执行、收集证据、返回结果

### 6.4 部署架构
```
                          ┌────────────────────┐
                          │   负载均衡 / 反向代理  │
                          │   (nginx / Caddy)    │
                          └─────────┬──────────┘
                                    │
                    ┌───────────────┼───────────────┐
                    │               │               │
              ┌─────▼─────┐  ┌─────▼─────┐  ┌─────▼─────┐
              │Astra Node 1│  │Astra Node 2│  │Astra Node N│
              │            │  │            │  │            │
              │ Session Mgr│  │ Session Mgr│  │ Session Mgr│
              │ Policy Eng │  │ Policy Eng │  │ Policy Eng │
              │ Exec Router│  │ Exec Router│  │ Exec Router│
              │            │  │            │  │            │
              │ Sandbox Pool│ │ Sandbox Pool│ │ Sandbox Pool│
              │ (docker)   │  │ (docker)   │  │ (docker)   │
              └─────┬──────┘  └─────┬──────┘  └─────┬──────┘
                    │               │               │
                    └───────────────┼───────────────┘
                                    │
                    ┌───────────────┼───────────────┐
                    │               │               │
              ┌─────▼─────┐  ┌─────▼─────┐  ┌─────▼─────┐
              │  Memoria   │  │MatrixOne  │  │UserVolume  │
              │  (Memory)  │  │  (State)  │  │  (Files)   │
              └────────────┘  └───────────┘  └────────────┘
```

- **Astra Node**：无状态，可水平扩展
- **Memoria**：Agent 记忆服务（语义/情景/程序记忆）
- **MatrixOne**：平台状态数据库（用户、会话、事件、审计）
- **UserVolume**：用户持久化文件存储（workspace、缓存、构建产物）

---

## 七、安全模型

### 7.1 纵深防御
Astra 采用三层纵深防御，每一层独立生效：

```
第 1 层: 工具面隐藏
  AI 的可用工具列表中根本看不到被禁用的工具
  → AI 不知道工具存在 → 无法尝试调用
  → 零 token 浪费，零错误处理

第 2 层: 策略引擎
  运行时检查每条工具调用
  → 来源验证 (user / agent / session)
  → 参数过滤 (命令白名单、路径限制、网络策略)
  → 配额检查 (频率、并发、总量)

第 3 层: 隔离边界
  在最坏情况下：即使策略被绕过，工具仍然运行在受控环境
  → 文件系统隔离 (chroot / overlay)
  → 系统调用过滤 (seccomp / gVisor sentry)
  → 网络隔离 (iptables / 虚拟网卡 / 策略路由)
  → 资源限制 (cgroups / 容器限制)
```

### 7.2 隔离级别矩阵
| 隔离级别      | 技术           | 文件系统     | 网络            | 系统调用      | 适用场景                     |
| ------------- | -------------- | ------------ | --------------- | ------------- | ---------------------------- |
| **None**      | 当前进程       | 共享         | 共享            | 共享          | 记忆读写、会话配置、任务状态 |
| **Process**   | 边车子进程     | 专属工作目录 | 独立网络栈\*    | 无额外限制    | 文件操作、Git、LSP、搜索     |
| **Container** | runc 容器      | Overlay FS   | 独立网络 NS\*   | Docker 默认   | 本地 Agent 命令执行          |
| **Sandbox**   | gVisor (runsc) | 独立 Overlay | 虚拟网卡 + 策略 | sentry 白名单 | 不受信代码、多租户、生产环境 |
| **MicroVM**   | Firecracker    | 独立块设备   | 独立虚拟网卡    | KVM 级隔离    | 最高安全需求、合规强制       |

> \*CLI 模式下边车为独立子进程，无独立网络命名空间（macOS 限制），但通过网络隧道策略实现等效控制。

### 7.3 策略模型
```yaml
# 策略定义示例
policy:
  id: "team-security-baseline"

  tool_policies:
    - tool: "bash"
      allow: true
      sandbox: "required" # 强制沙箱执行
      network: "block_outbound" # 默认禁止出站
      allow_list: # 允许的域名白名单
        - "*.github.com"
        - "crates.io"
        - "registry.npmjs.org"

    - tool: "web_search"
      allow: true
      rate_limit: "10/minute"

    - tool: "mo_query"
      allow: true
      readonly: true # 只允许 SELECT

    - tool: "git_push"
      allow: false # 禁止 force push
      message: "Push requires manual approval"

  resource_limits:
    max_cpu_per_execution: 4
    max_memory_per_execution: "8GB"
    execution_timeout_seconds: 600
    max_concurrent_executions_per_user: 10

  audit:
    log_all_tool_calls: true
    log_parameters: true # 记录完整参数（含敏感信息需脱敏）
    retention_days: 90
```

### 7.4 证据链与不可否认性
每次工具调用生成结构化证据（RuntimeEvidence），包含：

```yaml
evidence:
  tool_call_id: "tc_abc123"
  tool: "bash"
  parameters: "cargo build --release"
  executor: "DockerExecutor"
  isolation: "Sandbox"
  sandbox_id: "sbx_def456"

  result:
    exit_code: 0
    stdout_sha256: "e3b0c44298fc1c14..."
    stderr_sha256: "cf83e1357eefb8bd..."
    duration_ms: 45230

  environment:
    image: "astra/rust-sandbox:v2.1"
    filesystem_changes:
      - path: "/workspace/target/release/myapp"
        action: "created"
        size_bytes: 12345678
    network_connections: [] # 无出站连接

  attestation:
    signed_by: "astra-node-3"
    timestamp: "2026-06-14T10:30:00Z"
    chain_hash: "a1b2c3..." # 前一条证据的哈希
```

证据链保证：从操作发生到审计查询之间，任何环节都无法篡改历史记录。

---

## 八、能力维度

### 8.1 工具能力全景
| 功能域       | 工具                                                                            | Server    | CLI       | Web        | 说明                                          |
| ------------ | ------------------------------------------------------------------------------- | --------- | --------- | ---------- | --------------------------------------------- |
| **文件操作** | read_file, write_file, str_replace, glob, list_dir                              | ✅        | ✅        | ✅         | 支持精确行替换、glob 模式搜索、大文件分段读取 |
| **命令执行** | bash                                                                            | ✅ (沙箱) | ✅ (边车) | ✅ (容器)  | 支持超时控制、环境变量注入、working directory |
| **版本控制** | git (status/diff/log/show/commit/blame)                                         | ✅        | ✅        | ❌         | 完整 Git 操作，支持 diff 预览                 |
| **代码分析** | symbols, find_definition, find_references, lsp (diagnostics/code_action/rename) | ✅        | ✅        | ✅ (基础)  | 基于 tree-sitter AST 解析，LSP 集成           |
| **代码审查** | review-changes, review-code                                                     | ✅        | ✅        | ❌         | 自动 diff 分析、测试覆盖率检查                |
| **网络访问** | web_search, web_fetch                                                           | ✅ (可控) | ✅        | ✅         | 支持搜索引擎、网页抓取、结构化提取            |
| **数据库**   | mo_query (MatrixOne SQL)                                                        | ✅        | ❌        | ❌         | 支持只读查询，可配置写权限                    |
| **GitHub**   | github (get_pr/ci_status/create_issue)                                          | ✅        | ✅        | ❌         | PR 管理、Issue 创建、CI 状态查询              |
| **记忆**     | memory (remember/recall/forget/update/reflect)                                  | ✅        | ✅        | ✅         | 9 种认知动词，跨 session 持久化               |
| **任务**     | task (create/update/list/stop/archive)                                          | ✅        | ✅        | ✅         | 持久化任务清单，支持依赖关系                  |
| **会话**     | session (config/compress/rollback)                                              | ✅        | ✅        | ✅         | 上下文压缩、状态回滚、配置热更新              |
| **智能体**   | agent (spawn/fanout/get_result/run_chain)                                       | ✅        | ✅        | ❌         | 多 Agent 并行/管道/对抗性审查                 |
| **技能**     | skill (预定义工作流)                                                            | ✅        | ✅        | ❌         | 可插拔技能系统                                |
| **通知**     | notify                                                                          | ✅        | ✅        | ❌         | 异步推送通知给用户                            |
| **认证**     | —                                                                               | SSO/OAuth | API Key   | 匿名/OAuth | 多认证后端                                    |

### 8.2 记忆系统
Astra 的记忆系统让 Agent 能够**跨 Session 学习**：

```
┌────────────────────────────────────────────────┐
│              记忆类型                           │
│                                                 │
│  情景记忆 (Episodic)                             │
│  "上周重构 auth 模块时遇到了循环依赖，            │
│   解决方案是把 trait 提取到单独 crate"            │
│                                                 │
│  语义记忆 (Semantic)                             │
│  "项目使用 Tokio 异步运行时，数据库是 PostgreSQL" │
│                                                 │
│  程序记忆 (Procedural)                           │
│  "运行测试的正确命令是 cargo test --workspace"    │
│                                                 │
│  工作记忆 (Working)                              │
│  当前 Session 中的上下文和临时结论               │
└────────────────────────────────────────────────┘
```

**记忆治理**（自动后台运行，无需用户干预）：

- **信任衰减**：长时间未使用的记忆自动降权
- **冲突检测**：新旧记忆冲突时标记并请求确认
- **压缩归档**：低价值记忆压缩后移入冷存储
- **Session 启动预热**：新 Session 自动加载相关记忆

### 8.3 多 Agent 编排
Astra 支持复杂的多 Agent 协作模式：

| 模式                | 描述                                   | 适用场景                               |
| ------------------- | -------------------------------------- | -------------------------------------- |
| **扇出 (Fan-Out)**  | 并行派发任务给多个专业 Agent，汇总结果 | 代码审查 + 安全扫描 + 性能分析同时进行 |
| **管道 (Pipeline)** | 串行传递：分析 → 修复 → 测试 → 审查    | 自动修复流水线                         |
| **对抗性审查**      | A 提出方案，B 批评，A 修改，循环至通过 | 架构设计审查                           |
| **委托 (Delegate)** | 编排 Agent 把子任务委托给专业 Agent    | 复杂任务分解                           |
| **动态组队**        | 根据任务特征自动选择 Agent 组合        | 自适应问题解决                         |

**审查门控**：所有 Agent 产出可选经过另一个 Agent 审查后才交付给用户。

### 8.4 持续学习与自我进化
Astra 平台本身也是一个 Agent——它监控自身运行并自动优化：

```
隐式反馈挖掘
  ↓
用户接受了 Agent 的建议？→ 正面信号
用户修改了 Agent 生成的代码？→ 负面信号 + 差异分析
  ↓
LLM 诊断: 为什么不完美？
  ↓
自动生成改进 (prompt 调整 / 技能参数优化)
  ↓
回归门控: 改进是否让历史任务变差？
  ↓
激活: 如果通过 → 自动部署改进
```

### 8.5 上下文工程
Astra 的上下文工程保证 Agent 始终在正确的上下文中工作：

| 能力               | 描述                                                 |
| ------------------ | ---------------------------------------------------- |
| **自动上下文压缩** | 上下文接近预算时自动摘要历史，保持关键信息           |
| **智能文件发现**   | 根据任务自动搜索并加载相关文件（无需显式声明）       |
| **记忆注入**       | Session 启动时加载相关记忆，执行中按需召回           |
| **项目规则感知**   | 自动读取 .astra/rules 和项目约定的编码规范           |
| **上下文优先级**   | 系统消息 > 项目规则 > 当前任务 > 历史对话 > 工具结果 |

---

## 九、用户旅程

### 9.1 旅程 A：团队代码迁移（Server）
**角色**：Tech Lead 张伟，带领 8 人后端团队  
**任务**：将旧 Java 服务迁移到 Rust，约 50,000 行代码  
**时间线**：2 周

```
Day 1 — 设置
────────────────────────────────────────────────
09:00  管理员李明部署 Astra Server
       → docker pull astra/server:latest
       → 配置 SSO (Okta)，导入团队
       → 设置策略: bash 沙箱强制, 禁止 git force-push
       → 创建项目 workspace，clone 代码仓库
       耗时: 30 分钟

09:30  张伟登录 Web 控制台
       → 看到预配置的 workspace，Rust 工具链已就绪
       → 创建第一个 Session: "从 UserService.java 开始"

10:00  第一次交互
       张伟: "分析 UserService.java，生成对应的 Rust 实现方案"

       张伟看不到的后台:
        · Agent 调用 read_file → LocalExecutor → 读取 UserService.java
        · Agent 调用 symbols → LocalExecutor → 解析 Java 类结构
        · Agent 调用 web_search → NetworkProvider → 搜索 Java→Rust 迁移最佳实践
        · 每个调用被策略引擎检查、记录到审计日志

10:05  Agent 返回分析报告:
        · UserService 有 3 个 public 方法: createUser, authenticate, updateProfile
        · 依赖: DatabasePool, PasswordHasher, EmailSender
        · 建议: 使用 axum + sqlx，PasswordHasher → argon2 crate
        · 风险: authenticate 中有自定义 JWT 逻辑，需要仔细迁移

11:00  张伟: "先迁移 createUser 方法，包括所有依赖"
        → Agent 创建 Task 清单跟踪进度

12:00  张伟去吃午饭，Agent 继续工作
        · 在沙箱中生成 Rust 代码
        · 运行 cargo check → 编译错误 → 自动修复 → 再次检查 → 通过
        · 生成单元测试 → cargo test → 3 passed
        执行环境: gVisor 微虚拟机，4 CPU, 8GB 内存

14:00  张伟回来
        → 查看 Agent 生成的代码 diff → 满意，但改了 error 类型
        → "用 thiserror 而不是 anyhow"
        → Agent 理解并修改

Day 7 — 进度过半
────────────────────────────────────────────────
       · 已完成 7 个 Service 的迁移
       · Agent 自动发现并修复了 3 个跨 Service 的接口不一致
       · 测试覆盖率: 82%

       张伟: "把所有迁移完成的模块做一个 PR"
       → Agent 调用 git 创建分支，提交变更
       → Agent 调用 github create_pr → 生成 PR 描述，标记 reviewers
       → CI 触发 → 通过 ✅

       张伟注意到: 上周二 Agent 遇到过类似的 serde 序列化问题，
       这周遇到时 Agent 直接用了正确的方案 → 记忆系统在工作

Day 14 — 完成
────────────────────────────────────────────────
       · 50,000 行 Java → 43,000 行 Rust
       · 测试覆盖率: 89%
       · 生成 PR: 12 个，全部通过 CI
       · 人手写的代码：约 3,000 行（边界情况、特殊业务逻辑）
       · Agent 独立完成: 约 40,000 行

       审计报告:
        · 工具调用总次数: 14,827
        · 沙箱执行总时长: 127 小时（分布式）
        · 被策略拒绝的调用: 23 次（全部是出站网络访问尝试）
        · Token 消耗: ~$420（使用 Claude Sonnet）
        · 人为错误阻止: 0（张伟记忆中的那个"差点删了配置文件"没发生，
          因为 sandbox 隔离了宿主文件系统）
```

### 9.2 旅程 B：个人日常开发（CLI）
**角色**：全栈开发者陈静，自由职业者  
**场景**：维护 3 个客户项目，每天使用 Astra CLI 提升效率

```
场景 1: 理解新代码 (09:15)
─────────────────────────
  $ cd ~/projects/client-x
  $ astra

  astra> 我刚接手这个 NestJS 项目，帮我梳理认证流程

  Astra 后台:
   · 搜索 auth 相关文件 → 找到 12 个文件
   · 读取 AuthModule, JwtStrategy, AuthGuard
   · 构建调用图: 请求 → AuthGuard → JwtStrategy → AuthService → DB
   · 生成流程图和说明

  输出:
  认证流程分为 3 层:
  1. AuthGuard (全局) — 拦截所有请求
  2. JwtStrategy (Passport) — 验证 JWT token
  3. AuthService — 查询用户、生成 token

  关键文件:
  · src/auth/auth.guard.ts — 入口守卫
  · src/auth/jwt.strategy.ts — JWT 验证逻辑
  · src/auth/auth.service.ts — 核心认证逻辑

  耗时: 8 秒

场景 2: 重构 (14:30)
─────────────────────
  astra> 把 user.service.ts 的错误处理统一改成 Result 模式

  Astra:
   · 分析 user.service.ts → 6 个 public 方法
   · 当前: throw new Error() 模式，3 种不同异常类型
   · 生成重写方案 → 用户确认
   · 执行:
      - str_replace × 12 次（每个方法的错误处理）
      - 更新 imports
      - 更新调用方 (3 个 controller 文件)
   · 运行测试 → 全部通过 ✅

  耗时: 45 秒
  手动操作等效: 约 20 分钟

场景 3: 辅助脚本 (16:00)
────────────────────────
  astra> 写一个脚本分析过去 3 个月的 git log，
        按作者统计 commit 数和代码行数，输出 CSV

  Astra:
   · 在边车子进程中执行 git log
   · 解析输出，统计
   · 生成 CSV 文件
   · 提示: "CSV 已生成: ~/projects/client-x/contributor_stats.csv"

  耗时: 12 秒
```

### 9.3 旅程 C：数据工程师快速原型（Web App）
**角色**：数据分析师王芳，不写生产代码，日常处理数据  
**场景**：用 Astra Web App 快速完成任务

```
周五 15:30 — 接到需求
─────────────────────
  业务团队: "我们需要一份上周用户活跃度的分析报告"
  王芳: (还有 2 小时下班，得快点搞)

15:32  打开 app.astra.dev
       → 上周的 session 还在，之前的 CSV 文件还在

15:33  "帮我从 PostgreSQL 导出上周的 user_events 表到 CSV，
        然后生成一份分析报告"

15:34  Astra:
        · 提示需要数据库连接信息
        · 王芳粘贴 (host, port, db, user, password)
        · Astra: "我会只执行 SELECT 查询，不会修改数据" ← 安全声明

15:35  执行:
        · mo_query: SELECT * FROM user_events
          WHERE event_date BETWEEN '2026-06-08' AND '2026-06-14'
        · 导出 45,678 行到 CSV
        · Python 脚本分析:
          - 日活趋势 (matplotlib 图表)
          - 最活跃 Top 10 用户
          - 事件类型分布
          - 同比上周变化

15:38  生成 Markdown 报告 + 3 张图表
       预览 → 满意 → 下载 Markdown + 图片

16:00  发给业务团队
       业务团队: "太棒了！能加个按地区分布的吗？"

16:02  王芳回到 Astra: "加一个按地区的分析"
       → Astra 在已有 CSV 和脚本基础上添加地区维度
       → 更新报告

16:05  完成。下班。

       全程: 35 分钟 → 其中王芳实际交互约 5 分钟
       替代方案: 手动 SQL + Excel 处理 ≈ 3 小时

幕后:
  · 所有操作在服务端边车容器中执行
  · 数据库密码仅在 Session 内存中，不持久化
  · Session 30 天后自动清理，不留痕迹
  · 无需安装任何软件
```

### 9.4 旅程 D：CI/CD 智能修复（Server + GitHub Integration）
**角色**：DevOps 工程师 David  
**场景**：CI 流水线集成 Astra 自动修复

```
08:30 — CI 告警
────────────────
  GitHub CI: build failed on main branch
  Error: type mismatch in src/api/v2/handler.rs:42

08:31  GitHub Webhook → Astra Server
       → 自动创建修复 Session
       → Agent: "CI build failed, analyzing..."

08:32  Agent 自动操作:
        · 读取失败日志
        · 读取 handler.rs 第 42 行上下文
        · 搜索最近的 commits: 谁改了什么
        · 定位: PR #342 改了 User 结构体，但 handler.rs 没更新

08:33  生成修复:
        · str_replace: 更新 handler.rs 中的 User 类型引用
        · cargo check → 通过 ✅
        · 运行相关测试 → 全部通过 ✅

08:34  Agent 创建修复 PR:
        · 标题: "fix: update handler.rs for User struct change in #342"
        · 描述: 自动生成的修复说明，含根因分析
        · 标签: automated-fix
        · 指派 reviewer: PR #342 的作者

08:35  David 看到通知，审查 diff → 确认正确 → 合并
       CI 重新触发 → 通过 ✅

       人工介入: David 审查 30 秒
       总耗时: 5 分钟（从失败到修复上线）
```

---

## 十、OpenShell 集成

OpenShell 是一个企业级 AI Agent 安全平台，提供策略管理、沙箱编排、审计网关等能力。Astra 与 OpenShell 的集成不是"选 OpenShell 还是 gVisor"，而是在不同架构层上组合使用。

### 10.1 分层看 OpenShell
回顾 Astra 的执行架构分层：

```
控制面  ─ 策略引擎 ─ Astra 自己的策略系统
        ─ 能力注册
        ─ 执行路由
        ─ Session 管理
        ──  ──  ──  ──  ──  ──  ──  ──  ──
执行面  ─ Sandbox Manager ─ 管理沙箱生命周期
        ─ Launch Driver  ─ 创建/销毁容器
        ─ Isolation Backend ─ 隔离边界 (gVisor/runc/MicroVM)
        ──  ──  ──  ──  ──  ──  ──  ──  ──
数据面  ─ 审计证据 ─ 工具调用记录与证据链
```

OpenShell 在不同集成模式下占据**不同层**：

| 层                | Astra 原生                | OpenShell 替代                              |
| ----------------- | ------------------------- | ------------------------------------------- |
| 策略引擎          | ✅ 自有                   | ✅ OpenShell Policy Gateway（更强）         |
| Sandbox Manager   | ✅ 自有（runner-manager） | ✅ OpenShell Gateway                        |
| Launch Driver     | ✅ Docker/containerd      | ✅ OpenShell Gateway（封装）                |
| Isolation Backend | ✅ gVisor/runc 直接调用   | ✅ OpenShell 内部选择（gVisor/MXC/MicroVM） |
| 审计证据          | ✅ 自有收集               | ✅ OpenShell 审计 + Astra 互补              |

### 10.2 集成模式一：OpenShell 作为 Sandbox Manager（推荐）
**OpenShell 替换执行面的 Sandbox Manager + Launch Driver 层。**

```
┌──────────────────────────────────────────┐
│           Astra Control Plane            │
│  策略引擎 · 能力注册 · Agent 编排        │
│  执行路由器                              │
└──────────────────┬───────────────────────┘
                   │ RunnerRpc / OpenShell API
                   ▼
┌──────────────────────────────────────────┐
│        OpenShell Gateway                 │
│  · Session 生命周期管理                   │
│  · Workspace 挂载                        │
│  · Policy 注入 + 强制执行                │
│  · Launch Driver (内部用 K8s/Docker)     │
│  · 审计日志                              │
└──────────────────┬───────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────┐
│     Isolation Backend (OpenShell 选择)    │
│  gVisor / MXC / Firecracker / runc       │
└──────────────────┬───────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────┐
│     Astra Runner (沙箱内进程)             │
│  接收工具调用，执行，返回结果 + 证据       │
└──────────────────────────────────────────┘
```

**适用场景**：

- 企业已有 OpenShell 部署，希望统一管控所有 AI Agent
- 需要最强的策略引擎和合规审计
- 多 Agent 平台统一管控（Astra 只是其中一个 Agent 平台）

**优势**：

- OpenShell 提供企业级的策略管理（比 Astra 自带的更成熟）
- 多平台统一审计（所有 Agent 平台的审计在 OpenShell 一处聚合）
- Astra 无需关心底层用什么隔离后端（OpenShell 内部切换）

**代价**：

- 多一次网络调用（Astra → OpenShell API）
- 依赖 OpenShell 的可用性
- OpenShell 的审计和 Astra 的证据链需要关联

### 10.3 集成模式二：Astra 原生 + OpenShell 策略增强
**Astra 保留执行面的控制权，但策略层对接 OpenShell Policy Gateway。**

```
┌──────────────────────────────────────────┐
│        OpenShell Policy Gateway          │
│  · 企业级策略定义                        │
│  · 跨平台策略同步                        │
│  · 合规报告                              │
└──────────────────┬───────────────────────┘
                   │ 策略查询 API
                   ▼
┌──────────────────────────────────────────┐
│           Astra Control Plane            │
│  策略引擎 ──→ 查询 OpenShell 策略        │
│  执行路由器                              │
│  Session Manager                         │
└──────────────────┬───────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────┐
│       Astra Sandbox Manager              │
│  Docker + gVisor (Astra 自己管理)        │
└──────────────────────────────────────────┘
```

**适用场景**：

- 团队使用 OpenShell 做策略管理，但希望 Astra 控制执行环境
- 不想引入 OpenShell Gateway 的操作复杂度
- 只需要策略层的统一，不需要执行层的统一

**优势**：

- 策略管理和执行管理解耦
- 轻量集成（只需调用 OpenShell 策略 API）
- Astra 执行面完全自主，延迟更低

### 10.4 集成模式三：Astra 原生 + OpenShell 审计聚合
**Astra 使用自己的全部栈，但把审计事件发送到 OpenShell 做聚合分析。**

```
┌──────────────────────────────────────────┐
│           Astra (完整自主)               │
│  策略 · 执行 · 沙箱 · 审计              │
└──────────────────┬───────────────────────┘
                   │ 审计事件流 (Webhook / Event Bridge)
                   ▼
┌──────────────────────────────────────────┐
│        OpenShell Audit Platform          │
│  · 多平台审计聚合                        │
│  · 异常检测                              │
│  · 合规报告生成                          │
└──────────────────────────────────────────┘
```

**适用场景**：

- 安全团队使用 OpenShell 做统一审计和异常检测
- Astra 作为独立平台运行，不依赖 OpenShell 做实时管控
- 只需要事后审计聚合

**优势**：

- 最小集成成本（单向推送审计事件）
- Astra 完全自主，不受 OpenShell 可用性影响
- 审计在 OpenShell 集中分析，异常检测能力更强

### 10.5 集成模式四：混合模式（未来）
**长期来看，Astra 和 OpenShell 的关系是分层协作，而不是二选一**：

```
                    ┌─────────────────────────┐
                    │  企业安全运营中心 (SOC)  │
                    └───────────┬─────────────┘
                                │
                    ┌───────────▼─────────────┐
                    │  OpenShell (统一策略+审计)│
                    │  ┌─────────────────────┐ │
                    │  │ Policy Gateway      │ │
                    │  │ Audit Aggregator    │ │
                    │  │ Threat Detection    │ │
                    │  └─────────┬───────────┘ │
                    └────────────┼─────────────┘
                                 │
              ┌──────────────────┼──────────────────┐
              │                  │                  │
    ┌─────────▼──────┐  ┌───────▼───────┐  ┌───────▼──────┐
    │  Astra Server  │  │ 其他 Agent    │  │  自研 Agent  │
    │                │  │ 平台          │  │  平台        │
    │  · CLI 用户    │  │               │  │              │
    │  · Web 用户    │  │               │  │              │
    │  · CI/CD 集成  │  │               │  │              │
    └────────┬───────┘  └───────────────┘  └──────────────┘
             │
             │ (沙箱管理可委托给 OpenShell 或自主管理)
             │
    ┌────────▼───────┐
    │ gVisor / runc  │
    │ (Astra 直接)   │
    └────────────────┘
```

### 10.6 集成模式选择指南
| 场景                    | 推荐模式                  | 理由                                           |
| ----------------------- | ------------------------- | ---------------------------------------------- |
| 初创团队，快速开始      | 模式三（仅审计聚合）      | 最小成本，Astra 自包含                         |
| 中型企业，已有安全团队  | 模式二（策略增强）        | 统一策略，保留执行控制                         |
| 大型企业，多 Agent 平台 | 模式一（Sandbox Manager） | 统一管控，避免重复建设                         |
| 金融机构，合规强制      | 模式一 + 模式三           | 最强管控 + 统一审计                            |
| 自建机房，无云依赖      | Astra 原生                | OpenShell 目前主要是 SaaS，自建可用 Astra 原生 |
| 混合部署（自建+云）     | 模式四（混合）            | 灵活选择每层的实现                             |

---

## 十一、运维部署

### 11.1 Day 0：规划与准备
**硬件要求**（单节点）：

| 规模                      | CPU                             | 内存     | 磁盘        | 网络     |
| ------------------------- | ------------------------------- | -------- | ----------- | -------- |
| **小**（<10 并发用户）    | 4 核                            | 16 GB    | 100 GB SSD  | 100 Mbps |
| **中**（10-50 并发用户）  | 16 核                           | 64 GB    | 500 GB NVMe | 1 Gbps   |
| **大**（50-200 并发用户） | 32 核                           | 128 GB   | 1 TB NVMe   | 10 Gbps  |
| **集群**（200+ 并发用户） | 3-10 节点，每节点 16 核 / 64 GB | 共享存储 | —           |

**软件依赖**：

| 组件           | 用途           | 是否必需                          |
| -------------- | -------------- | --------------------------------- |
| Docker 24+     | 容器运行时     | Server 模式必需                   |
| containerd     | gVisor 依赖    | 使用 gVisor sandbox 时必需        |
| gVisor (runsc) | 内核级沙箱     | Server 模式推荐                   |
| MatrixOne      | 平台状态数据库 | Server 模式必需（可共用外部实例） |
| Memoria        | Agent 记忆服务 | 推荐（本地 SQLite 可降级）        |

**网络规划**：

```
入站:
  :443 (HTTPS)       — Web Console, API
  :8080 (可选)        — Admin API (建议仅内网)

出站:
  api.anthropic.com   — LLM API (Claude)
  api.openai.com      — LLM API (GPT)
  github.com          — GitHub integration
  registry-1.docker.io — Docker 镜像拉取
  *.openshell.dev     — OpenShell 集成（可选）
```

### 11.2 Day 1：部署
**方式 A：Docker Compose（单机，推荐中小团队）**

```yaml
# docker-compose.yml
version: "3.8"
services:
  astra-server:
    image: astra/server:latest
    ports:
      - "443:443"
    volumes:
      - ./config:/etc/astra
      - user_volumes:/data/astra/user-volumes
      - /var/run/docker.sock:/var/run/docker.sock # 管理沙箱容器
    environment:
      - ASTRA_DEPLOYMENT_PROFILE=server
      - ASTRA_DISABLED_TOOLS=git_push,github_create_issue
      - ASTRA_SANDBOX_BACKEND=gvisor
      - ASTRA_SANDBOX_POOL_WARM=5
      - ASTRA_LLM_API_KEY=${LLM_API_KEY}
      - ASTRA_DATABASE_URL=mysql://matrixone:4000/astra
      - ASTRA_MEMORIA_URL=http://memoria:8080
    depends_on:
      - matrixone
      - memoria
    restart: always

  matrixone:
    image: matrixorigin/matrixone:latest
    volumes:
      - mo_data:/data
    environment:
      - MO_SERVER_PORT=4000

  memoria:
    image: astra/memoria:latest
    volumes:
      - memoria_data:/data

volumes:
  user_volumes:
  mo_data:
  memoria_data:
```

```bash
# 部署
docker compose up -d

# 验证
curl https://astra.internal/health
# {"status": "healthy", "sandbox_pool": 5, "active_sessions": 0}
```

**方式 B：Kubernetes（集群，推荐大型团队）**

```yaml
# k8s/astra-server-deployment.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: astra-server
spec:
  replicas: 3
  selector:
    matchLabels:
      app: astra-server
  template:
    spec:
      containers:
        - name: astra-server
          image: astra/server:latest
          ports:
            - containerPort: 443
          env:
            - name: ASTRA_DEPLOYMENT_PROFILE
              value: "server"
            - name: ASTRA_SANDBOX_BACKEND
              value: "gvisor"
            - name: ASTRA_SANDBOX_POOL_WARM
              value: "10"
            - name: ASTRA_DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: astra-secrets
                  key: database_url
            - name: ASTRA_LLM_API_KEY
              valueFrom:
                secretKeyRef:
                  name: astra-secrets
                  key: llm_api_key
          resources:
            requests:
              cpu: "4"
              memory: "8Gi"
            limits:
              cpu: "8"
              memory: "16Gi"
          volumeMounts:
            - mountPath: /var/run/docker.sock
              name: docker-sock
            - mountPath: /data/astra/user-volumes
              name: user-volumes
      volumes:
        - name: docker-sock
          hostPath:
            path: /var/run/docker.sock
        - name: user-volumes
          persistentVolumeClaim:
            claimName: astra-user-volumes
---
apiVersion: v1
kind: Service
metadata:
  name: astra-server
spec:
  type: LoadBalancer
  ports:
    - port: 443
      targetPort: 443
  selector:
    app: astra-server
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: astra-server-hpa
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: astra-server
  minReplicas: 3
  maxReplicas: 20
  metrics:
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: 70
    - type: Resource
      resource:
        name: memory
        target:
          type: Utilization
          averageUtilization: 80
```

```bash
# 部署
kubectl apply -f k8s/

# 验证
kubectl get pods -l app=astra-server
kubectl get hpa astra-server-hpa
```

### 11.3 Day 2：运维
**监控看板**（Prometheus + Grafana）：

| 指标                                    | 类型      | 告警阈值       |
| --------------------------------------- | --------- | -------------- |
| `astra_sessions_active`                 | Gauge     | —              |
| `astra_tool_calls_total`                | Counter   | —              |
| `astra_tool_calls_duration_p50/p95/p99` | Histogram | p99 > 5s       |
| `astra_tool_calls_errors_total`         | Counter   | rate > 1/min   |
| `astra_sandbox_pool_available`          | Gauge     | < 2            |
| `astra_sandbox_startup_duration_p50`    | Histogram | p50 > 3s       |
| `astra_policy_denials_total`            | Counter   | spike > 50/min |
| `astra_llm_api_errors_total`            | Counter   | rate > 0.1/min |

**日常操作**：

```bash
# 查看当前禁用的工具
curl https://astra.internal/admin/tools/disabled

# 紧急关停命令执行（如发现 0-day）
curl -X PUT https://astra.internal/admin/tools/disabled \
  -H "Content-Type: application/json" \
  -d '{"tool_name": "bash"}'

# 恢复
curl -X DELETE https://astra.internal/admin/tools/disabled/bash

# 查看审计日志
curl "https://astra.internal/admin/audit?user=zhangwei&tool=bash&from=2026-06-01&to=2026-06-14"

# 查看沙箱池状态
curl https://astra.internal/admin/sandbox-pool
# {"available": 8, "in_use": 12, "warming": 2, "max": 30}

# 手动清理过期 session
curl -X POST https://astra.internal/admin/sessions/cleanup \
  -d '{"older_than_days": 30}'
```

**备份策略**：

| 数据              | 备份方式                 | 频率     | 保留       |
| ----------------- | ------------------------ | -------- | ---------- |
| MatrixOne 数据库  | `mo snapshot` / 全量备份 | 每日     | 30 天      |
| UserVolume (文件) | rsync / 对象存储         | 每日增量 | 90 天      |
| 审计日志          | 日志聚合 (Loki / ELK)    | 实时     | 按合规要求 |
| 配置文件          | Git 版本管理             | 每次变更 | 永久       |

**灾难恢复**：

```
场景: Astra Node 全部宕机

恢复步骤:
  1. 启动新 Astra Node (K8s 自动或手动)
  2. MatrixOne 数据仍在（独立部署，不受影响）
  3. UserVolume 从备份恢复（如使用共享存储则自动可用）
  4. 沙箱预热池自动重建
  5. 用户 Session 从 MatrixOne 恢复

  总 RTO: < 5 分钟 (K8s 自动恢复)
         < 30 分钟 (手动恢复 + UserVolume 恢复)

场景: MatrixOne 数据损坏

  1. 从最近快照恢复 MatrixOne
  2. Astra Node 自动重连
  3. 最近的 Session 数据可能丢失（快照点之后的数据）

  总 RTO: < 1 小时
  RPO: < 24 小时（快照频率）
```

### 11.4 配置管理
**多环境配置策略**：

```bash
# 环境差异通过环境变量覆盖
# 基础配置: /etc/astra/server.toml

# 开发环境
export ASTRA_SANDBOX_BACKEND=runc           # 开发不用 gVisor
export ASTRA_LOG_LEVEL=debug
export ASTRA_DISABLED_TOOLS=                # 开发不禁用任何工具

# 生产环境
export ASTRA_SANDBOX_BACKEND=gvisor
export ASTRA_SANDBOX_POOL_WARM=10
export ASTRA_LOG_LEVEL=info
export ASTRA_DISABLED_TOOLS=git_force_push,mo_query_destructive
export ASTRA_AUDIT_RETENTION_DAYS=90
```

**配置项完整参考**：

| 配置项                                | 类型     | 默认值  | 说明                                   |
| ------------------------------------- | -------- | ------- | -------------------------------------- |
| `deployment.profile`                  | string   | `cli`   | 部署形态：server/cli/web               |
| `deployment.disabled_tools`           | string[] | `[]`    | 禁用的工具列表                         |
| `sandbox.backend`                     | string   | `none`  | 沙箱后端：none/docker/gvisor/openshell |
| `sandbox.pool_warm_size`              | int      | `0`     | 预热沙箱数量                           |
| `sandbox.pool_max_size`               | int      | `50`    | 最大沙箱并发数                         |
| `resources.max_cpu_per_sandbox`       | int      | `4`     | 单沙箱 CPU 核数上限                    |
| `resources.max_memory_per_sandbox`    | string   | `"8GB"` | 单沙箱内存上限                         |
| `resources.execution_timeout_seconds` | int      | `600`   | 工具调用超时                           |
| `resources.max_concurrent_executions` | int      | `50`    | 全局并发执行上限                       |
| `storage.user_volume_path`            | string   | —       | UserVolume 存储路径                    |
| `storage.session_retention_days`      | int      | `30`    | Session 保留天数                       |
| `audit.enabled`                       | bool     | `true`  | 是否启用审计                           |
| `audit.retention_days`                | int      | `90`    | 审计日志保留天数                       |
| `network.allowed_domains`             | string[] | `[]`    | 出站域名白名单（空=全部允许）          |
| `network.blocked_domains`             | string[] | `[]`    | 出站域名黑名单                         |

---

## 十二、性能与可扩展性

### 12.1 延迟分析
```
典型工具调用链路延迟分解 (Server 模式):

  策略检查         < 1ms  (内存操作)
  执行器路由       < 1ms  (内存操作)
  沙箱分配         < 5ms  (从预热池获取)
  RunnerRpc 传输   < 2ms  (本地 Unix socket / gRPC)
  工具执行         N ms   (取决于工具本身)
  证据收集         < 1ms
  ─────────────────────────
  平台开销:        < 10ms (不含工具执行时间)
```

**各隔离级别冷/热启动**：

| 隔离级别              | 冷启动  | 热启动（预热池） |
| --------------------- | ------- | ---------------- |
| None (DirectExecutor) | —       | —                |
| Process (边车)        | < 100ms | < 5ms (进程复用) |
| Container (runc)      | < 1s    | < 50ms           |
| Sandbox (gVisor)      | < 3s    | < 50ms           |
| MicroVM (Firecracker) | < 5s    | < 200ms          |

### 12.2 扩展性
**水平扩展**：

```
Astra Node 是无状态的 → 直接加节点

负载均衡策略:
  · Session 粘性: 同一 Session 路由到同一 Node (WebSocket 连接)
  · 新 Session: 轮询到最空闲的 Node
  · 工具调用: 通过 Session 所在 Node 执行

K8s HPA 自动扩缩:
  · CPU > 70% → 加 2 个 Pod
  · CPU < 30% → 减 1 个 Pod
  · 最小 3 Pod，最大 20 Pod
```

**多租户隔离**：

```
                 ┌──────────────────────┐
                 │   物理集群 / 可用区   │
                 └──────────┬───────────┘
                            │
          ┌─────────────────┼─────────────────┐
          │                 │                 │
    ┌─────▼─────┐     ┌─────▼─────┐     ┌─────▼─────┐
    │  租户 A   │     │  租户 B   │     │  租户 C   │
    │           │     │           │     │           │
    │ 独立      │     │ 独立      │     │ 共享      │
    │ MatrixOne │     │ MatrixOne │     │ MatrixOne │
    │ 独立      │     │ 共享      │     │ 共享      │
    │ UserVolume│     │ UserVolume│     │ UserVolume│
    │           │     │           │     │           │
    │ 策略:     │     │ 策略:     │     │ 策略:     │
    │ 允许网络  │     │ 禁止网络  │     │ 只读文件  │
    └───────────┘     └───────────┘     └───────────┘
```

- 租户策略彼此独立
- 沙箱池可配置为共享或独立
- 资源配额按租户设置

### 12.3 可用性
| 组件          | 可用性策略              | 目标       |
| ------------- | ----------------------- | ---------- |
| Astra Node    | 多副本 + HPA            | 99.9%      |
| MatrixOne     | 内置 Raft 集群          | 99.99%     |
| Memoria       | 多副本 + 本地降级       | 99.9%      |
| UserVolume    | 共享存储 (NFS/对象存储) | 99.9%      |
| Docker/gVisor | 宿主机服务              | 跟随宿主机 |

---

## 十三、竞品定位

### 13.1 市场地图
```
              隔离能力
                ▲
                │    E2B          Astra Server
    Firecracker │    (沙箱服务)    (Agent Runtime)
                │
                │    Daytona
    Docker      │    (沙箱服务)
                │
                │    Claude Code  Astra CLI
    Process     │    (编程助手)    (Agent 终端)
                │
                │    ChatGPT      Astra Web
    None        │    (聊天)       (Agent 浏览器)
                │
                └──────────────────────────────►
                    单点工具    Agent 平台    完整运行时
```

### 13.2 差异化
| 维度           | Claude Code / Cursor  | E2B / Daytona | Astra                                |
| -------------- | --------------------- | ------------- | ------------------------------------ |
| **定位**       | AI 编程助手           | 远程代码沙箱  | Agent 执行运行时                     |
| **隔离**       | 无（直接操作本地 FS） | 容器/微虚拟机 | 三级隔离，按工具路由                 |
| **多 Agent**   | 不支持                | 不支持        | 扇出、管道、对抗性审查               |
| **记忆**       | 无跨 session          | 无            | 语义/情景/程序记忆 + 自动治理        |
| **审计**       | 无                    | 基础日志      | 完整证据链，不可篡改                 |
| **运维管控**   | 无                    | API Key 级    | 工具级、用户级、租户级三层管控       |
| **部署形态**   | 本地 CLI              | SaaS          | Server / CLI / Web 三合一            |
| **自我进化**   | 无                    | 无            | 隐式反馈挖掘 → 自动优化              |
| **团队协作**   | 无                    | 无            | 共享 workspace、Agent 委托、审查门控 |
| **CI/CD 集成** | 间接                  | 间接          | 原生 Webhook + 自动修复              |

### 13.3 一句话定位
> **Astra 不是在和 Claude Code 竞争，Astra 是在构建让 Claude Code 类应用可以安全部署到生产环境的基础设施。** 就像 Kubernetes 不是在和你的本地 Docker 竞争——它解决的是不同层次的问题。

---

## 十四、路线图

### Phase 1：坚固的执行基础（当前）

- ✅ 策略驱动的工具路由
- ✅ 多级隔离执行器
- ✅ 部署级工具管控（disabled_tools TOML + 环境变量 + Admin API 实时调整）
- ✅ 审计证据收集
- 🔵 OpenShell 集成（Sandbox Manager 模式）

### Phase 2：完整的 Agent 生命周期

- 记忆系统跨 session 学习
- 多 Agent 编排（扇出/管道/对抗）
- 隐式反馈挖掘 → 自动优化
- UserVolume 持久化存储
- 审查门控

### Phase 3：企业级平台

- SSO / OAuth 集成
- 多租户隔离
- 审计合规报告（SOC 2 / ISO 27001）
- K8s Operator 自动运维
- 多云部署

### Phase 4：生态与平台

- 技能市场（社区贡献技能）
- 自定义能力提供者 SDK
- IDE 插件（VS Code / JetBrains）
- 成本优化（智能模型路由、token 预算管理）
- Agent 评估基准与回归测试框架

---

## 附录 A：术语表

| 术语                                  | 定义                                                            |
| ------------------------------------- | --------------------------------------------------------------- |
| **Agentic Runtime**                   | 管理 AI Agent 生命周期（执行、安全、记忆、审计）的运行时平台    |
| **工具（Tool）**                      | AI Agent 可调用的原子能力，如 read_file、bash、web_search       |
| **能力提供者（Capability Provider）** | 提供一组相关工具的模块，声明这些工具所需的隔离级别和资源        |
| **执行器（Executor）**                | 在特定隔离级别下执行工具调用的组件                              |
| **沙箱（Sandbox）**                   | 隔离的执行环境，Agent 的命令和代码在其中运行                    |
| **边车（Sidecar）**                   | 与主进程分离的独立子进程或容器，提供进程级隔离                  |
| **UserVolume**                        | 用户持久化文件存储，跨 Session 保留 workspace 和构建产物        |
| **Session**                           | 用户与 Agent 的一次交互会话，包含上下文、工具调用记录、中间产物 |
| **证据链（Evidence Chain）**          | 哈希链接的审计记录序列，保证不可篡改                            |
| **预热池（Warm Pool）**               | 预先创建好的沙箱实例池，消除冷启动延迟                          |
| **审查门控（Review Gate）**           | Agent 产出必须经过另一个 Agent（或人类）审查才能交付            |

## 附录 B：参考文档

| 文档                     | 位置                                              |
| ------------------------ | ------------------------------------------------- |
| 架构设计                 | `docs/design/ARCHITECTURE.md`                     |
| 部署架构                 | `docs/design/deployment-architecture.md`          |
| 边云执行                 | `docs/design/edge-cloud-execution.md`             |
| Agent 与编排             | `docs/design/agents-and-orchestration.md`         |
| 安全与信任               | `docs/design/trust-and-safety.md`                 |
| 代码沙箱                 | `docs/design/code-sandbox.md`                     |
| 记忆系统                 | `docs/design/memory/README.md`                    |
| 沙箱架构（含 OpenShell） | `plans/sandbox-architecture.md`                   |
| 执行器架构               | `plans/astra-executor-architecture-2026-06-14.md` |
| 能力提供者模型           | `plans/capability-provider-model-2026-06-14.md`   |

---

> **Astra: 让 AI Agent 安全地操作世界。**
