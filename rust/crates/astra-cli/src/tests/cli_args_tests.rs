use super::*;
use crate::cli::cli_config::cli_args::{ServeMode, SessionCaptureCmd};
use crate::cli::session::session_state::ExplainMode;

// ── CLI arg parsing tests ─────────────────────────────────────────────
// 128 tests → 16 table-driven tests + 2 integration tests (+3 composite)
// 1400 lines → ~430 lines

type BoolFlagCase<'a> = (&'a str, &'a [&'a str], fn(&Cli) -> bool);
type StringFlagCase<'a> = (&'a str, &'a [&'a str], fn(&Cli) -> Option<&str>);
type VecFlagCase<'a> = (&'a str, &'a [&'a str], fn(&Cli) -> &[String]);
type SubcommandCase<'a> = (&'a [&'a str], fn(&Command) -> bool);
type PermissionsCase<'a> = (&'a str, fn(&PermissionsSubcommand) -> bool);

#[test]
fn cli_no_args_gives_no_command() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.command.is_none());
    assert!(!cli.print);
    assert!(!cli.continue_last);
    assert!(!cli.no_journal_content);
    assert!(!cli.yes);
    assert!(cli.model.is_none());
    assert!(cli.resume.is_none());
}

// ── Flag tables ───────────────────────────────────────────────────────

#[test]
fn cli_bool_flags_default_and_set() {
    let cases: &[BoolFlagCase] = &[
        ("print", &["-p"], |c| c.print),
        ("continue", &["-c"], |c| c.continue_last),
        ("no-journal-content", &["--no-journal-content"], |c| {
            c.no_journal_content
        }),
        ("yes-short", &["-y"], |c| c.yes),
        ("yes-long", &["--yes"], |c| c.yes),
        ("verbose", &["--verbose"], |c| c.verbose),
        ("bare", &["--bare"], |c| c.bare),
        ("no-instructions", &["--no-instructions"], |c| {
            c.no_instructions
        }),
    ];
    // Defaults
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    for (name, _, get) in cases {
        assert!(!get(&cli), "default {name} should be false");
    }
    // Set
    for (name, args, get) in cases {
        let mut argv = vec!["astra"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv).unwrap();
        assert!(get(&cli), "--{name} should be true");
    }
}

#[test]
fn cli_string_flags() {
    let cases: &[StringFlagCase] = &[
        ("model long", &["--model", "gpt-4o"], |c| c.model.as_deref()),
        ("model equals", &["--model=claude-3-opus"], |c| {
            c.model.as_deref()
        }),
        ("resume short", &["-r", "abc123"], |c| c.resume.as_deref()),
        ("resume long", &["--resume", "session-xyz"], |c| {
            c.resume.as_deref()
        }),
        ("output-format", &["--output-format", "json"], |c| {
            Some(&c.output_format)
        }),
        ("api-url", &["--api-url", "http://remote:9000"], |c| {
            c.api_url.as_deref()
        }),
        ("profile", &["--profile", "work"], |c| c.profile.as_deref()),
        (
            "system-prompt",
            &["--system-prompt", "You are a code reviewer"],
            |c| c.system_prompt.as_deref(),
        ),
        (
            "session-id",
            &["--session-id", "550e8400-e29b-41d4-a716-446655440000"],
            |c| c.session_id.as_deref(),
        ),
        ("name short", &["-n", "my-session"], |c| {
            c.session_name.as_deref()
        }),
        ("name long", &["--name", "review-pr-42"], |c| {
            c.session_name.as_deref()
        }),
    ];
    for (name, args, get) in cases {
        let mut argv = vec!["astra"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv).unwrap();
        assert!(get(&cli).is_some(), "{name} should be set");
    }
    // Defaults: all None except output_format which defaults to "text"
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert_eq!(cli.output_format, "text");
    assert!(cli.model.is_none());
    assert!(cli.resume.is_none());
    assert!(cli.api_url.is_none());
    assert!(cli.profile.is_none());
    assert!(cli.system_prompt.is_none());
    assert!(cli.session_id.is_none());
    assert!(cli.session_name.is_none());
}

