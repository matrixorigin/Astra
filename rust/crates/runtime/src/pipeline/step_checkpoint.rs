//! Step checkpoint persistence — write/read StepCheckpoint JSON to local filesystem.
//!
//! Stores checkpoints at:
//! `~/.mo-agent/sessions/<session_id>/step_checkpoints/<number>-<tier>.json`
//!
//! Light checkpoints (~1KB) written after each tool completion.
//! Heavy checkpoints (~10-100KB) written after each turn's verdict.
//! On crash recovery, the latest heavy checkpoint restores full session state.

use std::path::{Path, PathBuf};

use super::step_protocol::{
    CheckpointTier, HeavyCheckpoint, LightCheckpoint, StepCheckpoint,
};

/// Directory name within session workspace for step checkpoints.
const STEP_CHECKPOINT_DIR: &str = "step_checkpoints";

/// Maximum number of light checkpoints to retain (older ones pruned).
const MAX_LIGHT_CHECKPOINTS: usize = 50;

/// Get the step checkpoint directory for a session.
fn checkpoint_dir_for(session_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mo-agent")
        .join("sessions")
        .join(session_id)
        .join(STEP_CHECKPOINT_DIR)
}

/// Write a step checkpoint to local filesystem.
/// Returns the path where the checkpoint was written.
pub fn write_step_checkpoint(
    session_id: &str,
    number: u32,
    checkpoint: &StepCheckpoint,
) -> std::io::Result<PathBuf> {
    let dir = checkpoint_dir_for(session_id);
    std::fs::create_dir_all(&dir)?;

    let tier = match checkpoint {
        StepCheckpoint::Light(_) => "light",
        StepCheckpoint::Heavy(_) => "heavy",
    };
    let filename = format!("{:06}-{}.json", number, tier);
    let path = dir.join(&filename);

    let json = serde_json::to_string(checkpoint)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Atomic write: write to temp file, then rename
    let tmp_path = dir.join(format!(".tmp-{}", filename));
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, &path)?;

    // Prune old light checkpoints if too many
    if tier == "light" {
        prune_light_checkpoints(&dir)?;
    }

    Ok(path)
}

/// Read the latest heavy checkpoint for session recovery.
/// Returns None if no heavy checkpoint exists.
pub fn read_latest_heavy_checkpoint(session_id: &str) -> std::io::Result<Option<HeavyCheckpoint>> {
    let dir = checkpoint_dir_for(session_id);
    if !dir.exists() {
        return Ok(None);
    }

    let mut heavy_files: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("-heavy.json")
        })
        .collect();

    // Sort by name descending (latest = highest number)
    heavy_files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    if let Some(entry) = heavy_files.first() {
        let content = std::fs::read_to_string(entry.path())?;
        let checkpoint: StepCheckpoint = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        match checkpoint {
            StepCheckpoint::Heavy(boxed) => Ok(Some(*boxed)),
            _ => Ok(None),
        }
    } else {
        Ok(None)
    }
}

/// Read the latest light checkpoint (for quick cursor restore).
pub fn read_latest_light_checkpoint(
    session_id: &str,
) -> std::io::Result<Option<LightCheckpoint>> {
    let dir = checkpoint_dir_for(session_id);
    if !dir.exists() {
        return Ok(None);
    }

    // Any checkpoint contains cursor info — find the highest numbered file
    let mut all_files: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .collect();

    all_files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    if let Some(entry) = all_files.first() {
        let content = std::fs::read_to_string(entry.path())?;
        let checkpoint: StepCheckpoint = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        match checkpoint {
            StepCheckpoint::Light(light) => Ok(Some(light)),
            StepCheckpoint::Heavy(heavy) => Ok(Some(heavy.light)),
        }
    } else {
        Ok(None)
    }
}

/// List all checkpoint numbers and tiers for a session.
pub fn list_checkpoints(session_id: &str) -> std::io::Result<Vec<(u32, CheckpointTier)>> {
    let dir = checkpoint_dir_for(session_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_suffix(".json") {
            if let Some((num_str, tier_str)) = rest.split_once('-') {
                if let Ok(num) = num_str.parse::<u32>() {
                    let tier = match tier_str {
                        "light" => CheckpointTier::Light,
                        "heavy" => CheckpointTier::Heavy,
                        _ => continue,
                    };
                    result.push((num, tier));
                }
            }
        }
    }
    result.sort_by_key(|(n, _)| *n);
    Ok(result)
}

