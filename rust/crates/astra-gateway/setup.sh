#!/bin/bash
# Astra Gateway 一键部署脚本
# 用法: bash gateway-setup.sh
set -e

echo "════════════════════════════════════════════"
echo "  Astra Gateway 部署向导"
echo "════════════════════════════════════════════"
echo ""

# ── Step 1: 检查依赖 ──
echo "📋 检查环境..."

check_cmd() {
    if command -v "$1" &>/dev/null; then
        echo "  ✅ $1"
        return 0
    else
        echo "  ❌ $1 未安装"
        return 1
    fi
}

MISSING=0
check_cmd cargo || MISSING=1
check_cmd mysql || echo "  ⚠️  mysql client 可选（用于验证 DB）"

if [ $MISSING -eq 1 ]; then
    echo ""
    echo "❌ 请先安装缺少的依赖，然后重新运行此脚本。"
    exit 1
fi

echo ""

# ── Step 2: 编译 ──
echo "🔨 编译 astra-gateway (release)..."
cd "$(dirname "$0")"
cargo build --manifest-path rust/Cargo.toml -p astra-gateway -p astra-cli --release 2>&1 | tail -3
echo "  ✅ 编译完成"
echo ""

# ── Step 3: 配置数据库 ──
echo "🗄️  数据库配置"
echo "  Gateway 需要一个 MatrixOne/MySQL 数据库。"

DB_URL="${GATEWAY_DATABASE_URL:-}"
if [ -z "$DB_URL" ]; then
    read -p "  数据库 URL [mysql://root:111@127.0.0.1:6001/astra_gateway]: " DB_URL
    DB_URL="${DB_URL:-mysql://root:111@127.0.0.1:6001/astra_gateway}"
fi
echo "  → $DB_URL"
echo ""

# ── Step 4: 企微配置 ──
echo "🤖 企业微信 AI Bot 配置"
echo ""
echo "  获取步骤:"
echo "  1. 登录企业微信管理后台: https://work.weixin.qq.com"
echo "  2. 应用管理 → 创建应用 → 选择「AI 机器人」"
echo "  3. 创建后获取 Bot ID 和 Secret"
echo "  4. 在「可见范围」中添加需要使用的部门/人员"
echo ""

BOT_ID="${WECOM_BOT_ID:-}"
if [ -z "$BOT_ID" ]; then
    read -p "  Bot ID: " BOT_ID
fi

SECRET="${WECOM_SECRET:-}"
if [ -z "$SECRET" ]; then
    read -sp "  Secret: " SECRET
    echo ""
fi

if [ -z "$BOT_ID" ] || [ -z "$SECRET" ]; then
    echo "  ⚠️  未填写企微凭证，跳过。可以后续设置环境变量:"
    echo "     export WECOM_BOT_ID=your-bot-id"
    echo "     export WECOM_SECRET=your-secret"
fi
echo ""

# ── Step 5: 选择模型 ──
echo "🧠 模型配置"
MODEL="MiniMax-M2.7"
if [ -f .models.yaml ]; then
    echo "  已检测到 .models.yaml，可用模型:"
    grep "^- name:" .models.yaml | sed 's/- name: /    /' | head -10
    read -p "  默认模型 [$MODEL]: " INPUT_MODEL
    MODEL="${INPUT_MODEL:-$MODEL}"
else
    echo "  ⚠️  未检测到 .models.yaml"
    read -p "  默认模型 [$MODEL]: " INPUT_MODEL
    MODEL="${INPUT_MODEL:-$MODEL}"
fi
echo "  → $MODEL"
echo ""

# ── Step 6: 选择 CLI 后端 ──
echo "🔧 CLI 后端"
echo "  Gateway 可以使用不同的 AI agent CLI："
echo "    1. astra (本项目，默认)"
echo "    2. claude (Claude Code)"
echo "    3. codex (OpenAI Codex CLI)"
read -p "  选择 [1]: " CLI_CHOICE
CLI_CHOICE="${CLI_CHOICE:-1}"

case "$CLI_CHOICE" in
    2)
        CLI_TYPE="claude"
        CLI_BIN="claude"
        ;;
    3)
        CLI_TYPE="codex"
        CLI_BIN="codex"
        ;;
    *)
        CLI_TYPE="astra"
        CLI_BIN="$(pwd)/rust/target/release/astra"
        ;;
esac
echo "  → $CLI_TYPE ($CLI_BIN)"
echo ""

# ── Step 7: 生成配置文件 ──
CONFIG_FILE="gateway.yaml"
echo "📝 生成 $CONFIG_FILE..."

cat > "$CONFIG_FILE" <<YAML
# Astra Gateway 配置 (自动生成于 $(date +%Y-%m-%d))
# 重新生成: bash gateway-setup.sh

astra:
  base_url: "http://localhost:8080"
  api_key: ""
  default_model: "$MODEL"

database:
  url: "$DB_URL"

cli:
  type: $CLI_TYPE
  bin: "$CLI_BIN"
$([ "$CLI_TYPE" = "astra" ] && echo "  permission_mode: auto")
$([ "$CLI_TYPE" = "astra" ] && echo "  model: $MODEL")
$([ "$CLI_TYPE" = "claude" ] && echo "  model: null")
$([ "$CLI_TYPE" = "codex" ] && echo "  approval_mode: full-auto")

platforms:
  wecom:
    enabled: $([ -n "$BOT_ID" ] && echo "true" || echo "false")
    bot_id: "$BOT_ID"
    secret: "$SECRET"
YAML

echo "  ✅ $CONFIG_FILE 已生成"
echo ""

# ── Step 8: 启动说明 ──
echo "════════════════════════════════════════════"
echo "  ✅ 配置完成！"
echo "════════════════════════════════════════════"
echo ""
echo "启动 Gateway:"
echo ""
echo "  # 方式 1: 直接运行"
echo "  ./rust/target/release/astra-gateway --config gateway.yaml"
echo ""
echo "  # 方式 2: 通过 make"
echo "  make gateway"
echo ""
echo "  # 方式 3: 环境变量覆盖凭证"
echo "  WECOM_BOT_ID=xxx WECOM_SECRET=yyy ./rust/target/release/astra-gateway --config gateway.yaml"
echo ""
echo "企微使用:"
echo "  在企业微信中找到你创建的 AI Bot，发送消息即可。"
echo "  可用命令: /help /status /inspect /cli /new /cron"
echo ""
