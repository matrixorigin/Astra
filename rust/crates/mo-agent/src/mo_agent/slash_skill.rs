use super::*;

pub(super) async fn handle_skill_command(
    arg: &str,
    api: &mo_thin_client::ThinClient,
    state: &mut ReplState,
    token: Option<&str>,
) -> Result<(), String> {
    // Parse subcommand and remaining args from `arg`
    let mut sub_parts = arg.splitn(2, ' ');
    let sub = sub_parts.next().unwrap_or("").trim();
    let sub_arg = sub_parts.next().unwrap_or("").trim();

    // Route based on subcommand
    match sub {
        "" | "list" => {
            // List skills from API
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };
            let body = api
                .get_skills_query_text(tok, &[("limit", "50".into()), ("offset", "0".into())])
                .await
                .map_err(map_thin_err)?;
            let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let skills = value
                .as_array()
                .cloned()
                .or_else(|| value.get("skills").and_then(|v| v.as_array()).cloned())
                .unwrap_or_default();
            eprintln!(
                "\n{}",
                format!("{:<30}  {:<10}  {}", "Name", "Version", "Description").bold()
            );
            eprintln!("{}", "\u{2500}".repeat(70).dim());
            for s in &skills {
                let name = s
                    .get("skill_name")
                    .or_else(|| s.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let version = s
                    .get("skill_version")
                    .or_else(|| s.get("version"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let desc = s.get("description").and_then(|v| v.as_str()).unwrap_or("");
                let desc_s = if desc.len() > 40 {
                    format!("{}\u{2026}", &desc[..40])
                } else {
                    desc.to_string()
                };
                eprintln!("  {:<28}  {:<10}  {}", name.cyan(), version.dim(), desc_s);
            }
            eprintln!();
        }

        "new" => {
            let name = sub_arg;
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill new <name>".yellow());
                return Ok(());
            }
            let skills_base = std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(".mo-agent/skills");
            let skill_dir = skills_base.join(name);
            if skill_dir.exists() {
                eprintln!(
                    "{}",
                    format!(
                        "  \u{2717} Skill directory already exists: {}",
                        skill_dir.display()
                    )
                    .yellow()
                );
                return Ok(());
            }
            std::fs::create_dir_all(&skill_dir).map_err(|e| e.to_string())?;

            let skill_py = format!(
                r#""""{name} skill."""
from pydantic import BaseModel

class Input(BaseModel):
    query: str

class Output(BaseModel):
    result: str

# side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)
# runtime = [RuntimeRequirement.NETWORK]

async def execute(input: Input) -> Output:
    # TODO: implement your skill logic here
    return Output(result=f"Hello from {{input.query}}")
"#
            );
            std::fs::write(skill_dir.join("skill.py"), skill_py).map_err(|e| e.to_string())?;

            let test_skill_py = r#"""""Basic local tests for the skill scaffold."""
import asyncio
import unittest

from skill import Input, Output, execute


class SkillScaffoldTests(unittest.TestCase):
    def test_execute_returns_output(self) -> None:
        result = asyncio.run(execute(Input(query="world")))
        self.assertIsInstance(result, Output)
        self.assertEqual(result.result, "Hello from world")


if __name__ == "__main__":
    unittest.main()
"#;
            std::fs::write(skill_dir.join("test_skill.py"), test_skill_py)
                .map_err(|e| e.to_string())?;

            let skill_json = serde_json::json!({
                "name": name,
                "version": "0.1.0",
                "description": ""
            });
            std::fs::write(
                skill_dir.join("skill.json"),
                serde_json::to_string_pretty(&skill_json).unwrap(),
            )
            .map_err(|e| e.to_string())?;

            eprintln!(
                "  {} Skill scaffolded: {}",
                "\u{2713}".green(),
                skill_dir.display().to_string().cyan()
            );
            eprintln!("  Files created: skill.py, test_skill.py, skill.json");
            eprintln!("  {}", format!("Test: /skill test {name}").dim());
            eprintln!("  {}", format!("Dev mode: /skill dev {name}").dim());
        }

        "test" => {
            let name = sub_arg.split(' ').next().unwrap_or("").trim();
            let json_args = sub_arg.split_once(' ').map(|x| x.1).unwrap_or("").trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill test <name> [json_args]".yellow());
                return Ok(());
            }
            eprintln!(
                "\n{}",
                format!("─── Skill test: {name} ───────────────────────────────────────").bold()
            );
            if !json_args.is_empty() {
                eprintln!("  Input: {}", json_args.cyan());
            }

            // Try API first
            let api_ok = if let Some(tok) = token {
                let payload = serde_json::json!({
                    "skill_id": name,
                    "args": if json_args.is_empty() {
                        serde_json::Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(json_args).unwrap_or(serde_json::Value::String(json_args.to_string()))
                    }
                });
                match api.post_skills_test_json(tok, &payload).await {
                    Ok(body) => {
                        eprintln!("  {}", "\u{2713} API test result:".green());
                        eprintln!("  {body}");
                        true
                    }
                    Err(_) => false,
                }
            } else {
                false
            };

            if !api_ok {
                eprintln!("  Running local skill tests...");
                let skill_dir = std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .join(".mo-agent/skills")
                    .join(name);
                let test_file = skill_dir.join("test_skill.py");
                if test_file.exists() {
                    let out = std::process::Command::new("python3")
                        .args([
                            "-m",
                            "unittest",
                            "discover",
                            "-s",
                            ".",
                            "-p",
                            "test_*.py",
                            "-q",
                        ])
                        .current_dir(&skill_dir)
                        .output();
                    match out {
                        Ok(o) => {
                            let stdout = String::from_utf8_lossy(&o.stdout);
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            if o.status.success() {
                                eprintln!("  {}", "\u{2713} Local skill tests passed".green());
                            } else {
                                eprintln!("  {}", "\u{2717} Local skill tests failed".red());
                            }
                            if !stdout.is_empty() {
                                eprintln!("{stdout}");
                            }
                            if !stderr.is_empty() {
                                eprintln!("{stderr}");
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "{}",
                                format!("  \u{2717} Failed to run local skill tests: {e}").red()
                            );
                        }
                    }
                } else {
                    eprintln!(
                                "  {}",
                                "No test file found. Create test_skill.py in the skill directory or re-run /skill new with a fresh name."
                                    .yellow()
                            );
                }
            }
            eprintln!();
        }

        "dev" => {
            if sub_arg == "off" {
                // Exit dev mode
                state.skill_dev_name = None;
                state.skill_dev_dir = None;
                state.skill_dev_context = None;
                eprintln!("  {}", "Exited skill dev mode".green());
                return Ok(());
            }
            let name = sub_arg;
            if name.is_empty() {
                if let Some(ref current) = state.skill_dev_name.clone() {
                    eprintln!(
                        "  \u{1f527} Currently in skill dev mode: {}",
                        current.as_str().cyan()
                    );
                    eprintln!("  Use /skill dev off to exit.");
                } else {
                    eprintln!(
                        "{}",
                        "  Usage: /skill dev <name>  (or /skill dev off)".yellow()
                    );
                }
                return Ok(());
            }
            let skill_dir = std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(".mo-agent/skills")
                .join(name);
            let skill_py_path = skill_dir.join("skill.py");
            if !skill_py_path.exists() {
                eprintln!(
                    "{}",
                    format!(
                        "  \u{2717} skill.py not found in {}. Use /skill new {name} to scaffold.",
                        skill_dir.display()
                    )
                    .yellow()
                );
                return Ok(());
            }
            let skill_src = std::fs::read_to_string(&skill_py_path).map_err(|e| e.to_string())?;
            state.skill_dev_name = Some(name.to_string());
            state.skill_dev_dir = Some(skill_dir.display().to_string());
            state.skill_dev_context = Some(skill_src);
            eprintln!(
                "\n  \u{1f527} {} {}",
                "Skill dev mode:".bold(),
                name.cyan().bold()
            );
            eprintln!("  {}", format!("Dir: {}", skill_dir.display()).dim());
            eprintln!(
                "  {}",
                "Skill source is injected into each turn. Ask me to improve it.".dim()
            );
            eprintln!("  {}", "Exit: /skill dev off".dim());
            eprintln!();
        }

        "doctor" => {
            eprintln!(
                "\n{}",
                "─── Skill Health ──────────────────────────────────────────────".bold()
            );
            // Try API first
            let api_ok = if let Some(tok) = token {
                match api.get_skills_status_query_text(tok, &[]).await {
                    Ok(body) => {
                        let value: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        let skills = value
                            .as_array()
                            .cloned()
                            .or_else(|| value.get("skills").and_then(|v| v.as_array()).cloned())
                            .unwrap_or_default();
                        eprintln!(
                            "{}",
                            format!(
                                "{:<28}  {:<10}  {:<8}  {}",
                                "Name", "Registered", "Healthy", "Issues"
                            )
                            .bold()
                        );
                        eprintln!("{}", "\u{2500}".repeat(70).dim());
                        for s in &skills {
                            let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let registered = s
                                .get("registered")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let healthy =
                                s.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false);
                            let issues = s.get("issues").and_then(|v| v.as_str()).unwrap_or("");
                            eprintln!(
                                "  {:<26}  {:<10}  {:<8}  {}",
                                name.cyan(),
                                if registered {
                                    "\u{2713}".green().to_string()
                                } else {
                                    "\u{2717}".red().to_string()
                                },
                                if healthy {
                                    "\u{2713}".green().to_string()
                                } else {
                                    "\u{2717}".red().to_string()
                                },
                                issues
                            );
                        }
                        eprintln!();
                        true
                    }
                    _ => false,
                }
            } else {
                false
            };

            if !api_ok {
                // Scan local skill directories
                let skills_base = std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .join(".mo-agent/skills");
                if !skills_base.exists() {
                    eprintln!(
                        "  {}",
                        "No local skills found (.mo-agent/skills/ does not exist).".dim()
                    );
                    return Ok(());
                }
                eprintln!(
                    "{}",
                    format!(
                        "{:<28}  {:<10}  {:<14}  {}",
                        "Name", "skill.py", "test_skill.py", "skill.json"
                    )
                    .bold()
                );
                eprintln!("{}", "\u{2500}".repeat(78).dim());
                let entries = std::fs::read_dir(&skills_base).map_err(|e| e.to_string())?;
                let mut found = false;
                for entry in entries.flatten() {
                    if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                        let name = entry.file_name().to_string_lossy().to_string();
                        let has_py = entry.path().join("skill.py").exists();
                        let has_test = entry.path().join("test_skill.py").exists();
                        let has_json = entry.path().join("skill.json").exists();
                        let py_s = if has_py {
                            "\u{2713}".green().to_string()
                        } else {
                            "\u{2717} missing".red().to_string()
                        };
                        let test_s = if has_test {
                            "\u{2713}".green().to_string()
                        } else {
                            "\u{2717} missing".red().to_string()
                        };
                        let json_s = if has_json {
                            "\u{2713}".green().to_string()
                        } else {
                            "\u{2717} missing".red().to_string()
                        };
                        eprintln!(
                            "  {:<26}  {:<10}  {:<14}  {}",
                            name.cyan(),
                            py_s,
                            test_s,
                            json_s
                        );
                        found = true;
                    }
                }
                if !found {
                    eprintln!("  {}", "No skill directories found.".dim());
                }
                eprintln!();
            }
        }

        "validate" => {
            let name = sub_arg;
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill validate <name>".yellow());
                return Ok(());
            }
            let skill_dir = std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(".mo-agent/skills")
                .join(name);
            let skill_py_path = skill_dir.join("skill.py");
            if !skill_py_path.exists() {
                eprintln!(
                    "{}",
                    format!("  \u{2717} skill.py not found in {}", skill_dir.display()).red()
                );
                return Ok(());
            }
            let src = std::fs::read_to_string(&skill_py_path).map_err(|e| e.to_string())?;
            let mut issues: Vec<String> = Vec::new();
            if !src.contains("async def execute") {
                issues.push("missing `async def execute`".to_string());
            }
            if src.contains("\ndef execute") || src.contains("\r\ndef execute") {
                issues.push(
                    "found non-async `def execute` (should be `async def execute`)".to_string(),
                );
            }
            if !src.contains("class Input") {
                issues.push("missing `Input` class".to_string());
            }
            if !src.contains("class Output") {
                issues.push("missing `Output` class".to_string());
            }
            if issues.is_empty() {
                eprintln!(
                    "  {} {}",
                    "\u{2713}".green(),
                    format!("{name} looks valid").green()
                );
            } else {
                eprintln!("  {} {} issue(s):", "\u{2717}".red(), issues.len());
                for issue in &issues {
                    eprintln!("    - {}", issue.as_str().yellow());
                }
            }
        }

        "config" => {
            let name = sub_arg;
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill config <name>".yellow());
                return Ok(());
            }
            let skill_dir = std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(".mo-agent/skills")
                .join(name);
            let json_path = skill_dir.join("skill.json");
            if !json_path.exists() {
                eprintln!(
                    "{}",
                    format!("  \u{2717} skill.json not found in {}", skill_dir.display()).red()
                );
                return Ok(());
            }
            let raw = std::fs::read_to_string(&json_path).map_err(|e| e.to_string())?;
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            let pretty = serde_json::to_string_pretty(&value).unwrap_or(raw);
            eprintln!(
                "\n{}",
                format!("─── {name}/skill.json ─────────────────────────────────────────").bold()
            );
            for line in pretty.lines() {
                eprintln!("  {line}");
            }
            eprintln!();
        }

        "system" => {
            let available = prompts::builtin_system_skills();
            if sub_arg.is_empty() || sub_arg == "list" {
                eprintln!("\n  {}", "System Skills".bold());
                for skill in &available {
                    let active = state
                        .active_system_skills
                        .iter()
                        .any(|s| s.name == skill.name);
                    let marker = if active {
                        "●".green().to_string()
                    } else {
                        "○".dim().to_string()
                    };
                    eprintln!(
                        "  {} {:<12} {}",
                        marker,
                        skill.name.as_str().cyan(),
                        skill.description.as_str().dim()
                    );
                }
                if state.active_system_skills.is_empty() {
                    eprintln!(
                        "\n  {}",
                        "No active system skills. Use /skill system <name> to toggle.".dim()
                    );
                } else {
                    let names: Vec<&str> = state
                        .active_system_skills
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect();
                    eprintln!("\n  Active: {}", names.join(", ").green());
                }
                eprintln!();
            } else {
                let name = sub_arg;
                if let Some(pos) = state
                    .active_system_skills
                    .iter()
                    .position(|s| s.name == name)
                {
                    state.active_system_skills.remove(pos);
                    eprintln!(
                        "  {} System skill {} {}",
                        "○".dim(),
                        name.cyan(),
                        "deactivated".dim()
                    );
                } else if let Some(skill) = available.iter().find(|s| s.name == name) {
                    state.active_system_skills.push(skill.clone());
                    eprintln!(
                        "  {} System skill {} {}",
                        "●".green(),
                        name.cyan(),
                        "activated".green()
                    );
                } else {
                    let names: Vec<&str> = available.iter().map(|s| s.name.as_str()).collect();
                    eprintln!(
                        "{}",
                        format!(
                            "  Unknown skill: '{}'. Available: {}",
                            name,
                            names.join(", ")
                        )
                        .yellow()
                    );
                }
            }
        }

        _ => {
            eprintln!(
                        "{}",
                        format!("  Unknown /skill subcommand: '{sub}'. Try /skill, /skill new, /skill test, /skill dev, /skill doctor, /skill validate, /skill config, /skill system").yellow()
                    );
        }
    }
    Ok(())
}