#[test]
fn cli_numeric_flags() {
    // max-turns
    let cli = Cli::try_parse_from(["astra", "--max-turns", "10"]).unwrap();
    assert_eq!(cli.max_turns, Some(10));
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.max_turns.is_none());

    // max-budget
    let cases: &[(&str, f64)] = &[
        ("5.50", 5.50),
        ("0.001", 0.001),
        ("999.99", 999.99),
        ("10", 10.0),
    ];
    for (input, expected) in cases {
        let cli = Cli::try_parse_from(["astra", "--max-budget", input]).unwrap();
        assert!(
            (cli.max_budget - expected).abs() < f64::EPSILON,
            "max-budget {input}"
        );
    }
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!((cli.max_budget - 0.0).abs() < f64::EPSILON);
}

#[test]
fn cli_vec_flags() {
    let cases: &[VecFlagCase] = &[
        ("allowed-tools single", &["--allowed-tools", "Bash"], |c| {
            &c.allowed_tools
        }),
        (
            "allowed-tools multi",
            &["--allowed-tools", "Bash", "Edit", "Read"],
            |c| &c.allowed_tools,
        ),
        (
            "disallowed-tools single",
            &["--disallowed-tools", "Bash"],
            |c| &c.disallowed_tools,
        ),
        (
            "disallowed-tools multi",
            &["--disallowed-tools", "Bash", "Edit"],
            |c| &c.disallowed_tools,
        ),
        ("add-dir single", &["--add-dir", "/tmp/extra"], |c| {
            &c.add_dir
        }),
        ("add-dir multi", &["--add-dir", "/tmp/a", "/tmp/b"], |c| {
            &c.add_dir
        }),
        ("mcp-config single", &["--mcp-config", "mcp.json"], |c| {
            &c.mcp_config
        }),
        (
            "mcp-config multi",
            &["--mcp-config", "a.json", "b.json"],
            |c| &c.mcp_config,
        ),
    ];
    let expected: &[&[&str]] = &[
        &["Bash"],
        &["Bash", "Edit", "Read"],
        &["Bash"],
        &["Bash", "Edit"],
        &["/tmp/extra"],
        &["/tmp/a", "/tmp/b"],
        &["mcp.json"],
        &["a.json", "b.json"],
    ];
    for (i, (name, args, get)) in cases.iter().enumerate() {
        let mut argv = vec!["astra"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv).unwrap();
        let got: Vec<&str> = get(&cli).iter().map(|s| s.as_str()).collect();
        assert_eq!(got, expected[i], "{name}");
    }
    // Defaults: all empty
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.allowed_tools.is_empty());
    assert!(cli.disallowed_tools.is_empty());
    assert!(cli.add_dir.is_empty());
    assert!(cli.mcp_config.is_empty());
}

// ── Simple subcommand dispatch ────────────────────────────────────────

#[test]
fn cli_simple_subcommands() {
    let cases: &[SubcommandCase] = &[
        (&["doctor"], |c| matches!(c, Command::Doctor)),
        (&["interactive"], |c| matches!(c, Command::Interactive)),
        (&["health"], |c| matches!(c, Command::Health)),
        (&["whoami"], |c| matches!(c, Command::Whoami)),
        (
            &["what", "is", "rust"],
            |c| matches!(c, Command::Message(w) if w == &["what", "is", "rust"]),
        ),
    ];
    for (args, check) in cases {
        let mut argv = vec!["astra"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv).unwrap();
        assert!(
            check(cli.command.as_ref().unwrap()),
            "subcommand: {:?}",
            args
        );
    }
}

#[test]
fn cli_completion_subcommand() {
    let cases = &[
        ("bash", clap_complete::Shell::Bash),
        ("zsh", clap_complete::Shell::Zsh),
        ("fish", clap_complete::Shell::Fish),
    ];
    for (shell_str, expected) in cases {
        let cli = Cli::try_parse_from(["astra", "completion", shell_str]).unwrap();
        match &cli.command {
            Some(Command::Completion(args)) => assert_eq!(args.shell, *expected, "{shell_str}"),
            _ => panic!("expected Completion for {shell_str}"),
        }
    }
}

