# astra-gateway

通过企业微信远程操控 Astra agent 的网关服务。

## 架构

```
企微用户发消息
  ↓ WeCom AI Bot WebSocket
astra-gateway
  ├── /command → 直接响应 (会话管理、状态查询、定时任务)
  ├── /cron → DB 业务层 (gw_cron_jobs 表)
  └── 普通消息 → spawn astra chat CLI → 完整 agent 能力
      ↓
      astra CLI (harness + tools + session + memory)
      ↓
      stdout JSON → gateway 解析 session_id + text
  ↓
astra-gateway 回复到企微
```

## 快速开始

```bash
# 1. 创建 gateway 数据库
mysql -h 127.0.0.1 -P 6001 -u root -p111 -e "CREATE DATABASE IF NOT EXISTS astra_gateway"

# 2. 启动 Astra server
make dev-start

# 3. 配置
cp rust/crates/astra-gateway/gateway.example.yaml gateway.yaml
export WECOM_BOT_ID="your-bot-id"
export WECOM_SECRET="your-secret"

# 4. 启动网关
cargo run -p astra-gateway --release -- --config gateway.yaml
```

## 用户命令

| 命令 | 说明 |
|------|------|
| `/new` | 新建会话 |
| `/status` | 模型 + 会话 + harness 状态 |
| `/inspect` | harness 详细快照 |
| `/session list` | 历史会话 |
| `/session switch <id>` | 切换会话 |
| `/model` | 当前模型 |
| `/cron list` | 查看定时任务 |
| `/cron add "<expr>" <msg>` | 创建定时任务 |
| `/cron del <id>` | 删除定时任务 |
| `/approve` | 权限说明 |
| `/help` | 帮助 |

## 多用户

每个企微用户 (platform_user_id) 有独立的:
- 会话隔离: 不同用户的 session 互不影响
- 会话历史: 可以切换回之前的对话
- 定时任务: 每个用户有自己的 cron jobs

用户身份由企微平台保证（gateway 信任 platform_user_id）。

## 定时任务

```
用户: /cron add "0 9 * * 1-5" 汇报昨天的 git commit
Bot:  ✅ 定时任务已创建
      - ID: abc12345
      - 表达式: 0 9 * * 1-5
      - 任务: 汇报昨天的 git commit
```

每个工作日早上 9 点，scheduler 自动执行 `astra chat -m "汇报昨天的 git commit"` 并将结果发到企微。

## 数据库

Gateway 使用独立的 MatrixOne 数据库 (`astra_gateway`)，与 Astra server 数据库隔离。

| 表 | 用途 |
|----|------|
| `gw_users` | 用户 profile |
| `gw_sessions` | 会话映射 + 历史 |
| `gw_cron_jobs` | 定时任务 |

## 测试

```bash
cargo test -p astra-gateway
# 33 tests: config, session, dedup, wecom, commands, scheduler
```
