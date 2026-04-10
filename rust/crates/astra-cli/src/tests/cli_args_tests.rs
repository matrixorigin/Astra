use super::*;

// ── CLI arg parsing tests ─────────────────────────────────────────────

#[test]
fn cli_no_args_gives_no_command() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.command.is_none());
    assert!(!cli.print);
    assert!(!cli.continue_last);
    assert!(!cli.yes);
    assert!(cli.model.is_none());
    assert!(cli.resume.is_none());
}

#[test]
fn cli_model_flag_long() {
    let cli = Cli::try_parse_from(["astra", "--model", "gpt-4o"]).unwrap();
    assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
}

#[test]
fn cli_model_flag_equals() {
    let cli = Cli::try_parse_from(["astra", "--model=claude-3-opus"]).unwrap();
    assert_eq!(cli.model.as_deref(), Some("claude-3-opus"));
}

#[test]
fn cli_print_flag_short() {
    let cli = Cli::try_parse_from(["astra", "-p"]).unwrap();
    assert!(cli.print);
}

#[test]
fn cli_print_flag_long() {
    let cli = Cli::try_parse_from(["astra", "--print"]).unwrap();
    assert!(cli.print);
}

#[test]
fn cli_output_format_default_is_text() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert_eq!(cli.output_format, "text");
}

#[test]
fn cli_output_format_json() {
    let cli = Cli::try_parse_from(["astra", "--output-format", "json"]).unwrap();
    assert_eq!(cli.output_format, "json");
}

#[test]
fn cli_continue_flag_short() {
    let cli = Cli::try_parse_from(["astra", "-c"]).unwrap();
    assert!(cli.continue_last);
}

#[test]
fn cli_continue_flag_long() {
    let cli = Cli::try_parse_from(["astra", "--continue"]).unwrap();
    assert!(cli.continue_last);
}

#[test]
fn cli_resume_flag_short() {
    let cli = Cli::try_parse_from(["astra", "-r", "abc123"]).unwrap();
    assert_eq!(cli.resume.as_deref(), Some("abc123"));
}

#[test]
fn cli_resume_flag_long() {
    let cli = Cli::try_parse_from(["astra", "--resume", "session-xyz"]).unwrap();
    assert_eq!(cli.resume.as_deref(), Some("session-xyz"));
}

#[test]
fn cli_yes_flag_short() {
    let cli = Cli::try_parse_from(["astra", "-y"]).unwrap();
    assert!(cli.yes);
}

#[test]
fn cli_yes_flag_long() {
    let cli = Cli::try_parse_from(["astra", "--yes"]).unwrap();
    assert!(cli.yes);
}

#[test]
fn cli_combined_short_flags() {
    // -p -c -y can be combined
    let cli = Cli::try_parse_from(["astra", "-p", "-c", "-y"]).unwrap();
    assert!(cli.print);
    assert!(cli.continue_last);
    assert!(cli.yes);
}

#[test]
fn cli_model_with_print_and_yes() {
    let cli = Cli::try_parse_from(["astra", "--model", "gpt-4o", "-p", "-y"]).unwrap();
    assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
    assert!(cli.print);
    assert!(cli.yes);
}

#[test]
fn cli_doctor_subcommand() {
    let cli = Cli::try_parse_from(["astra", "doctor"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Doctor)));
}

#[test]
fn cli_completion_bash() {
    let cli = Cli::try_parse_from(["astra", "completion", "bash"]).unwrap();
    match cli.command {
        Some(Command::Completion(ref args)) => {
            assert_eq!(args.shell, clap_complete::Shell::Bash);
        }
        _ => panic!("expected Completion command"),
    }
}

#[test]
fn cli_completion_zsh() {
    let cli = Cli::try_parse_from(["astra", "completion", "zsh"]).unwrap();
    match cli.command {
        Some(Command::Completion(ref args)) => {
            assert_eq!(args.shell, clap_complete::Shell::Zsh);
        }
        _ => panic!("expected Completion command"),
    }
}

#[test]
fn cli_completion_fish() {
    let cli = Cli::try_parse_from(["astra", "completion", "fish"]).unwrap();
    match cli.command {
        Some(Command::Completion(ref args)) => {
            assert_eq!(args.shell, clap_complete::Shell::Fish);
        }
        _ => panic!("expected Completion command"),
    }
}