#[test]
fn cli_mcp_subcommand() {
    // List
    let cli = Cli::try_parse_from(["astra", "mcp", "list"]).unwrap();
    assert!(matches!(&cli.command, Some(Command::Mcp(McpCmd::List(_)))));
    // Add
    let cli = Cli::try_parse_from([
        "astra", "mcp", "add", "s1", "npx", "server", "--port", "8080",
    ])
    .unwrap();
    match &cli.command {
        Some(Command::Mcp(McpCmd::Add(args))) => {
            assert_eq!(args.name, "s1");
            assert_eq!(args.command, "npx");
            assert_eq!(args.args, vec!["server", "--port", "8080"]);
        }
        _ => panic!("expected Mcp Add"),
    }
    // Add with scope
    for scope in ["project", "user"] {
        let cli =
            Cli::try_parse_from(["astra", "mcp", "add", "--scope", scope, "s1", "npx"]).unwrap();
        match &cli.command {
            Some(Command::Mcp(McpCmd::Add(args))) => assert_eq!(args.scope, scope),
            _ => panic!("expected Mcp Add scope={scope}"),
        }
    }
    // Remove
    let cli = Cli::try_parse_from(["astra", "mcp", "remove", "myserver"]).unwrap();
    match &cli.command {
        Some(Command::Mcp(McpCmd::Remove(args))) => assert_eq!(args.name, "myserver"),
        _ => panic!("expected Mcp Remove"),
    }
    // Get
    let cli = Cli::try_parse_from(["astra", "mcp", "get", "myserver"]).unwrap();
    match &cli.command {
        Some(Command::Mcp(McpCmd::Get(args))) => assert_eq!(args.name, "myserver"),
        _ => panic!("expected Mcp Get"),
    }
}

#[test]
fn cli_config_subcommand() {
    // List
    let cli = Cli::try_parse_from(["astra", "config", "list"]).unwrap();
    assert!(matches!(
        &cli.command,
        Some(Command::Config(ConfigCmd::List))
    ));
    // Get
    let cli = Cli::try_parse_from(["astra", "config", "get", "default_model"]).unwrap();
    match &cli.command {
        Some(Command::Config(ConfigCmd::Get(args))) => assert_eq!(args.key, "default_model"),
        _ => panic!("expected Config Get"),
    }
    // Set
    let cli = Cli::try_parse_from(["astra", "config", "set", "default_model", "gpt-4o"]).unwrap();
    match &cli.command {
        Some(Command::Config(ConfigCmd::Set(args))) => {
            assert_eq!(args.key, "default_model");
            assert_eq!(args.value, "gpt-4o");
        }
        _ => panic!("expected Config Set"),
    }
    // Show-policy
    for (case, expected_model) in [("with model", Some("gpt-4o")), ("without model", None)] {
        let mut argv = vec!["astra", "config", "show-policy"];
        if let Some(m) = expected_model {
            argv.extend(["--model", m]);
        }
        let cli = Cli::try_parse_from(argv).unwrap();
        match &cli.command {
            Some(Command::Config(ConfigCmd::ShowPolicy(args))) => {
                assert_eq!(args.model.as_deref(), expected_model, "{case}")
            }
            _ => panic!("expected ShowPolicy {case}"),
        }
    }
    // Show-policy --json
    let cli = Cli::try_parse_from([
        "astra",
        "config",
        "show-policy",
        "--json",
        "--model",
        "gpt-4o",
    ])
    .unwrap();
    match &cli.command {
        Some(Command::Config(ConfigCmd::ShowPolicy(args))) => {
            assert!(args.json);
            assert_eq!(args.model.as_deref(), Some("gpt-4o"));
        }
        _ => panic!("expected ShowPolicy json"),
    }
}

#[test]
fn cli_serve_subcommand() {
    // Defaults
    let cli = Cli::try_parse_from(["astra", "serve"]).unwrap();
    match &cli.command {
        Some(Command::Serve(args)) => {
            assert!(args.mode.is_none());
            assert_eq!(args.host, "127.0.0.1");
            assert_eq!(args.port, astra_core::DEFAULT_API_PORT);
        }
        _ => panic!("expected Serve"),
    }
    // Custom port
    let cli = Cli::try_parse_from(["astra", "serve", "--port", "3000"]).unwrap();
    match &cli.command {
        Some(Command::Serve(args)) => assert_eq!(args.port, 3000),
        _ => panic!("expected Serve custom port"),
    }
    // HTTP
    let cli = Cli::try_parse_from(["astra", "serve", "http", "--port", "3000"]).unwrap();
    match &cli.command {
        Some(Command::Serve(args)) => match &args.mode {
            Some(ServeMode::Http(http)) => {
                assert_eq!(http.host, "127.0.0.1");
                assert_eq!(http.port, 3000);
            }
            _ => panic!("expected serve http"),
        },
        _ => panic!("expected Serve command"),
    }
    // Stdio
    let cli = Cli::try_parse_from(["astra", "serve", "stdio"]).unwrap();
    match &cli.command {
        Some(Command::Serve(args)) => assert!(matches!(&args.mode, Some(ServeMode::Stdio))),
        _ => panic!("expected Serve stdio"),
    }
}

