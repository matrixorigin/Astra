use super::*;

#[test]
fn cli_self_snapshot_subcommand() {
    let cli = Cli::try_parse_from(["astra", "self", "snapshot"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::SelfInspect(SelfCmd::Snapshot(_)))
    ));
}

#[test]
fn cli_self_reflect_subcommand() {
    let cli = Cli::try_parse_from([
        "astra",
        "self",
        "reflect",
        "--focus",
        "performance",
        "--question",
        "why was bash slow?",
        "--last-n",
        "12",
    ])
    .unwrap();
    match cli.command {
        Some(Command::SelfInspect(SelfCmd::Reflect(args))) => {
            assert_eq!(args.focus, "performance");
            assert_eq!(args.question.as_deref(), Some("why was bash slow?"));
            assert_eq!(args.last_n, 12);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn cli_self_mutate_preview_parses() {
    let cli = Cli::try_parse_from([
        "astra",
        "self",
        "mutate",
        "preview",
        "--path",
        "verification.strictness",
        "--value",
        "0.8",
    ])
    .unwrap();
    match cli.command {
        Some(Command::SelfInspect(SelfCmd::Mutate(SelfMutateCmd::Preview(args)))) => {
            assert_eq!(args.path, "verification.strictness");
            assert_eq!(args.value, "0.8");
        }
        _ => panic!("expected self mutate preview"),
    }
}
