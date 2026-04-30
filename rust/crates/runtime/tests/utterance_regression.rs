//! # Utterance Regression Tests
//!
//! Comprehensive regression suite testing tool selection behavior for ALL
//! common user utterance patterns. Uses data-driven tables for signal detection
//! and tool selection, with individual tests for complex/unique assertions.

use astra_runtime::pipeline::routing::DomainHint;
use astra_runtime::tool_registry::ToolRegistry;
use astra_runtime::tool_registry::{
    ConversationState, IntentType, TOOL_CATALOG, pre_filter_dynamic,
};
use astra_runtime::tool_selector::compute_selection_confidence;
use astra_runtime::tool_selector::{SelectionContext, TfIdfSelector, ToolSelector};

// ─── Test helpers ────────────────────────────────────────────────────────────

struct SelectionSnapshot {
    signal_count: usize,
    is_fetch: bool,
    is_mutate: bool,
    is_github: bool,
    is_git: bool,
    is_analytical: bool,
    is_conversational: bool,
    is_memory: bool,
    references_history: bool,
    dynamic_tools: Vec<(String, Vec<IntentType>)>,
    confidence: f64,
}

impl SelectionSnapshot {
    fn from_query(query: &str) -> Self {
        let state = ConversationState::from_message(query, 1);
        let results = pre_filter_dynamic(&state, query);

        let pinned: std::collections::HashSet<&str> = TOOL_CATALOG
            .iter()
            .filter(|t| t.pinned)
            .map(|t| t.name)
            .collect();

        let dynamic_tools: Vec<(String, Vec<IntentType>)> = results
            .iter()
            .map(|&(idx, _)| {
                let t = &TOOL_CATALOG[idx];
                (t.name.to_string(), t.intents.to_vec())
            })
            .collect();

        let dynamic_count = dynamic_tools
            .iter()
            .filter(|(name, _)| !pinned.contains(name.as_str()))
            .count();

        let confidence = compute_selection_confidence(state.signal_count(), dynamic_count);

        Self {
            signal_count: state.signal_count(),
            is_fetch: state.is_fetch,
            is_mutate: state.is_mutate,
            is_github: state.is_github,
            is_git: state.is_git,
            is_analytical: state.is_analytical,
            is_conversational: state.is_conversational,
            is_memory: state.is_memory,
            references_history: state.references_history,
            dynamic_tools,
            confidence,
        }
    }

    fn has_intent(&self, intent: IntentType) -> bool {
        self.dynamic_tools
            .iter()
            .any(|(_, intents)| intents.contains(&intent))
    }

    fn tool_names(&self) -> Vec<&str> {
        self.dynamic_tools.iter().map(|(n, _)| n.as_str()).collect()
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SIGNAL DETECTION — Data-driven tests for is_github/is_git/is_fetch/etc.
// ═══════════════════════════════════════════════════════════════════════════════

/// Tests that specific queries trigger expected signal flags.
/// Each entry: (query, expected_flags_description, assertion_closure)
#[test]
fn signal_github_queries() {
    let cases: &[(&str, &[&str])] = &[
        ("matrixorigin最新的pr", &["is_github", "is_fetch"]),
        ("看看matrixorigin有哪些issue", &["is_github", "is_fetch"]),
        ("memoria最新的ci状态", &["is_github", "is_fetch"]),
        ("给matrixorigin创建一个issue", &["is_github", "is_mutate"]),
        ("matrixorigin仓库的情况", &["is_github", "is_fetch"]),
        (
            "list all open pull requests for memoria",
            &["is_github", "is_fetch"],
        ),
        ("show me PR #123 details", &["is_github", "is_fetch"]),
        ("check CI status for the main branch", &["is_github"]),
        (
            "create a new issue for this bug",
            &["is_github", "is_mutate"],
        ),
        (
            "show me the github actions status",
            &["is_github", "is_fetch"],
        ),
        ("show repository information", &["is_github", "is_fetch"]),
    ];
    for &(query, flags) in cases {
        let s = SelectionSnapshot::from_query(query);
        for &flag in flags {
            let ok = match flag {
                "is_github" => s.is_github,
                "is_fetch" => s.is_fetch,
                "is_mutate" => s.is_mutate,
                _ => panic!("unknown flag {flag}"),
            };
            assert!(ok, "query '{query}': expected {flag}=true");
        }
    }
}

#[test]
fn signal_github_queries_have_github_intent() {
    let cases = [
        "matrixorigin最新的pr",
        "看看matrixorigin有哪些issue",
        "memoria最新的ci状态",
        "给matrixorigin创建一个issue",
        "list all open pull requests for memoria",
        "show me PR #123 details",
        "check CI status for the main branch",
        "create a new issue for this bug",
        "show me the github actions status",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.has_intent(IntentType::GitHub),
            "query '{query}': should include GitHub tools"
        );
    }
}

#[test]
fn signal_git_queries() {
    let cases: &[(&str, &[&str])] = &[
        ("看看git diff", &["is_git"]),
        ("查看最近的提交记录", &["is_git", "is_fetch"]),
        ("当前分支是什么", &["is_git", "is_fetch"]),
        ("合并这个分支到main", &["is_git"]),
        ("git status", &["is_git"]),
        (
            "show me the git log for the last 5 commits",
            &["is_git", "is_fetch"],
        ),
        ("show me the diff", &["is_git"]),
        ("create a new branch from main", &["is_git", "is_mutate"]),
        ("rebase this branch onto main", &["is_git"]),
        ("stash my current changes", &["is_git"]),
    ];
    for &(query, flags) in cases {
        let s = SelectionSnapshot::from_query(query);
        for &flag in flags {
            let ok = match flag {
                "is_git" => s.is_git,
                "is_fetch" => s.is_fetch,
                "is_mutate" => s.is_mutate,
                _ => panic!("unknown flag {flag}"),
            };
            assert!(ok, "query '{query}': expected {flag}=true");
        }
    }
}

#[test]
fn signal_git_queries_have_git_intent() {
    let cases = [
        "看看git diff",
        "git status",
        "show me the git log for the last 5 commits",
        "show me the diff",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.has_intent(IntentType::Git),
            "query '{query}': should include Git tools"
        );
    }
}