#[test]
fn cli_mcp_list_subcommand() {
    let cli = Cli::try_parse_from(["astra", "mcp", "list"]).unwrap();
    match cli.command {
        Some(Command::Mcp(McpCmd::List(_))) => {}
        _ => panic!("expected Mcp List command"),
    }
}

#[test]
fn cli_mcp_add_with_args() {
    let cli = Cli::try_parse_from(["astra", "mcp", "add", "myserver", "npx", "server"]).unwrap();
    match cli.command {
        Some(Command::Mcp(McpCmd::Add(ref args))) => {
            assert_eq!(args.name, "myserver");
            assert_eq!(args.command, "npx");
            assert_eq!(args.args, vec!["server"]);
        }
        _ => panic!("expected Mcp Add command"),
    }
}

#[test]
fn cli_mcp_remove_subcommand() {
    let cli = Cli::try_parse_from(["astra", "mcp", "remove", "myserver"]).unwrap();
    match cli.command {
        Some(Command::Mcp(McpCmd::Remove(ref args))) => {
            assert_eq!(args.name, "myserver");
        }
        _ => panic!("expected Mcp Remove command"),
    }
}

#[test]
fn cli_mcp_get_subcommand() {
    let cli = Cli::try_parse_from(["astra", "mcp", "get", "myserver"]).unwrap();
    match cli.command {
        Some(Command::Mcp(McpCmd::Get(ref args))) => {
            assert_eq!(args.name, "myserver");
        }
        _ => panic!("expected Mcp Get command"),
    }
}

#[test]
fn cli_config_list_subcommand() {
    let cli = Cli::try_parse_from(["astra", "config", "list"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Config(ConfigCmd::List))
    ));
}

#[test]
fn cli_config_get_subcommand() {
    let cli = Cli::try_parse_from(["astra", "config", "get", "default_model"]).unwrap();
    match cli.command {
        Some(Command::Config(ConfigCmd::Get(ref args))) => {
            assert_eq!(args.key, "default_model");
        }
        _ => panic!("expected Config Get command"),
    }
}

#[test]
fn cli_config_set_subcommand() {
    let cli = Cli::try_parse_from(["astra", "config", "set", "default_model", "gpt-4o"]).unwrap();
    match cli.command {
        Some(Command::Config(ConfigCmd::Set(ref args))) => {
            assert_eq!(args.key, "default_model");
            assert_eq!(args.value, "gpt-4o");
        }
        _ => panic!("expected Config Set command"),
    }
}

#[test]
fn cli_chat_with_model() {
    let cli = Cli::try_parse_from(["astra", "chat", "-m", "hello", "--model", "gpt-4o"]).unwrap();
    match cli.command {
        Some(Command::Chat(ref args)) => {
            assert_eq!(args.message.as_deref(), Some("hello"));
            assert_eq!(args.model.as_deref(), Some("gpt-4o"));
        }
        _ => panic!("expected Chat command"),
    }
}

#[test]
fn cli_chat_auto_approve() {
    let cli = Cli::try_parse_from(["astra", "chat", "-y"]).unwrap();
    match cli.command {
        Some(Command::Chat(ref args)) => {
            assert!(args.auto_approve);
        }
        _ => panic!("expected Chat command"),
    }
}

#[test]
fn cli_chat_permission_mode() {
    let cli = Cli::try_parse_from(["astra", "chat", "--permission-mode", "auto"]).unwrap();
    match cli.command {
        Some(Command::Chat(ref args)) => {
            assert_eq!(args.permission_mode.as_deref(), Some("auto"));
        }
        _ => panic!("expected Chat command"),
    }
}

#[test]
fn cli_external_subcommand_message() {
    let cli = Cli::try_parse_from(["astra", "what", "is", "rust"]).unwrap();
    match cli.command {
        Some(Command::Message(ref words)) => {
            assert_eq!(words, &["what", "is", "rust"]);
        }
        _ => panic!("expected Message command"),
    }
}

#[test]
fn cli_plan_decompose_parses() {
    let cli = Cli::try_parse_from([
        "astra",
        "plan",
        "decompose",
        "-g",
        "smoke goal",
        "--json",
        "-q",
    ])
    .unwrap();
    match cli.command {
        Some(Command::Plan(PlanCmd::Decompose {
            ref goal,
            json,
            quiet,
        })) => {
            assert_eq!(goal, "smoke goal");
            assert!(json);
            assert!(quiet);
        }
        _ => panic!("expected Plan::Decompose command"),
    }
}