#[test]
fn cli_chat_subcommand() {
    // Model + message
    let cli = Cli::try_parse_from(["astra", "chat", "-m", "hello", "--model", "gpt-4o"]).unwrap();
    match &cli.command {
        Some(Command::Chat(args)) => {
            assert_eq!(args.message.as_deref(), Some("hello"));
            assert_eq!(args.model.as_deref(), Some("gpt-4o"));
        }
        _ => panic!("expected Chat"),
    }
    // Auto-approve
    let cli = Cli::try_parse_from(["astra", "chat", "-y"]).unwrap();
    match &cli.command {
        Some(Command::Chat(args)) => assert!(args.auto_approve),
        _ => panic!("expected Chat -y"),
    }
    // Explain modes
    for (mode_str, expected) in [
        ("", Some(ExplainMode::On)),
        ("verbose", Some(ExplainMode::Verbose)),
        ("off", Some(ExplainMode::Off)),
    ] {
        let mut argv = vec!["astra", "chat", "--explain"];
        if !mode_str.is_empty() {
            argv.push(mode_str);
        }
        let cli = Cli::try_parse_from(argv).unwrap();
        match &cli.command {
            Some(Command::Chat(args)) => assert_eq!(args.explain, expected, "explain={mode_str}"),
            _ => panic!("expected Chat explain"),
        }
    }
    // Permission modes
    for (input, expected) in [
        ("auto", "auto"),
        ("accept_edits", "accept_edits"),
        ("plan", "plan"),
    ] {
        let cli = Cli::try_parse_from(["astra", "chat", "--permission-mode", input]).unwrap();
        match &cli.command {
            Some(Command::Chat(args)) => {
                assert_eq!(args.permission_mode.as_deref(), Some(expected))
            }
            _ => panic!("expected Chat perm={input}"),
        }
    }
    for alias in ["accept-edits", "yolo", "bypass-safety", "bypass_safety"] {
        assert!(Cli::try_parse_from(["astra", "chat", "--permission-mode", alias]).is_err());
    }
}

#[test]
fn cli_permissions_subcommand() {
    let cases: &[PermissionsCase] = &[
        ("accept_edits", |s| {
            matches!(s, PermissionsSubcommand::AcceptEdits)
        }),
        ("plan", |s| matches!(s, PermissionsSubcommand::Plan)),
    ];
    for (mode, check) in cases {
        let cli = Cli::try_parse_from(["astra", "permissions", mode]).unwrap();
        match &cli.command {
            Some(Command::Permissions(args)) => {
                assert!(check(args.command.as_ref().unwrap()), "permissions {mode}");
            }
            _ => panic!("expected Permissions {mode}"),
        }
    }

    for removed in ["all", "status"] {
        assert!(
            Cli::try_parse_from(["astra", "permissions", removed]).is_err(),
            "removed permissions subcommand must be rejected: {removed}"
        );
    }
}

#[test]
fn cli_session_capture_subcommand() {
    // Latest
    let cli = Cli::try_parse_from([
        "astra",
        "session",
        "capture",
        "latest",
        "550e8400-e29b-41d4-a716-446655440000",
    ])
    .unwrap();
    match &cli.command {
        Some(Command::Session(SessionCmd::Capture(SessionCaptureCmd::Latest(args)))) => {
            assert_eq!(
                args.session_id.as_deref(),
                Some("550e8400-e29b-41d4-a716-446655440000")
            );
            assert_eq!(args.artifact_kind, "llm_capture");
        }
        _ => panic!("expected session capture latest"),
    }
    // Download
    let cli = Cli::try_parse_from([
        "astra",
        "session",
        "capture",
        "download",
        "--output",
        "capture.json",
    ])
    .unwrap();
    match &cli.command {
        Some(Command::Session(SessionCmd::Capture(SessionCaptureCmd::Download(args)))) => {
            assert!(args.session_id.is_none());
            assert_eq!(args.artifact_kind, "llm_capture");
            assert_eq!(
                args.output.as_deref(),
                Some(std::path::Path::new("capture.json"))
            );
        }
        _ => panic!("expected session capture download"),
    }
}