#[test]
fn signal_mutate_queries() {
    let cases = [
        "修复main.rs里的编译错误",
        "新建一个配置文件",
        "修改这个函数的返回值",
        "删除这个临时文件",
        "fix the failing test in auth.rs",
        "add a new validation function",
        "remove the unused imports",
        "create a new auth module",
        "write unit tests for the parser",
        "update the database config",
        "改一下这个变量名",
        "添加一个新字段",
        "change the timeout to 30 seconds",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(s.is_mutate, "query '{query}': expected is_mutate=true");
    }
}

#[test]
fn signal_fetch_queries() {
    let cases = [
        "查看main.rs的内容",
        "列出src目录下所有rust文件",
        "这个文件里有什么",
        "show me the contents of config.toml",
        "get the current project status",
        "check what's in the test directory",
        "tell me about the project structure",
        "看一下配置文件",
        "查看 src/main.rs 的内容",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(s.is_fetch, "query '{query}': expected is_fetch=true");
    }
}

#[test]
fn signal_analytical_queries() {
    let cases = [
        "分析一下这段代码的性能",
        "为什么这个测试会失败",
        "explain how this function works",
        "debug the authentication issue",
        "什么原因导致测试失败",
        "what went wrong with the deployment",
        "what's the root cause of this failure",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.is_analytical,
            "query '{query}': expected is_analytical=true"
        );
    }
}

#[test]
fn signal_memory_queries() {
    let cases = [
        "我关注matrixorigin",
        "I prefer Rust for systems programming",
        "remember that I use PostgreSQL",
        "I'm interested in the new release features",
        "保存到记忆",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(s.is_memory, "query '{query}': expected is_memory=true");
    }
}