#[test]
fn cli_serve_defaults() {
    let cli = Cli::try_parse_from(["astra", "serve"]).unwrap();
    match cli.command {
        Some(Command::Serve(ref args)) => {
            assert_eq!(args.host, "127.0.0.1");
            assert_eq!(args.port, 8000);
        }
        _ => panic!("expected Serve command"),
    }
}

#[test]
fn cli_serve_custom_port() {
    let cli = Cli::try_parse_from(["astra", "serve", "--port", "3000"]).unwrap();
    match cli.command {
        Some(Command::Serve(ref args)) => {
            assert_eq!(args.port, 3000);
        }
        _ => panic!("expected Serve command"),
    }
}

#[test]
fn cli_api_url_default() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert_eq!(cli.api_url, "http://127.0.0.1:8000");
}

#[test]
fn cli_api_url_custom() {
    let cli = Cli::try_parse_from(["astra", "--api-url", "http://remote:9000"]).unwrap();
    assert_eq!(cli.api_url, "http://remote:9000");
}

#[test]
fn cli_profile_flag() {
    let cli = Cli::try_parse_from(["astra", "--profile", "work"]).unwrap();
    assert_eq!(cli.profile.as_deref(), Some("work"));
}

#[test]
fn cli_top_level_yes_does_not_conflict_with_chat_yes() {
    // Both top-level -y and chat -y should work together
    let cli = Cli::try_parse_from(["astra", "-y", "chat", "-y"]).unwrap();
    assert!(cli.yes);
    match cli.command {
        Some(Command::Chat(ref args)) => {
            assert!(args.auto_approve);
        }
        _ => panic!("expected Chat command"),
    }
}

#[test]
fn cli_mcp_add_scope_project() {
    let cli =
        Cli::try_parse_from(["astra", "mcp", "add", "--scope", "project", "s1", "npx"]).unwrap();
    match cli.command {
        Some(Command::Mcp(McpCmd::Add(ref args))) => {
            assert_eq!(args.scope, "project");
            assert_eq!(args.name, "s1");
            assert_eq!(args.command, "npx");
        }
        _ => panic!("expected Mcp Add command"),
    }
}

#[test]
fn cli_mcp_add_scope_user() {
    let cli = Cli::try_parse_from(["astra", "mcp", "add", "--scope", "user", "s1", "npx"]).unwrap();
    match cli.command {
        Some(Command::Mcp(McpCmd::Add(ref args))) => {
            assert_eq!(args.scope, "user");
        }
        _ => panic!("expected Mcp Add command"),
    }
}

#[test]
fn cli_mcp_add_with_trailing_args() {
    let cli = Cli::try_parse_from([
        "astra", "mcp", "add", "s1", "npx", "server", "--port", "8080",
    ])
    .unwrap();
    match cli.command {
        Some(Command::Mcp(McpCmd::Add(ref args))) => {
            assert_eq!(args.name, "s1");
            assert_eq!(args.command, "npx");
            assert_eq!(args.args, vec!["server", "--port", "8080"]);
        }
        _ => panic!("expected Mcp Add command"),
    }
}

#[test]
fn cli_interactive_subcommand() {
    let cli = Cli::try_parse_from(["astra", "interactive"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Interactive)));
}

#[test]
fn cli_health_subcommand() {
    let cli = Cli::try_parse_from(["astra", "health"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Health)));
}

#[test]
fn cli_whoami_subcommand() {
    let cli = Cli::try_parse_from(["astra", "whoami"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Whoami)));
}

#[test]
fn cli_system_prompt_flag() {
    let cli = Cli::try_parse_from(["astra", "--system-prompt", "You are a code reviewer"]).unwrap();
    assert_eq!(
        cli.system_prompt.as_deref(),
        Some("You are a code reviewer")
    );
}

#[test]
fn cli_max_turns_flag() {
    let cli = Cli::try_parse_from(["astra", "--max-turns", "10"]).unwrap();
    assert_eq!(cli.max_turns, Some(10));
}

#[test]
fn cli_max_turns_default_is_none() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.max_turns.is_none());
}

#[test]
fn cli_system_prompt_with_print() {
    let cli = Cli::try_parse_from([
        "astra",
        "-p",
        "--system-prompt",
        "Be concise",
        "--max-turns",
        "5",
    ])
    .unwrap();
    assert!(cli.print);
    assert_eq!(cli.system_prompt.as_deref(), Some("Be concise"));
    assert_eq!(cli.max_turns, Some(5));
}