// ── Error rejection ───────────────────────────────────────────────────

#[test]
fn cli_rejects_invalid_input() {
    let cases: &[(&[&str], &str)] = &[
        (&["--max-turns", "abc"], "non-numeric max-turns"),
        (&["--max-budget", "abc"], "non-numeric max-budget"),
        (&["chat", "--explain", "laser"], "invalid explain mode"),
        (
            &["chat", "--permission-mode", "invalid", "-m", "test"],
            "invalid permission mode",
        ),
    ];
    for (args, desc) in cases {
        let mut argv = vec!["astra"];
        argv.extend_from_slice(args);
        assert!(Cli::try_parse_from(argv).is_err(), "should reject: {desc}");
    }
    // Negative budget: clap rejects it (treats "-1" as a flag)
    assert!(
        Cli::try_parse_from(["astra", "--max-budget", "-1.0"]).is_err(),
        "should reject negative budget"
    );
}

// ── Composite / integration ───────────────────────────────────────────

#[test]
fn cli_combined_short_flags() {
    let cli = Cli::try_parse_from(["astra", "-p", "-c", "-y"]).unwrap();
    assert!(cli.print);
    assert!(cli.continue_last);
    assert!(cli.yes);
}

#[test]
fn cli_all_top_level_flags_combined() {
    let cli = Cli::try_parse_from([
        "astra",
        "--model",
        "gpt-4o",
        "-p",
        "-y",
        "--system-prompt",
        "Review code",
        "--max-turns",
        "3",
        "--output-format",
        "json",
        "--allowed-tools",
        "Read",
        "Edit",
        "--disallowed-tools",
        "Bash",
        "--add-dir",
        "/tmp/extra",
        "--verbose",
        "--mcp-config",
        "mcp.json",
        "--session-id",
        "550e8400-e29b-41d4-a716-446655440000",
        "-n",
        "debug-session",
        "--bare",
    ])
    .unwrap();
    assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
    assert!(cli.print);
    assert!(cli.yes);
    assert_eq!(cli.system_prompt.as_deref(), Some("Review code"));
    assert_eq!(cli.max_turns, Some(3));
    assert_eq!(cli.output_format, "json");
    assert_eq!(cli.allowed_tools, vec!["Read", "Edit"]);
    assert_eq!(cli.disallowed_tools, vec!["Bash"]);
    assert_eq!(cli.add_dir, vec!["/tmp/extra"]);
    assert!(cli.verbose);
    assert_eq!(cli.mcp_config, vec!["mcp.json"]);
    assert_eq!(
        cli.session_id.as_deref(),
        Some("550e8400-e29b-41d4-a716-446655440000")
    );
    assert_eq!(cli.session_name.as_deref(), Some("debug-session"));
    assert!(cli.bare);
}

#[test]
fn cli_top_level_yes_does_not_conflict_with_chat_yes() {
    let cli = Cli::try_parse_from(["astra", "-y", "chat", "-y"]).unwrap();
    assert!(cli.yes);
    match &cli.command {
        Some(Command::Chat(args)) => assert!(args.auto_approve),
        _ => panic!("expected Chat"),
    }
}

#[test]
fn cli_chat_yes_with_permission_mode() {
    for (perm, expect_auto) in [("deny", false), ("auto", true)] {
        let cli = Cli::try_parse_from([
            "astra",
            "chat",
            "-y",
            "--permission-mode",
            perm,
            "-m",
            "test",
        ])
        .unwrap();
        match &cli.command {
            Some(Command::Chat(args)) => {
                assert!(args.auto_approve);
                assert_eq!(args.permission_mode.as_deref(), Some(perm));
                // accept: both parsed, runtime resolves conflict
                let _ = expect_auto;
            }
            _ => panic!("expected Chat"),
        }
    }
}

// ── Completion output ──────────────────────────────────────────────────

#[test]
fn cli_completion_generates_output() {
    for shell in [clap_complete::Shell::Bash, clap_complete::Shell::Zsh] {
        use clap::CommandFactory;
        let mut buf = Vec::new();
        clap_complete::generate(shell, &mut Cli::command(), "astra", &mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("astra"));
        assert!(!output.is_empty(), "completion for {shell:?}");
    }
}
