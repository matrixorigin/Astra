# mo-agent Documentation

Welcome to mo-agent documentation! This guide will help you find the information you need.

## 🚀 Quick Start

**New to mo-agent?** Start here:

- [5-Minute Quick Start](quickstart/README.md) - Get up and running fast
- [Development Environment](quickstart/development.md) - Set up local development
- [Docker Deployment](quickstart/docker.md) - Run with Docker
- [Production Deployment](quickstart/production.md) - Deploy to production

## 📘 Guides

**Learn how to use mo-agent:**

- [Development Workflow](guides/development-workflow.md) - Daily development commands and workflows
- [Testing Guide](guides/testing.md) - Run and write tests
- [Deployment Guide](guides/deployment.md) - Deploy to various environments
- [Troubleshooting](guides/troubleshooting.md) - Common issues and solutions

## 📚 Reference

**Detailed reference documentation:**

- [API Reference](reference/api-reference.md) - Complete API endpoint documentation
- [Makefile Commands](reference/makefile-commands.md) - All available make commands
- [CLI Commands](reference/cli-commands.md) - mo-agent and mo-admin CLI reference
- [Configuration](reference/configuration.md) - Environment variables and configuration
- [Dependencies](reference/dependencies.md) - Dependency groups, installation, and optional extras

## 🏗️ Design & Architecture

**Understand the system design:**

- [Architecture](design/ARCHITECTURE.md) - System overview and data flow
- [Memory Architecture](design/memory/README.md) - Cognitive memory and context engineering
- [Context Window Management](design/context-window-management.md) - Token budgets, history compression, procedural memory injection
- [Tool Result Quality Firewall](design/tool-result-quality-firewall.md) - Pre-LLM tool result quality assessment and annotation
- [Trust and Safety](design/trust-and-safety.md) - Audit, guardrails, and robustness
- [Skills and Tools](design/skills-and-tools.md) - Skill architecture and marketplace
- [Agents and Orchestration](design/agents-and-orchestration.md) - ChatLoop, planning, and teams
- [Data Versioning](design/data-versioning.md) - Snapshot, clone, and branch workflows
- [Evaluation and Evolution](design/evaluation-and-evolution.md) - Quality and CI/CD

## 🔧 Implementation Details

**Deep dive into implementation:**

- [Authentication](implementation/authentication.md) - JWT and authorization
- [LLM Integration](implementation/llm-integration.md) - Provider routing and cost tracking
- [GitHub Integration](implementation/github-integration.md) - Repository operations
- [Deployment Details](implementation/deployment.md) - Project structure and Docker
- [Scope Configuration](implementation/scope-configuration.md) - Scope-based config resolution
- [CI/CD](implementation/ci.md) - GitHub Actions workflows

## 🆘 Need Help?

- Check [Troubleshooting Guide](guides/troubleshooting.md)
- Review [API Documentation](reference/api-reference.md)
- See [Examples](../examples/)

## 📖 Documentation Structure

```
docs/
├── README.md (you are here)
├── quickstart/          # Get started in minutes
├── guides/              # How-to guides
├── reference/           # Detailed reference
├── design/              # Architecture and design
└── implementation/      # Implementation details
```