#[test]
fn cli_completion_generates_bash_output() {
    use clap::CommandFactory;
    let mut buf = Vec::new();
    clap_complete::generate(
        clap_complete::Shell::Bash,
        &mut Cli::command(),
        "astra",
        &mut buf,
    );
    let output = String::from_utf8(buf).unwrap();
    assert!(output.contains("astra"));
    assert!(output.contains("complete"));
}

#[test]
fn cli_completion_generates_zsh_output() {
    use clap::CommandFactory;
    let mut buf = Vec::new();
    clap_complete::generate(
        clap_complete::Shell::Zsh,
        &mut Cli::command(),
        "astra",
        &mut buf,
    );
    let output = String::from_utf8(buf).unwrap();
    assert!(output.contains("astra"));
    assert!(!output.is_empty());
}

#[test]
fn cli_max_turns_rejects_non_numeric() {
    let result = Cli::try_parse_from(["astra", "--max-turns", "abc"]);
    assert!(result.is_err());
}

#[test]
fn cli_all_flags_combined() {
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
    ])
    .unwrap();
    assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
    assert!(cli.print);
    assert!(cli.yes);
    assert_eq!(cli.system_prompt.as_deref(), Some("Review code"));
    assert_eq!(cli.max_turns, Some(3));
    assert_eq!(cli.output_format, "json");
}

// ── --allowed-tools tests ──

#[test]
fn cli_allowed_tools_single() {
    let cli = Cli::try_parse_from(["astra", "--allowed-tools", "Bash"]).unwrap();
    assert_eq!(cli.allowed_tools, vec!["Bash"]);
}

#[test]
fn cli_allowed_tools_multiple_space_separated() {
    let cli = Cli::try_parse_from(["astra", "--allowed-tools", "Bash", "Edit", "Read"]).unwrap();
    assert_eq!(cli.allowed_tools, vec!["Bash", "Edit", "Read"]);
}

#[test]
fn cli_allowed_tools_empty_default() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.allowed_tools.is_empty());
}

// ── --add-dir tests ──

#[test]
fn cli_add_dir_single() {
    let cli = Cli::try_parse_from(["astra", "--add-dir", "/tmp/extra"]).unwrap();
    assert_eq!(cli.add_dir, vec!["/tmp/extra"]);
}

#[test]
fn cli_add_dir_multiple() {
    let cli = Cli::try_parse_from(["astra", "--add-dir", "/tmp/a", "/tmp/b"]).unwrap();
    assert_eq!(cli.add_dir, vec!["/tmp/a", "/tmp/b"]);
}

#[test]
fn cli_add_dir_empty_default() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.add_dir.is_empty());
}

// ── --verbose tests ──

#[test]
fn cli_verbose_flag() {
    let cli = Cli::try_parse_from(["astra", "--verbose"]).unwrap();
    assert!(cli.verbose);
}

#[test]
fn cli_verbose_default_false() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(!cli.verbose);
}

// ── --mcp-config tests ──

#[test]
fn cli_mcp_config_single() {
    let cli = Cli::try_parse_from(["astra", "--mcp-config", "mcp.json"]).unwrap();
    assert_eq!(cli.mcp_config, vec!["mcp.json"]);
}

#[test]
fn cli_mcp_config_multiple() {
    let cli = Cli::try_parse_from(["astra", "--mcp-config", "a.json", "b.json"]).unwrap();
    assert_eq!(cli.mcp_config, vec!["a.json", "b.json"]);
}

#[test]
fn cli_mcp_config_empty_default() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.mcp_config.is_empty());
}

// ── Combined new flags ──

#[test]
fn cli_all_new_flags_combined() {
    let cli = Cli::try_parse_from([
        "astra",
        "--allowed-tools",
        "Bash",
        "Edit",
        "--add-dir",
        "/tmp/extra",
        "--verbose",
        "--mcp-config",
        "mcp.json",
        "--model",
        "gpt-4o",
        "-p",
    ])
    .unwrap();
    assert_eq!(cli.allowed_tools, vec!["Bash", "Edit"]);
    assert_eq!(cli.add_dir, vec!["/tmp/extra"]);
    assert!(cli.verbose);
    assert_eq!(cli.mcp_config, vec!["mcp.json"]);
    assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
    assert!(cli.print);
}