#[test]
fn signal_history_references() {
    let cases = [
        "之前讨论的那个方案",
        "上次你说的那个工具",
        "刚才的结果不对",
        "查看历史对话",
        "刚刚那个结果呢",
        "之前说过的那个方案",
        "前面提到的函数",
        "上次的改动还在吗",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.references_history,
            "query '{query}': expected references_history=true"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// CONVERSATIONAL — must return 0 dynamic tools
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn conversational_queries_return_empty() {
    let cases = [
        "你好",
        "谢谢",
        "再见",
        "好的",
        "是的",
        "嗯",
        "hi",
        "hello",
        "thanks",
        "thank you",
        "bye",
        "ok",
        "yes",
        "no",
        "nope",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.is_conversational,
            "query '{query}': expected is_conversational"
        );
        assert!(
            s.dynamic_tools.is_empty(),
            "query '{query}': conversational should get 0 dynamic tools, got {}",
            s.dynamic_tools.len()
        );
    }
}

#[test]
fn conversational_words_in_longer_query_dont_short_circuit() {
    // "thanks" + action intent should not be purely conversational
    let s = SelectionSnapshot::from_query("thanks, now fix the bug in parser.rs");
    assert!(
        !s.is_conversational || s.is_mutate,
        "Long message with 'thanks' + 'fix' should not be purely conversational"
    );

    let s = SelectionSnapshot::from_query("好的，帮我查看一下代码");
    assert!(
        s.is_fetch || !s.dynamic_tools.is_empty(),
        "Long message should not be purely conversational"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// TOOL SELECTION — verify specific tools are selected for specific queries
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn tool_selection_mo_query() {
    let cases = [
        "统计一下数据库里的记录数",
        "aggregate the sales data",
        "分析数据趋势",
        "查询数据库中的用户表",
        "run a SQL query to check the data",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"mo_query"),
            "query '{query}': should select mo_query. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_reflect() {
    let cases = [
        "诊断一下这个问题",
        "排查一下原因",
        "diagnose the issue",
        "what happened with that last operation",
        "出了什么问题",
        "哪里出错了",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"reflect"),
            "query '{query}': should select reflect. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_github_create_issue() {
    let cases = ["新建一个issue", "创建问题"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"github_create_issue"),
            "query '{query}': should select github_create_issue. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_web_fetch() {
    let cases = [
        "打开链接",
        "open this link https://example.com",
        "fetch the content from https://example.com",
        "打开这个网址看看内容",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"web_fetch"),
            "query '{query}': should select web_fetch. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_git_diff() {
    let cases = ["看改动", "review changes"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"git_diff"),
            "query '{query}': should select git_diff. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_git_contributors() {
    let cases = [
        "who worked on this file",
        "谁做的这个功能",
        "who are the top contributors",
        "这个项目的贡献者有谁",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"git_contributors"),
            "query '{query}': should select git_contributors. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_git_status() {
    let cases = ["改了吗"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"git_status"),
            "query '{query}': should select git_status. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_git_show() {
    let cases = ["show commit abc123", "这个提交改了什么"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"git_show"),
            "query '{query}': should select git_show. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_git_file_history() {
    let cases = [
        "show file history for main.rs",
        "这个文件的文件历史是什么",
        "查看文件改动记录",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"git_file_history"),
            "query '{query}': should select git_file_history. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_git_log_search() {
    let cases = [
        "search commits for authentication fix",
        "搜索提交记录中有关认证的修改",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"git_log_search"),
            "query '{query}': should select git_log_search. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_github_repo_stats() {
    let cases = ["how many stars does this repo have", "多少star了"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"github_repo_stats"),
            "query '{query}': should select github_repo_stats. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_get_agent_info() {
    let cases = ["what can you do", "你能做什么", "代理能力有哪些"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"get_agent_info"),
            "query '{query}': should select get_agent_info. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_memory_purge() {
    let cases = [
        "forget what I told you about Python",
        "删除记忆中关于Python的内容",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"memory_purge"),
            "query '{query}': should select memory_purge. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_memory_correct() {
    let cases = [
        "update memory to change my preferred language",
        "修正记忆中我的偏好设置",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"memory_correct"),
            "query '{query}': should select memory_correct. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_memory_profile() {
    let cases = ["what do you know about me", "我的偏好和习惯是什么"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"memory_profile"),
            "query '{query}': should select memory_profile. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_git_blame() {
    let cases = ["who last modified this function", "谁改的这段代码"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"git_blame"),
            "query '{query}': should select git_blame. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_github_ci_status() {
    let cases = [
        "what's the CI build status",
        "CI状态怎么样",
        "check ci status",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"github_ci_status"),
            "query '{query}': should select github_ci_status. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_github_list_prs() {
    let cases = ["show me the github pr list"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"github_list_prs"),
            "query '{query}': should select github_list_prs. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_github_get_pr() {
    let cases = ["what's the merge status of PR #42"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"github_get_pr"),
            "query '{query}': should select github_get_pr. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_github_get_issue() {
    let cases = ["查看issue #10的状态"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"github_get_issue"),
            "query '{query}': should select github_get_issue. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_mo_snapshot() {
    let cases = [
        "create a data snapshot before the experiment",
        "创建一个数据快照",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"mo_snapshot"),
            "query '{query}': should select mo_snapshot. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_mo_branch() {
    let cases = [
        "create an experiment branch for the data",
        "创建实验分支隔离数据",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.tool_names().contains(&"mo_branch"),
            "query '{query}': should select mo_branch. Got: {:?}",
            s.tool_names()
        );
    }
}

#[test]
fn tool_selection_context_analysis() {
    let s = SelectionSnapshot::from_query("刚才的执行运行指标怎么样?");
    assert!(s.tool_names().contains(&"context_analysis"));

    // context_analysis should rank above memory_profile for session queries
    let s = SelectionSnapshot::from_query("这个session的啊");
    let names = s.tool_names();
    let ctx_pos = names
        .iter()
        .position(|n| *n == "context_analysis")
        .unwrap_or(999);
    let mem_pos = names
        .iter()
        .position(|n| *n == "memory_profile")
        .unwrap_or(999);
    assert!(ctx_pos < mem_pos, "Got: {:?}", names);
}

#[test]
fn tool_selection_diagnose() {
    let s = SelectionSnapshot::from_query("check the current session health and tool availability");
    assert!(s.tool_names().contains(&"diagnose"));
}

#[test]
fn tool_selection_git_log() {
    let s = SelectionSnapshot::from_query("看看最近的git log");
    assert!(s.is_git);
    assert!(s.tool_names().contains(&"git_log"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// INVARIANTS — structural properties that must always hold
// ═══════════════════════════════════════════════════════════════════════════════

mod invariants {
    use super::*;

    #[test]
    fn non_conversational_always_gets_tools() {
        let queries = [
            "matrixorigin",
            "我关注这个项目",
            "help me please",
            "show me the code",
            "analyze performance",
            "12345",
            "kubernetes deployment",
            "帮帮我",
            "what about this",
        ];
        for q in &queries {
            let s = SelectionSnapshot::from_query(q);
            assert!(
                !s.dynamic_tools.is_empty(),
                "Non-conversational '{q}' should get >= 1 dynamic tool"
            );
        }
    }

    #[test]
    fn more_signals_higher_confidence() {
        let q0 = SelectionSnapshot::from_query("matrixorigin");
        let q2 = SelectionSnapshot::from_query("show me the github PRs");
        assert!(
            q0.confidence <= q2.confidence,
            "0-signal ({:.2}) should have <= confidence than 2+-signal ({:.2})",
            q0.confidence,
            q2.confidence
        );
    }

    #[test]
    fn github_queries_include_github_tools() {
        let queries = [
            "list PRs",
            "show me the issues",
            "check CI status",
            "create an issue",
            "github actions",
            "matrixorigin最新的pr",
        ];
        for q in &queries {
            let s = SelectionSnapshot::from_query(q);
            assert!(
                s.has_intent(IntentType::GitHub),
                "GitHub query '{q}' should include GitHub tools, got: {:?}",
                s.tool_names()
            );
        }
    }

    #[test]
    fn git_queries_include_git_tools() {
        let queries = [
            "git status",
            "show me the diff",
            "git log",
            "rebase onto main",
            "stash changes",
        ];
        for q in &queries {
            let s = SelectionSnapshot::from_query(q);
            assert!(
                s.has_intent(IntentType::Git),
                "Git query '{q}' should include Git tools, got: {:?}",
                s.tool_names()
            );
        }
    }

    #[test]
    fn commit_history_includes_git_tools() {
        let s = SelectionSnapshot::from_query("commit history");
        assert!(s.is_git);
        assert!(
            s.has_intent(IntentType::Git) || !s.dynamic_tools.is_empty(),
            "Should get some tools for git query"
        );
    }

    #[test]
    fn signal_count_consistent() {
        let queries = [
            "hi",
            "show me PRs",
            "analyze why CI failed and fix it",
            "matrixorigin",
            "git diff",
            "create issue",
        ];
        for q in &queries {
            let state = ConversationState::from_message(q, 1);
            let manual_count = [
                state.is_fetch,
                state.is_mutate,
                state.is_github,
                state.is_git,
                state.is_analytical,
                state.references_history,
            ]
            .iter()
            .filter(|&&x| x)
            .count();
            assert_eq!(
                state.signal_count(),
                manual_count,
                "signal_count() mismatch for '{q}': method={} manual={}",
                state.signal_count(),
                manual_count
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MIXED INTENT — queries that fire multiple signals
// ═══════════════════════════════════════════════════════════════════════════════

mod mixed_intent {
    use super::*;

    #[test]
    fn fetch_plus_mutate() {
        let s = SelectionSnapshot::from_query("show me the PRs and create a new issue");
        assert!(s.is_fetch);
        assert!(s.is_mutate);
        assert!(s.is_github);
        assert!(s.signal_count >= 3, "Multiple signals: {}", s.signal_count);
    }

    #[test]
    fn github_plus_git() {
        let s = SelectionSnapshot::from_query("check the PR diff and git log");
        assert!(s.is_github);
        assert!(s.is_git);
        assert!(s.has_intent(IntentType::GitHub));
        assert!(s.has_intent(IntentType::Git));
    }

    #[test]
    fn analytical_plus_fetch() {
        let s = SelectionSnapshot::from_query("why did the latest CI fail");
        assert!(s.is_analytical);
        assert!(s.is_fetch);
        assert!(s.is_github);
    }

    #[test]
    fn history_plus_analytical() {
        let s = SelectionSnapshot::from_query("分析一下之前的决策是否正确");
        assert!(s.is_analytical);
        assert!(s.references_history);
    }

    #[test]
    fn memory_plus_github() {
        let s = SelectionSnapshot::from_query("记住matrixorigin这个repo，然后查看它的PR");
        assert!(s.is_github);
        assert!(s.is_fetch);
        assert!(s.has_intent(IntentType::GitHub));
    }

    #[test]
    fn cn_three_intents() {
        let s = SelectionSnapshot::from_query("查看git log，分析为什么CI失败，然后修复");
        assert!(s.is_fetch);
        assert!(s.is_git);
        assert!(s.is_analytical);
        assert!(s.is_mutate);
        assert!(s.is_github);
        assert!(
            s.signal_count >= 4,
            "Should have 4+ signals: {}",
            s.signal_count
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// VAGUE / AMBIGUOUS — queries with 0 signals that still get tools
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn vague_queries_not_conversational() {
    let cases = [
        "帮帮忙",
        "看看这个",
        "怎么搞",
        "接下来怎么办",
        "help me with this",
        "kubernetes",
        "matrixorigin的东西",
        "what about",
    ];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            !s.is_conversational,
            "query '{query}': should not be conversational"
        );
    }
}

#[test]
fn vague_queries_still_get_tools() {
    let cases = ["帮我写一个排序算法", "help me with this", "kubernetes"];
    for query in &cases {
        let s = SelectionSnapshot::from_query(query);
        assert!(
            s.dynamic_tools.len() >= 3,
            "query '{query}': should get tools"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// EDGE CASES — boundary conditions, special characters, etc.
// ═══════════════════════════════════════════════════════════════════════════════

mod edge_cases {
    use super::*;

    #[test]
    fn empty_and_special_inputs() {
        // None of these should panic
        let cases = [
            ("", 0),
            ("a", 0),
            ("12345", 0),
            ("!@#$%^&*()", 0),
            ("???", 0),
            ("...", 0),
        ];
        for (query, expected_signals) in &cases {
            let s = SelectionSnapshot::from_query(query);
            assert_eq!(s.signal_count, *expected_signals, "query '{query}'");
        }
    }

    #[test]
    fn very_long_query() {
        let long = "please analyze the code ".repeat(100);
        let s = SelectionSnapshot::from_query(&long);
        assert!(s.is_analytical);
    }

    #[test]
    fn emoji_handling() {
        let s = SelectionSnapshot::from_query("👍");
        assert!(s.is_conversational, "Pure emoji should be conversational");

        let s = SelectionSnapshot::from_query("🔥 show me the PRs");
        assert!(s.is_fetch);
        assert!(s.is_github);
    }

    #[test]
    fn repeated_keywords() {
        let s = SelectionSnapshot::from_query("list list list show show show");
        assert!(s.is_fetch);
        assert_eq!(
            s.signal_count, 1,
            "Multiple keywords in same signal = still 1"
        );
    }

    #[test]
    fn whitespace_and_newlines() {
        let s = SelectionSnapshot::from_query("   show   me   the   diff   ");
        assert!(!s.is_conversational);

        let s = SelectionSnapshot::from_query("show me the PRs\nand create an issue");
        assert!(!s.is_conversational);
    }

    #[test]
    fn japanese_cjk_overlap() {
        let s = SelectionSnapshot::from_query("分析してください");
        assert!(
            s.is_analytical,
            "'分析' should trigger even in Japanese context"
        );
    }

    #[test]
    fn code_and_errors_as_queries() {
        let s = SelectionSnapshot::from_query("fn main() { println!(\"hello\"); }");
        assert!(!s.is_conversational);

        let s = SelectionSnapshot::from_query("error[E0308]: mismatched types");
        assert!(!s.is_conversational);

        let s = SelectionSnapshot::from_query("src/main.rs:42");
        assert!(!s.is_conversational);
    }

    #[test]
    fn url_triggers_github() {
        let s = SelectionSnapshot::from_query("https://github.com/matrixorigin/matrixone");
        assert!(s.is_github);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TRIGGER QUALITY — structural rules on TOOL_CATALOG
// ═══════════════════════════════════════════════════════════════════════════════

mod trigger_quality {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn no_exact_duplicate_triggers_across_tools() {
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for tool in TOOL_CATALOG {
            for &trigger in tool.triggers {
                if let Some(&other) = seen.get(trigger)
                    && other != tool.name
                {
                    panic!(
                        "Duplicate trigger '{trigger}' in both '{other}' and '{}'",
                        tool.name
                    );
                }
                seen.insert(trigger, tool.name);
            }
        }
    }

    #[test]
    fn no_duplicate_triggers_within_any_tool() {
        for tool in TOOL_CATALOG.iter() {
            let mut seen = std::collections::HashSet::new();
            for trigger in tool.triggers {
                assert!(
                    seen.insert(trigger),
                    "Duplicate trigger '{trigger}' in tool '{}'",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn no_single_char_cjk_triggers() {
        for tool in TOOL_CATALOG {
            for &trigger in tool.triggers {
                let cjk_chars: Vec<char> = trigger
                    .chars()
                    .filter(|c| ('\u{4E00}'..='\u{9FFF}').contains(c))
                    .collect();
                if !cjk_chars.is_empty() && trigger.chars().count() <= 1 {
                    panic!(
                        "Tool '{}' has single-char CJK trigger '{trigger}'",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn no_ultra_short_english_triggers() {
        for tool in TOOL_CATALOG {
            for &trigger in tool.triggers {
                if trigger.is_ascii() && trigger.len() <= 2 {
                    panic!(
                        "Tool '{}' has ultra-short trigger '{trigger}' (<=2 chars)",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn no_cjk_triggers_that_are_only_common_particles() {
        let dangerous: &[char] = &['有', '是', '的', '了', '在', '不', '这', '那', '我', '你'];
        for tool in TOOL_CATALOG {
            for &trigger in tool.triggers {
                let chars: Vec<char> = trigger.chars().collect();
                let is_cjk = chars.iter().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(c));
                if is_cjk && chars.len() <= 2 && chars.iter().all(|c| dangerous.contains(c)) {
                    panic!(
                        "Tool '{}' has CJK trigger '{trigger}' of only common particles",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn minimum_trigger_count() {
        for tool in TOOL_CATALOG {
            assert!(
                tool.triggers.len() >= 4,
                "Tool '{}' has only {} triggers",
                tool.name,
                tool.triggers.len()
            );
        }
    }

    #[test]
    fn save_disambiguation() {
        let wf = TOOL_CATALOG
            .iter()
            .find(|t| t.name == "write_file")
            .unwrap();
        let ms = TOOL_CATALOG
            .iter()
            .find(|t| t.name == "memory_store")
            .unwrap();
        assert!(
            !wf.triggers.contains(&"保存"),
            "write_file should use '保存文件' not bare '保存'"
        );
        assert!(
            ms.triggers.contains(&"保存"),
            "memory_store should keep '保存'"
        );
    }

    #[test]
    fn remember_not_on_search() {
        let ms = TOOL_CATALOG
            .iter()
            .find(|t| t.name == "memory_search")
            .unwrap();
        assert!(!ms.triggers.contains(&"remember"));
    }

    #[test]
    fn no_bare_pr_or_ci_trigger() {
        for tool in TOOL_CATALOG {
            assert!(
                !tool.triggers.contains(&"pr"),
                "Tool '{}' has bare 'pr'",
                tool.name
            );
            assert!(
                !tool.triggers.contains(&"ci"),
                "Tool '{}' has bare 'ci'",
                tool.name
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// CROSS-DOMAIN RANKING — verify tools rank correctly relative to each other
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn memory_query_ranks_memory_above_github() {
    let s = SelectionSnapshot::from_query("我有哪些记忆？");
    assert!(s.is_memory);
    let names = s.tool_names();
    let mem_pos = names
        .iter()
        .position(|n| n.contains("memory"))
        .unwrap_or(999);
    let gh_pos = names
        .iter()
        .position(|n| *n == "github_list_prs")
        .unwrap_or(999);
    assert!(
        mem_pos < gh_pos,
        "memory tools should rank above github. Got: {:?}",
        names
    );
}

#[test]
fn git_query_ranks_git_above_mo_query() {
    let s = SelectionSnapshot::from_query("show me recent commits");
    let names = s.tool_names();
    let git_pos = names
        .iter()
        .position(|n| n.starts_with("git_"))
        .unwrap_or(999);
    let mo_pos = names.iter().position(|n| *n == "mo_query").unwrap_or(999);
    assert!(
        git_pos < mo_pos,
        "git tools should rank above mo_query. Got: {:?}",
        names
    );
}

#[test]
fn memory_query_ranks_memory_first() {
    let s = SelectionSnapshot::from_query("搜索一下记忆");
    assert!(s.is_memory);
    let names = s.tool_names();
    if let Some(first) = names.first() {
        assert!(
            first.contains("memory") || *first == "reflect",
            "memory query first result should be memory-related, got: {first}"
        );
    }
}

#[test]
fn git_query_ranks_git_first() {
    let s = SelectionSnapshot::from_query("show me the git log");
    assert!(s.is_git);
    let names = s.tool_names();
    if let Some(first) = names.first() {
        assert!(
            first.starts_with("git_") || first.starts_with("mo_"),
            "git query first result should be git-related, got: {first}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// EDGE CASE REGRESSION — code-switching, typos, paths, negation, multiline
// ═══════════════════════════════════════════════════════════════════════════════

mod edge_case_regression {
    use super::*;

    #[test]
    fn code_switching() {
        let s = SelectionSnapshot::from_query("看看最近的git log");
        assert!(s.is_git);
        assert!(s.tool_names().contains(&"git_log"));

        let s = SelectionSnapshot::from_query("帮我review一下PR #42");
        assert!(s.is_github);

        let s = SelectionSnapshot::from_query("fix一下这个bug");
        assert!(s.signal_count >= 1);

        let s = SelectionSnapshot::from_query("help me记住这个偏好");
        assert!(s.is_memory);

        let s = SelectionSnapshot::from_query("帮我修改 process_request 函数");
        assert!(s.is_mutate);

        let s = SelectionSnapshot::from_query("查看 src/main.rs 的内容");
        assert!(s.is_fetch);
    }

    #[test]
    fn queries_with_urls() {
        let s = SelectionSnapshot::from_query(
            "check https://github.com/matrixorigin/matrixone/pull/123",
        );
        assert!(s.is_github);

        let s = SelectionSnapshot::from_query("fetch https://example.com/api/data");
        assert!(s.is_fetch);
    }

    #[test]
    fn queries_with_pr_numbers() {
        let s = SelectionSnapshot::from_query("show me PR #1234");
        assert!(s.is_github);

        let s = SelectionSnapshot::from_query("compare PR #123 and PR #456");
        assert!(s.is_github);
    }

    #[test]
    fn negation_queries() {
        let s = SelectionSnapshot::from_query("don't use bash for this, use git diff instead");
        assert!(s.is_git);

        let s = SelectionSnapshot::from_query("不要用grep，用git log search查一下");
        assert!(s.is_git);
    }

    #[test]
    fn multiline_query() {
        let s = SelectionSnapshot::from_query(
            "我需要做三件事：\n1. 检查git状态\n2. 看看PR\n3. 修改代码",
        );
        assert!(s.is_git);
        assert!(s.is_github);
        assert!(s.is_mutate);
    }

    #[test]
    fn queries_with_code_snippets() {
        let s = SelectionSnapshot::from_query(
            "fix this function:\nfn main() {\n    println!(\"hello\");\n}",
        );
        assert!(s.signal_count >= 1);

        let s = SelectionSnapshot::from_query(
            "fix this error: thread 'main' panicked at 'index out of bounds'",
        );
        assert!(s.is_mutate);
    }

    #[test]
    fn followup_patterns() {
        for q in ["yes", "ok do it", "继续", "对"] {
            let s = SelectionSnapshot::from_query(q);
            assert_eq!(s.signal_count, 0, "'{q}' should have no signals");
        }
    }

    #[test]
    fn correction_patterns() {
        let s = SelectionSnapshot::from_query("no wait, check PR #42 instead");
        assert!(s.is_github);

        let s = SelectionSnapshot::from_query("不是那个，查看git log");
        assert!(s.is_git);
    }

    #[test]
    fn git_tag_reference() {
        let s = SelectionSnapshot::from_query("show git diff since tag v1.2.3");
        assert!(s.is_git);
    }

    #[test]
    fn single_word_domain_queries() {
        let s = SelectionSnapshot::from_query("diff");
        assert!(s.is_git);
    }

    #[test]
    fn backtick_references() {
        // Should not crash
        let _ = SelectionSnapshot::from_query("what does `detect_task_type` do?");

        let s = SelectionSnapshot::from_query("fix the `NullPointerException` in login handler");
        assert!(s.is_mutate);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TRIGGER VERIFICATION — tests for specific trigger routing changes
// ═══════════════════════════════════════════════════════════════════════════════

mod trigger_verification {
    use super::*;

    #[test]
    fn save_disambiguation() {
        let s = SelectionSnapshot::from_query("写入文件到磁盘");
        assert!(s.is_mutate);

        let s = SelectionSnapshot::from_query("保存到记忆");
        assert!(s.is_memory);
    }

    #[test]
    fn memory_signal_alignment() {
        let s = SelectionSnapshot::from_query("我关注matrixorigin");
        assert_eq!(s.signal_count, 1);
        assert!(s.is_memory);

        let s = SelectionSnapshot::from_query("I'm interested in Rust performance");
        assert!(s.is_memory);
    }

    #[test]
    fn contamination_prevention() {
        // "我有问题" should NOT strongly match memory_profile
        let s = SelectionSnapshot::from_query("我有问题");
        let names = s.tool_names();
        let mp_pos = names
            .iter()
            .position(|n| *n == "memory_profile")
            .unwrap_or(999);
        assert!(
            mp_pos >= 3,
            "我有问题 should not strongly trigger memory_profile (pos={mp_pos})"
        );

        // "你好" should NOT strongly match get_agent_info
        let s = SelectionSnapshot::from_query("你好");
        let names = s.tool_names();
        let ai_pos = names
            .iter()
            .position(|n| *n == "get_agent_info")
            .unwrap_or(999);
        assert!(
            ai_pos >= 2 || names.is_empty(),
            "你好 should not strongly trigger get_agent_info (pos={ai_pos})"
        );

        // "的" should not cause false matches
        let s = SelectionSnapshot::from_query("这个的确有意思");
        assert_eq!(s.signal_count, 0);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// REAL-WORLD PATTERNS — informal language, multi-step, corrections
// ═══════════════════════════════════════════════════════════════════════════════

mod real_world_patterns {
    use super::*;

    #[test]
    fn informal_cn() {
        let s = SelectionSnapshot::from_query("老哥，查看一下这个代码呗");
        assert!(s.is_fetch);

        for q in ["行吧", "搞起来", "嗯嗯"] {
            let s = SelectionSnapshot::from_query(q);
            assert_eq!(s.signal_count, 0, "'{q}' is conversational/vague");
        }
    }

    #[test]
    fn informal_en() {
        let s = SelectionSnapshot::from_query("pls show me the diff");
        assert!(s.is_fetch || s.is_git);

        let s = SelectionSnapshot::from_query("lgtm");
        assert_eq!(s.signal_count, 0);
    }

    #[test]
    fn multi_step_requests() {
        let s = SelectionSnapshot::from_query("review the code and fix any bugs you find");
        assert!(s.is_mutate);

        let s = SelectionSnapshot::from_query("检查CI状态，如果通过就部署");
        assert!(s.is_github || s.is_fetch);
    }

    #[test]
    fn cjk_balance_fetch() {
        let s = SelectionSnapshot::from_query("最新的消息");
        assert!(s.is_fetch);
    }

    #[test]
    fn all_caps_still_works() {
        let s = SelectionSnapshot::from_query("CHECK GIT STATUS NOW");
        assert!(s.is_git || s.is_fetch);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PIPELINE INTEGRATION — full TfIdfSelector with budget_pressure, restrictions
// ═══════════════════════════════════════════════════════════════════════════════

fn test_registry() -> ToolRegistry {
    let schemas: Vec<serde_json::Value> = TOOL_CATALOG
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": {"type": "object", "properties": {}}
                }
            })
        })
        .collect();
    ToolRegistry::new(schemas)
}

fn make_ctx<'a>(query: &'a str) -> SelectionContext<'a> {
    SelectionContext {
        query,
        turn_count: 1,
        recent_tools: &[],
        budget_tokens: 800,
        boost_terms: vec![],
        budget_pressure: 0.0,
        memory_domain_hints: vec![],
        restricted_tools: vec![],
        file_context: vec![],
        outcome_bias: std::collections::HashMap::new(),
        previous_confidence_fallback: None,
    }
}

mod pipeline_integration {
    use super::*;

    #[tokio::test]
    async fn normal_pressure_selects_more_tools_than_aggressive() {
        let selector = TfIdfSelector::new(test_registry());
        let q = "list github pull requests and check CI status";

        let normal = selector.select(&make_ctx(q)).await;
        let aggressive = selector
            .select(&SelectionContext {
                budget_pressure: 0.9,
                ..make_ctx(q)
            })
            .await;

        assert!(
            normal.tool_names.len() >= aggressive.tool_names.len(),
            "higher pressure should select fewer tools: normal={}, aggressive={}",
            normal.tool_names.len(),
            aggressive.tool_names.len()
        );
    }

    #[tokio::test]
    async fn pressure_doesnt_lose_primary_tool() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                budget_pressure: 0.9,
                ..make_ctx("show open pull requests")
            })
            .await;
        assert!(result.tool_names.contains(&"github_list_prs".to_string()));
    }

    #[tokio::test]
    async fn restricted_tool_excluded() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                restricted_tools: vec!["github_ci_status".to_string()],
                ..make_ctx("show pull requests and check CI")
            })
            .await;
        assert!(!result.tool_names.contains(&"github_ci_status".to_string()));
        assert!(result.tool_names.contains(&"github_list_prs".to_string()));
    }

    #[tokio::test]
    async fn restricted_git_doesnt_affect_github() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                restricted_tools: vec!["git_log".to_string(), "git_diff".to_string()],
                ..make_ctx("list open PRs on github")
            })
            .await;
        assert!(result.tool_names.contains(&"github_list_prs".to_string()));
    }

    #[tokio::test]
    async fn github_domain_hint_helps_entity_query() {
        let selector = TfIdfSelector::new(test_registry());
        let without = selector.select(&make_ctx("matrixorigin")).await;
        let with = selector
            .select(&SelectionContext {
                memory_domain_hints: vec![DomainHint::GitHub],
                ..make_ctx("matrixorigin")
            })
            .await;

        let without_has_github = without.tool_names.iter().any(|n| n.starts_with("github_"));
        let with_has_github = with.tool_names.iter().any(|n| n.starts_with("github_"));
        if !without_has_github {
            assert!(with_has_github, "domain hint should boost github tools");
        }
    }

    #[tokio::test]
    async fn database_domain_hint_boosts_mo_tools() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                memory_domain_hints: vec![DomainHint::Database],
                ..make_ctx("查询一下数据库")
            })
            .await;
        let has_db = result
            .tool_names
            .iter()
            .any(|n| n.contains("mo_") || n.contains("query"));
        assert!(
            has_db,
            "database hint should promote db tools: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn recent_tools_boost_followup() {
        let selector = TfIdfSelector::new(test_registry());
        let recent = vec!["github_list_prs".to_string()];
        let result = selector
            .select(&SelectionContext {
                turn_count: 3,
                recent_tools: &recent,
                ..make_ctx("还有呢？")
            })
            .await;
        let has_github = result.tool_names.iter().any(|n| n.starts_with("github_"));
        assert!(
            has_github,
            "follow-up should include github tools: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn recent_tools_dont_overpower_new_intent() {
        let selector = TfIdfSelector::new(test_registry());
        let recent = vec!["github_list_prs".to_string()];
        let result = selector
            .select(&SelectionContext {
                turn_count: 3,
                recent_tools: &recent,
                ..make_ctx("check git diff for the latest changes")
            })
            .await;
        let has_git = result.tool_names.iter().any(|n| n.starts_with("git_"));
        assert!(
            has_git,
            "explicit git intent should override recent github: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn cjk_queries_full_pipeline() {
        let selector = TfIdfSelector::new(test_registry());

        let result = selector
            .select(&make_ctx("matrixorigin memoria 最新的pr?"))
            .await;
        assert!(result.tool_names.contains(&"github_list_prs".to_string()));

        let result = selector.select(&make_ctx("我有哪些记忆？")).await;
        assert!(result.tool_names.iter().any(|n| n.contains("memory")));

        let result = selector
            .select(&make_ctx("谁改了这个文件？最近的提交记录"))
            .await;
        assert!(result.tool_names.iter().any(|n| n.starts_with("git_")));
    }

    #[tokio::test]
    async fn combined_pressure_and_restriction() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                budget_pressure: 0.6,
                restricted_tools: vec!["github_ci_status".to_string()],
                ..make_ctx("check CI and show open PRs")
            })
            .await;
        assert!(!result.tool_names.contains(&"github_ci_status".to_string()));
        assert!(result.tool_names.contains(&"github_list_prs".to_string()));
    }

    #[tokio::test]
    async fn combined_hints_and_pressure() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector
            .select(&SelectionContext {
                budget_pressure: 0.6,
                memory_domain_hints: vec![DomainHint::GitHub],
                ..make_ctx("matrixorigin 的状态")
            })
            .await;
        let has_github = result.tool_names.iter().any(|n| n.starts_with("github_"));
        assert!(
            has_github,
            "domain hint should help under pressure: {:?}",
            result.tool_names
        );
    }

    #[tokio::test]
    async fn clear_query_higher_confidence_than_vague() {
        let selector = TfIdfSelector::new(test_registry());
        let clear = selector
            .select(&make_ctx("list open pull requests on github repository"))
            .await;
        let vague = selector.select(&make_ctx("help")).await;
        assert!(
            clear.confidence >= vague.confidence,
            "clear={} vague={}",
            clear.confidence,
            vague.confidence
        );
    }

    #[tokio::test]
    async fn conversational_selects_few_dynamic_tools() {
        let selector = TfIdfSelector::new(test_registry());
        let result = selector.select(&make_ctx("你好啊")).await;
        assert!(
            result.tool_names.len() <= 12,
            "conversational: {:?}",
            result.tool_names
        );
    }
}