/// Remove old light checkpoints, keeping only the most recent MAX_LIGHT_CHECKPOINTS.
fn prune_light_checkpoints(dir: &Path) -> std::io::Result<()> {
    let mut light_files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .ends_with("-light.json")
        })
        .collect();

    if light_files.len() <= MAX_LIGHT_CHECKPOINTS {
        return Ok(());
    }

    // Sort ascending by name, remove oldest
    light_files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    let to_remove = light_files.len() - MAX_LIGHT_CHECKPOINTS;
    for entry in light_files.into_iter().take(to_remove) {
        let _ = std::fs::remove_file(entry.path());
    }

    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::step_protocol::{
        ExecutionCursor, PROTOCOL_VERSION,
    };
    use serde_json::json;

    fn make_light(step_id: &str, progress: f64) -> LightCheckpoint {
        LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor: ExecutionCursor::for_act(1),
            step_id: step_id.to_string(),
            task_id: "task-1".to_string(),
            agent_id: "agent-1".to_string(),
            progress,
            total_tokens: 1000,
            created_at: 1234567890,
        }
    }

    fn make_heavy(step_id: &str, messages: Vec<serde_json::Value>) -> HeavyCheckpoint {
        HeavyCheckpoint {
            light: make_light(step_id, 0.5),
            messages,
            budget_remaining_tokens: 50000,
            budget_remaining_rounds: 8,
            blocked_tools: vec!["bash".to_string()],
            recent_tools: vec!["grep".to_string(), "read_file".to_string()],
            learning_snapshot_id: None,
            memory_context: None,
        }
    }

    #[test]
    fn write_and_read_light_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("step_checkpoints");
        std::fs::create_dir_all(&dir).unwrap();

        let light = make_light("step-1", 0.75);
        let cp = StepCheckpoint::Light(light);
        let json_str = serde_json::to_string(&cp).unwrap();

        let path = dir.join("000001-light.json");
        std::fs::write(&path, &json_str).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&content).unwrap();
        match restored {
            StepCheckpoint::Light(l) => {
                assert_eq!(l.step_id, "step-1");
                assert!((l.progress - 0.75).abs() < f64::EPSILON);
                assert_eq!(l.protocol_version, PROTOCOL_VERSION);
            }
            _ => panic!("Expected Light checkpoint"),
        }
    }

    #[test]
    fn write_and_read_heavy_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("step_checkpoints");
        std::fs::create_dir_all(&dir).unwrap();

        let msgs = vec![
            json!({"role": "user", "content": "fix the bug"}),
            json!({"role": "assistant", "content": "I'll check the code."}),
        ];
        let heavy = make_heavy("step-2", msgs);
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = serde_json::to_string(&cp).unwrap();

        let path = dir.join("000002-heavy.json");
        std::fs::write(&path, &json_str).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&content).unwrap();
        match restored {
            StepCheckpoint::Heavy(h) => {
                assert_eq!(h.light.step_id, "step-2");
                assert_eq!(h.messages.len(), 2);
                assert_eq!(h.budget_remaining_tokens, 50000);
                assert_eq!(h.blocked_tools, vec!["bash"]);
                assert_eq!(h.recent_tools, vec!["grep", "read_file"]);
            }
            _ => panic!("Expected Heavy checkpoint"),
        }
    }

    #[test]
    fn checkpoint_serialization_roundtrip() {
        let light = make_light("step-rt", 0.33);
        let cp = StepCheckpoint::Light(light);
        let json_str = serde_json::to_string_pretty(&cp).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&json_str).unwrap();
        let json2 = serde_json::to_string_pretty(&restored).unwrap();
        assert_eq!(json_str, json2, "Round-trip serialization must be stable");
    }

    #[test]
    fn heavy_checkpoint_preserves_cjk_messages() {
        let msgs = vec![
            json!({"role": "system", "content": "You are a helpful assistant."}),
            json!({"role": "user", "content": "帮我查一下PR状态"}),
            json!({"role": "assistant", "content": "好的，让我查看一下。"}),
        ];
        let heavy = make_heavy("step-cjk", msgs);
        let json_str = serde_json::to_string(&heavy).unwrap();
        let restored: HeavyCheckpoint = serde_json::from_str(&json_str).unwrap();
        assert_eq!(restored.messages.len(), 3);
        assert_eq!(restored.messages[1]["content"], "帮我查一下PR状态");
    }

    #[test]
    fn checkpoint_tier_from_filename() {
        let name = "000005-heavy.json";
        let rest = name.strip_suffix(".json").unwrap();
        let (num_str, tier_str) = rest.split_once('-').unwrap();
        assert_eq!(num_str.parse::<u32>().unwrap(), 5);
        assert_eq!(tier_str, "heavy");

        let name2 = "000012-light.json";
        let rest2 = name2.strip_suffix(".json").unwrap();
        let (num_str2, tier_str2) = rest2.split_once('-').unwrap();
        assert_eq!(num_str2.parse::<u32>().unwrap(), 12);
        assert_eq!(tier_str2, "light");
    }

    #[test]
    fn prune_respects_max_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        for i in 0..(MAX_LIGHT_CHECKPOINTS + 10) {
            let name = format!("{:06}-light.json", i);
            std::fs::write(dir.join(&name), "{}").unwrap();
        }

        prune_light_checkpoints(dir).unwrap();

        let remaining: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with("-light.json"))
            .collect();

        assert_eq!(remaining.len(), MAX_LIGHT_CHECKPOINTS);
    }

    #[test]
    fn prune_keeps_heavy_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        for i in 0..5 {
            std::fs::write(dir.join(format!("{:06}-light.json", i)), "{}").unwrap();
            std::fs::write(dir.join(format!("{:06}-heavy.json", i)), "{}").unwrap();
        }

        prune_light_checkpoints(dir).unwrap();

        let heavy_count = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with("-heavy.json"))
            .count();

        assert_eq!(heavy_count, 5, "Heavy checkpoints must not be pruned");
    }

    #[test]
    fn write_step_checkpoint_creates_dir_and_file() {
        // Use a unique session ID with tempdir-like suffix to avoid collision
        let session_id = format!("test-step-cp-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id);

        // Clean up from any previous run
        let _ = std::fs::remove_dir_all(&dir);

        let light = make_light("step-write-test", 1.0);
        let cp = StepCheckpoint::Light(light);
        let result = write_step_checkpoint(&session_id, 1, &cp);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.exists());

        // Read back
        let content = std::fs::read_to_string(&path).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&content).unwrap();
        match restored {
            StepCheckpoint::Light(l) => assert_eq!(l.step_id, "step-write-test"),
            _ => panic!("Expected Light"),
        }

        // Clean up
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_latest_heavy_on_empty_returns_none() {
        let result = read_latest_heavy_checkpoint("nonexistent-session-xyz-42");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }
}