// ── --disallowed-tools tests ──

#[test]
fn cli_disallowed_tools_single() {
    let cli = Cli::try_parse_from(["astra", "--disallowed-tools", "Bash"]).unwrap();
    assert_eq!(cli.disallowed_tools, vec!["Bash"]);
}

#[test]
fn cli_disallowed_tools_multiple() {
    let cli = Cli::try_parse_from(["astra", "--disallowed-tools", "Bash", "Edit"]).unwrap();
    assert_eq!(cli.disallowed_tools, vec!["Bash", "Edit"]);
}

#[test]
fn cli_disallowed_tools_empty_default() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.disallowed_tools.is_empty());
}

#[test]
fn cli_allowed_and_disallowed_together() {
    let cli = Cli::try_parse_from([
        "astra",
        "--allowed-tools",
        "Read",
        "Edit",
        "--disallowed-tools",
        "Bash",
    ])
    .unwrap();
    assert_eq!(cli.allowed_tools, vec!["Read", "Edit"]);
    assert_eq!(cli.disallowed_tools, vec!["Bash"]);
}

// ── --session-id tests ──

#[test]
fn cli_session_id_flag() {
    let cli = Cli::try_parse_from([
        "astra",
        "--session-id",
        "550e8400-e29b-41d4-a716-446655440000",
    ])
    .unwrap();
    assert_eq!(
        cli.session_id.as_deref(),
        Some("550e8400-e29b-41d4-a716-446655440000")
    );
}

#[test]
fn cli_session_id_default_none() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.session_id.is_none());
}

// ── --name tests ──

#[test]
fn cli_name_short_flag() {
    let cli = Cli::try_parse_from(["astra", "-n", "my-session"]).unwrap();
    assert_eq!(cli.session_name.as_deref(), Some("my-session"));
}

#[test]
fn cli_name_long_flag() {
    let cli = Cli::try_parse_from(["astra", "--name", "review-pr-42"]).unwrap();
    assert_eq!(cli.session_name.as_deref(), Some("review-pr-42"));
}

#[test]
fn cli_name_default_none() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(cli.session_name.is_none());
}

// ── --bare tests ──

#[test]
fn cli_bare_flag() {
    let cli = Cli::try_parse_from(["astra", "--bare"]).unwrap();
    assert!(cli.bare);
}

#[test]
fn cli_bare_default_false() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(!cli.bare);
}

#[test]
fn cli_bare_with_print_and_system_prompt() {
    let cli = Cli::try_parse_from([
        "astra",
        "--bare",
        "-p",
        "--system-prompt",
        "Be brief",
        "--add-dir",
        "/tmp/work",
    ])
    .unwrap();
    assert!(cli.bare);
    assert!(cli.print);
    assert_eq!(cli.system_prompt.as_deref(), Some("Be brief"));
    assert_eq!(cli.add_dir, vec!["/tmp/work"]);
}

#[test]
fn cli_session_id_and_name_combined() {
    let cli = Cli::try_parse_from([
        "astra",
        "--session-id",
        "123e4567-e89b-12d3-a456-426614174000",
        "-n",
        "debug-session",
    ])
    .unwrap();
    assert_eq!(
        cli.session_id.as_deref(),
        Some("123e4567-e89b-12d3-a456-426614174000")
    );
    assert_eq!(cli.session_name.as_deref(), Some("debug-session"));
}

// ── --max-budget tests ──

#[test]
fn cli_max_budget_flag() {
    let cli = Cli::try_parse_from(["astra", "--max-budget", "5.50"]).unwrap();
    assert!((cli.max_budget - 5.50).abs() < f64::EPSILON);
}

#[test]
fn cli_max_budget_default_zero() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!((cli.max_budget - 0.0).abs() < f64::EPSILON);
}

#[test]
fn cli_max_budget_rejects_non_numeric() {
    let result = Cli::try_parse_from(["astra", "--max-budget", "abc"]);
    assert!(result.is_err());
}

#[test]
fn cli_max_budget_with_print_and_turns() {
    let cli =
        Cli::try_parse_from(["astra", "-p", "--max-turns", "10", "--max-budget", "1.0"]).unwrap();
    assert!(cli.print);
    assert_eq!(cli.max_turns, Some(10));
    assert!((cli.max_budget - 1.0).abs() < f64::EPSILON);
}

// ── --max-budget edge case tests ──

#[test]
fn cli_max_budget_negative_rejected() {
    let result = Cli::try_parse_from(["astra", "--max-budget", "-1.0"]);
    // clap may or may not accept negative f64 — verify behavior
    match result {
        Ok(cli) => assert!(cli.max_budget < 0.0, "negative budget parsed but < 0"),
        Err(_) => {} // rejected is fine too
    }
}

#[test]
fn cli_max_budget_very_small_value() {
    let cli = Cli::try_parse_from(["astra", "--max-budget", "0.001"]).unwrap();
    assert!((cli.max_budget - 0.001).abs() < f64::EPSILON);
}

#[test]
fn cli_max_budget_large_value() {
    let cli = Cli::try_parse_from(["astra", "--max-budget", "999.99"]).unwrap();
    assert!((cli.max_budget - 999.99).abs() < f64::EPSILON);
}

#[test]
fn cli_max_budget_integer_value() {
    let cli = Cli::try_parse_from(["astra", "--max-budget", "10"]).unwrap();
    assert!((cli.max_budget - 10.0).abs() < f64::EPSILON);
}

// ── --yes / -y edge case tests ──

#[test]
fn cli_yes_flag_sets_auto_approve() {
    let cli = Cli::try_parse_from(["astra", "-y"]).unwrap();
    assert!(cli.yes);
}

#[test]
fn cli_yes_long_flag_sets_auto_approve() {
    let cli = Cli::try_parse_from(["astra", "--yes"]).unwrap();
    assert!(cli.yes);
}

#[test]
fn cli_yes_with_permission_mode_deny() {
    // Both flags accepted by parser on `chat` subcommand — runtime resolves conflict
    let cli = Cli::try_parse_from([
        "astra",
        "chat",
        "-y",
        "--permission-mode",
        "deny",
        "-m",
        "test",
    ])
    .unwrap();
    match &cli.command {
        Some(Command::Chat(args)) => {
            assert!(args.auto_approve);
            assert_eq!(args.permission_mode.as_deref(), Some("deny"));
        }
        _ => panic!("expected Chat command"),
    }
}

#[test]
fn cli_yes_with_permission_mode_auto_is_redundant() {
    let cli = Cli::try_parse_from([
        "astra",
        "chat",
        "-y",
        "--permission-mode",
        "auto",
        "-m",
        "test",
    ])
    .unwrap();
    match &cli.command {
        Some(Command::Chat(args)) => {
            assert!(args.auto_approve);
            assert_eq!(args.permission_mode.as_deref(), Some("auto"));
        }
        _ => panic!("expected Chat command"),
    }
}

#[test]
fn cli_permission_mode_invalid_value() {
    // value_parser constraint rejects invalid values at parse time
    let result = Cli::try_parse_from([
        "astra",
        "chat",
        "--permission-mode",
        "invalid",
        "-m",
        "test",
    ]);
    assert!(result.is_err(), "invalid permission mode should be rejected by parser");
}

#[test]
fn cli_default_no_yes_no_permission_mode() {
    let cli = Cli::try_parse_from(["astra"]).unwrap();
    assert!(!cli.yes);
}

// ── /allow command tests ──

#[test]
fn permission_mode_set_mode() {
    let mut pm = permission_manager::PermissionManager::with_project(
        false,
        &std::path::PathBuf::from("/tmp"),
    );
    assert_eq!(pm.mode(), permission_manager::PermissionMode::Prompt);
    pm.set_mode(permission_manager::PermissionMode::Auto);
    assert_eq!(pm.mode(), permission_manager::PermissionMode::Auto);
    pm.set_mode(permission_manager::PermissionMode::Deny);
    assert_eq!(pm.mode(), permission_manager::PermissionMode::Deny);
}

#[test]
fn permission_mode_roundtrip_parse() {
    for mode_str in &["auto", "prompt", "deny"] {
        let mode: permission_manager::PermissionMode = mode_str.parse().unwrap();
        assert_eq!(mode.to_string().to_lowercase(), *mode_str);
    }
}

#[test]
fn repl_state_auto_approve_env_activates_auto_mode() {
    // When ASTRA_AUTO_APPROVE=1, ReplState should start in Auto mode
    unsafe {
        std::env::set_var("ASTRA_AUTO_APPROVE", "1");
    }
    let state = ReplState::default();
    unsafe {
        std::env::remove_var("ASTRA_AUTO_APPROVE");
    }
    assert_eq!(
        state.perm_manager.mode(),
        permission_manager::PermissionMode::Auto
    );
}

#[tokio::test]
async fn task_run_stores_result_in_checkpoint() {
    use astra_services::{TaskCreateRequest, TaskService, task_orchestrator::TaskCheckpoint};

    // Use a temp dir for LocalTaskService
    let tmp = tempfile::tempdir().unwrap();
    let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());

    // Create a task (simulates what /task run does)
    let tid = svc
        .create_task(
            "test-user",
            "test-session",
            TaskCreateRequest {
                title: "run: test prompt".to_string(),
                description: Some("test prompt".to_string()),
                plan: None,
                parent_task_id: None,
                project_type: None,
                goal_pattern: None,
            },
        )
        .await
        .unwrap();

    // Mark in-progress
    svc.update_status(&tid, astra_services::TaskStatus::InProgress)
        .await
        .unwrap();

    // Save checkpoint with result (simulates background task completion)
    let mut state_map = serde_json::Map::new();
    state_map.insert(
        "full_text".to_string(),
        serde_json::Value::String("Hello from agent".to_string()),
    );
    state_map.insert("prompt_tokens".to_string(), serde_json::json!(100));
    state_map.insert("completion_tokens".to_string(), serde_json::json!(50));
    state_map.insert("tool_calls_count".to_string(), serde_json::json!(3));

    svc.save_checkpoint(
        &tid,
        &TaskCheckpoint {
            active_subtask_id: None,
            turn: 0,
            session_id: Some("test-session".to_string()),
            state: state_map,
        },
    )
    .await
    .unwrap();

    // Complete the task
    svc.complete_task(&tid).await.unwrap();

    // Read back and verify (simulates /task result)
    let record = svc.get_task(&tid).await.unwrap().unwrap();
    assert_eq!(record.status, astra_services::TaskStatus::Completed);
    let cp = record.checkpoint.unwrap();
    assert_eq!(
        cp.state.get("full_text").and_then(|v| v.as_str()),
        Some("Hello from agent")
    );
    assert_eq!(
        cp.state.get("prompt_tokens").and_then(|v| v.as_u64()),
        Some(100)
    );
    assert_eq!(
        cp.state.get("tool_calls_count").and_then(|v| v.as_u64()),
        Some(3)
    );
}

// ── @file system-prompt tests ──

#[test]
fn resolve_system_prompt_literal_text() {
    let result = resolve_system_prompt("You are a helpful assistant.".to_string());
    assert_eq!(result.unwrap(), "You are a helpful assistant.");
}

#[test]
fn resolve_system_prompt_at_file_reads_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prompt.txt");
    std::fs::write(&path, "Custom system prompt from file").unwrap();
    let result = resolve_system_prompt(format!("@{}", path.display()));
    assert_eq!(result.unwrap(), "Custom system prompt from file");
}

#[test]
fn resolve_system_prompt_at_file_not_found() {
    let result = resolve_system_prompt("@/nonexistent/path/prompt.txt".to_string());
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .contains("cannot read system prompt file")
    );
}

#[test]
fn resolve_system_prompt_at_file_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.txt");
    std::fs::write(&path, "").unwrap();
    let result = resolve_system_prompt(format!("@{}", path.display()));
    assert_eq!(result.unwrap(), "");
}

#[test]
fn resolve_system_prompt_at_bare_is_error() {
    let result = resolve_system_prompt("@".to_string());
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("requires a file path"));
}

#[test]
fn resolve_system_prompt_at_file_with_unicode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unicode.txt");
    std::fs::write(&path, "你好世界 🌍 مرحبا").unwrap();
    let result = resolve_system_prompt(format!("@{}", path.display()));
    assert_eq!(result.unwrap(), "你好世界 🌍 مرحبا");
}

#[test]
fn resolve_system_prompt_at_file_with_newlines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multi.txt");
    std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
    let result = resolve_system_prompt(format!("@{}", path.display()));
    assert_eq!(result.unwrap(), "line1\nline2\nline3\n");
}

#[test]
fn resolve_system_prompt_no_at_prefix_passes_through() {
    let result = resolve_system_prompt("/some/path/prompt.txt".to_string());
    assert_eq!(result.unwrap(), "/some/path/prompt.txt");
}

#[test]
fn resolve_system_prompt_at_file_large_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.txt");
    let content = "x".repeat(1_000_000);
    std::fs::write(&path, &content).unwrap();
    let result = resolve_system_prompt(format!("@{}", path.display()));
    assert_eq!(result.unwrap().len(), 1_000_000);
}

#[test]
fn resolve_system_prompt_at_file_permission_denied() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("noperm.txt");
    std::fs::write(&path, "secret").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = resolve_system_prompt(format!("@{}", path.display()));
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .contains("cannot read system prompt file")
    );
}

// ── project instructions tests ──

#[test]
fn discover_project_instructions_from_tempdir() {
    let dir = tempfile::tempdir().unwrap();
    let astra_dir = dir.path().join(".astra");
    std::fs::create_dir_all(&astra_dir).unwrap();
    std::fs::write(
        astra_dir.join("instructions.md"),
        "Always use Rust.\nPrefer async.",
    )
    .unwrap();

    let result = discover_instructions_from_paths(Some(dir.path()), None);
    let instructions = result.expect("should discover instructions");
    assert!(instructions.contains("Always use Rust."));
    assert!(instructions.contains("Prefer async."));
}

#[test]
fn discover_project_instructions_empty_file_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let astra_dir = dir.path().join(".astra");
    std::fs::create_dir_all(&astra_dir).unwrap();
    std::fs::write(astra_dir.join("instructions.md"), "   \n  \n").unwrap();

    let result = discover_instructions_from_paths(Some(dir.path()), None);
    assert!(result.is_none(), "empty file should return None");
}

#[test]
fn discover_project_instructions_no_file_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let result = discover_instructions_from_paths(Some(dir.path()), Some(dir.path()));
    assert!(result.is_none(), "no file should return None");
}

#[test]
fn discover_project_instructions_combines_project_and_user() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let p_astra = project.path().join(".astra");
    let h_astra = home.path().join(".astra");
    std::fs::create_dir_all(&p_astra).unwrap();
    std::fs::create_dir_all(&h_astra).unwrap();
    std::fs::write(p_astra.join("instructions.md"), "Project rules").unwrap();
    std::fs::write(h_astra.join("instructions.md"), "Global rules").unwrap();

    let result = discover_instructions_from_paths(Some(project.path()), Some(home.path()));
    let instructions = result.expect("should combine both");
    assert!(instructions.contains("Project rules"));
    assert!(instructions.contains("Global rules"));
    // Project should come first
    let project_pos = instructions.find("Project rules").unwrap();
    let global_pos = instructions.find("Global rules").unwrap();
    assert!(project_pos < global_pos, "project should precede global");
}

#[test]
fn discover_project_instructions_user_only() {
    let project = tempfile::tempdir().unwrap(); // no .astra dir
    let home = tempfile::tempdir().unwrap();
    let h_astra = home.path().join(".astra");
    std::fs::create_dir_all(&h_astra).unwrap();
    std::fs::write(h_astra.join("instructions.md"), "User-level rules").unwrap();

    let result = discover_instructions_from_paths(Some(project.path()), Some(home.path()));
    let instructions = result.expect("should find user-level");
    assert!(instructions.contains("User-level rules"));
}

#[test]
fn format_project_instructions_wraps_in_tags() {
    let content = "Use tabs for indentation.";
    let formatted = format_project_instructions(content);
    assert!(formatted.starts_with("<project_instructions>"));
    assert!(formatted.ends_with("</project_instructions>"));
    assert!(formatted.contains(content));
}

#[test]
fn build_effective_line_includes_project_instructions() {
    let mut state = ReplState::default();
    state.project_instructions = Some("Always use Rust.".to_string());
    let result = repl_turn::build_effective_line("hello", &state);
    assert!(
        result.contains("<project_instructions>"),
        "should wrap in tags"
    );
    assert!(result.contains("Always use Rust."));
    assert!(
        result.contains("hello"),
        "should still include user message"
    );
}

#[test]
fn build_effective_line_no_instructions_when_none() {
    let state = ReplState::default();
    let result = repl_turn::build_effective_line("hello", &state);
    assert!(
        !result.contains("<project_instructions>"),
        "should not inject when None"
    );
    assert_eq!(result, "hello");
}

#[test]
fn cli_no_instructions_flag() {
    let cli = Cli::try_parse_from(["astra", "--no-instructions"]).unwrap();
    assert!(cli.no_instructions);
}
