//! Persistence for skill evolution proposals.
//!
//! Stores proposals in `{skill_dir}/evolutions.json` and applies approved
//! skill diffs back into SKILL.md files.

use std::path::PathBuf;

use super::types::*;

/// Proposal TTL: unapproved proposals expire after 7 days.
const PROPOSAL_TTL_SECS: u64 = 7 * 24 * 3600;

/// Persists and applies skill evolution proposals.
pub struct EvolutionStore {
    skills_base_dir: PathBuf,
}

impl EvolutionStore {
    pub fn new(skills_base_dir: PathBuf) -> Self {
        Self { skills_base_dir }
    }

    /// Path to a skill's evolutions.json.
    fn evolutions_path(&self, skill_name: &str) -> PathBuf {
        self.skills_base_dir
            .join(skill_name)
            .join("evolutions.json")
    }

    /// Path to a skill's SKILL.md.
    fn skill_md_path(&self, skill_name: &str) -> PathBuf {
        self.skills_base_dir.join(skill_name).join("SKILL.md")
    }

    /// Load stored proposals for a skill. Expired proposals are filtered out.
    pub fn load(&self, skill_name: &str) -> Result<Vec<StoredProposal>, String> {
        let path = self.evolutions_path(skill_name);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let mut proposals: Vec<StoredProposal> = serde_json::from_str(&content)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;

        let now = now_epoch();
        let before = proposals.len();
        proposals.retain(|p| {
            p.status != StoredStatus::Pending
                || now.saturating_sub(p.created_at) < PROPOSAL_TTL_SECS
        });
        // If we expired any, save the cleaned list.
        if proposals.len() < before {
            let _ = self.save_proposals(skill_name, &proposals);
        }
        Ok(proposals)
    }

    /// Append a proposal to a skill's evolutions.json.
    pub fn append(&self, skill_name: &str, proposal: &EvolutionProposal) -> Result<(), String> {
        let mut proposals = self.load(skill_name)?;
        proposals.push(StoredProposal::from_proposal(proposal));
        self.save_proposals(skill_name, &proposals)
    }

    /// Mark a proposal as applied in the store.
    pub fn mark_applied(&self, skill_name: &str, proposal_id: &str) -> Result<(), String> {
        self.mark_status(skill_name, proposal_id, StoredStatus::Applied)
    }

    /// Mark a proposal as rejected in the store.
    pub fn mark_rejected(&self, skill_name: &str, proposal_id: &str) -> Result<(), String> {
        self.mark_status(skill_name, proposal_id, StoredStatus::Rejected)
    }

    fn mark_status(
        &self,
        skill_name: &str,
        proposal_id: &str,
        status: StoredStatus,
    ) -> Result<(), String> {
        let mut proposals = self.load(skill_name)?;
        if let Some(p) = proposals.iter_mut().find(|p| p.id == proposal_id) {
            p.status = status;
        }
        self.save_proposals(skill_name, &proposals)
    }

    /// Apply a skill diff to SKILL.md. Returns the updated content.
    pub fn apply_skill_diff(
        &self,
        skill_name: &str,
        section: &SkillSection,
        diff: &SkillDiff,
    ) -> Result<String, String> {
        let path = self.skill_md_path(skill_name);
        if !path.exists() {
            return Err(format!("SKILL.md not found: {}", path.display()));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

        let updated = apply_diff_to_content(&content, section, diff)?;

        std::fs::write(&path, &updated)
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        Ok(updated)
    }

    /// List skill names that have pending proposals.
    pub fn skills_with_pending(&self) -> Vec<String> {
        let mut result = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.skills_base_dir) else {
            return result;
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('_') || name.starts_with('.') {
                continue;
            }
            let evo_path = entry.path().join("evolutions.json");
            if evo_path.exists() {
                if let Ok(proposals) = self.load(&name) {
                    if proposals.iter().any(|p| p.status == StoredStatus::Pending) {
                        result.push(name);
                    }
                }
            }
        }
        result
    }

    fn save_proposals(&self, skill_name: &str, proposals: &[StoredProposal]) -> Result<(), String> {
        let path = self.evolutions_path(skill_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create dir {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(proposals)
            .map_err(|e| format!("failed to serialize: {e}"))?;
        // Atomic write: tmp file + rename to prevent corruption on crash.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)
            .map_err(|e| format!("failed to write tmp {}: {e}", tmp.display()))?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("failed to rename {}: {e}", path.display()));
        }
        Ok(())
    }
}

/// Apply a structured diff to SKILL.md content.
pub fn apply_diff_to_content(
    content: &str,
    section: &SkillSection,
    diff: &SkillDiff,
) -> Result<String, String> {
    let heading = format!("## {}", section.heading());

    match diff {
        SkillDiff::Append { content: new_text } => {
            if let Some(section_start) = content.find(&heading) {
                // Find the end of this section (next ## or EOF).
                let after_heading = section_start + heading.len();
                let section_end = content[after_heading..]
                    .find("\n## ")
                    .map(|i| after_heading + i)
                    .unwrap_or(content.len());

                let mut result = String::with_capacity(content.len() + new_text.len() + 2);
                result.push_str(&content[..section_end]);
                if !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push('\n');
                result.push_str(new_text);
                result.push('\n');
                result.push_str(&content[section_end..]);
                Ok(result)
            } else {
                // Section doesn't exist — append at end.
                let mut result = content.to_string();
                if !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push('\n');
                result.push_str(&heading);
                result.push('\n');
                result.push('\n');
                result.push_str(new_text);
                result.push('\n');
                Ok(result)
            }
        }
        SkillDiff::Replace {
            old_marker,
            new_content,
        } => {
            // Scope replacement to the target section to avoid unintended matches elsewhere.
            if let Some(section_start) = content.find(&heading) {
                let after_heading = section_start + heading.len();
                let section_end = content[after_heading..]
                    .find("\n## ")
                    .map(|i| after_heading + i)
                    .unwrap_or(content.len());
                let section_text = &content[section_start..section_end];
                if section_text.contains(old_marker.as_str()) {
                    let new_section = section_text.replacen(old_marker, new_content, 1);
                    Ok(format!(
                        "{}{}{}",
                        &content[..section_start],
                        new_section,
                        &content[section_end..]
                    ))
                } else {
                    Err(format!(
                        "marker not found in section '{}': {old_marker}",
                        section.heading()
                    ))
                }
            } else if content.contains(old_marker) {
                // Fallback: section heading not found, but marker exists globally.
                Ok(content.replacen(old_marker, new_content, 1))
            } else {
                Err(format!("marker not found in SKILL.md: {old_marker}"))
            }
        }
        SkillDiff::Remove { marker } => {
            // Scope removal to the target section.
            if let Some(section_start) = content.find(&heading) {
                let after_heading = section_start + heading.len();
                let section_end = content[after_heading..]
                    .find("\n## ")
                    .map(|i| after_heading + i)
                    .unwrap_or(content.len());
                let section_text = &content[section_start..section_end];
                if section_text.contains(marker.as_str()) {
                    let new_section = section_text.replacen(marker, "", 1);
                    Ok(format!(
                        "{}{}{}",
                        &content[..section_start],
                        new_section,
                        &content[section_end..]
                    ))
                } else {
                    Err(format!(
                        "marker not found in section '{}': {marker}",
                        section.heading()
                    ))
                }
            } else if content.contains(marker) {
                Ok(content.replacen(marker, "", 1))
            } else {
                Err(format!("marker not found in SKILL.md: {marker}"))
            }
        }
    }
}

// ── Serializable stored proposal ──

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum StoredStatus {
    Pending,
    Applied,
    Rejected,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredProposal {
    pub id: String,
    pub signal_type: String,
    pub axis_type: String,
    pub skill_name: Option<String>,
    pub section: Option<String>,
    pub diff_content: Option<String>,
    pub confidence: f64,
    pub reasoning: String,
    pub created_at: u64,
    pub status: StoredStatus,
}

impl StoredProposal {
    pub fn from_proposal(p: &EvolutionProposal) -> Self {
        let signal_type = match &p.signal {
            EvolutionSignal::ToolFailure { .. } => "tool_failure",
            EvolutionSignal::UserCorrection { .. } => "user_correction",
            EvolutionSignal::PatternDrift { .. } => "pattern_drift",
            EvolutionSignal::RepeatedStall { .. } => "repeated_stall",
            EvolutionSignal::LlmReflection { .. } => "llm_reflection",
        }
        .to_string();

        let (axis_type, skill_name, section, diff_content) = match &p.axis {
            EvolutionAxis::Skill {
                skill_name,
                section,
                diff,
            } => {
                let dc = match diff {
                    SkillDiff::Append { content } => Some(content.clone()),
                    SkillDiff::Replace { new_content, .. } => Some(new_content.clone()),
                    SkillDiff::Remove { marker } => Some(format!("[remove] {marker}")),
                };
                (
                    "skill".to_string(),
                    Some(skill_name.clone()),
                    Some(section.heading().to_string()),
                    dc,
                )
            }
            EvolutionAxis::Pattern { signature, .. } => {
                ("pattern".to_string(), None, None, Some(signature.clone()))
            }
            EvolutionAxis::Calibration { .. } => ("calibration".to_string(), None, None, None),
            EvolutionAxis::Entity { entity, .. } => {
                ("entity".to_string(), None, None, Some(entity.clone()))
            }
        };

        let status = match p.status {
            ApprovalStatus::Pending => StoredStatus::Pending,
            ApprovalStatus::Approved | ApprovalStatus::AutoApplied => StoredStatus::Applied,
            ApprovalStatus::Rejected => StoredStatus::Rejected,
        };

        Self {
            id: p.id.clone(),
            signal_type,
            axis_type,
            skill_name,
            section,
            diff_content,
            confidence: p.confidence,
            reasoning: p.reasoning.clone(),
            created_at: p.created_at,
            status,
        }
    }
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn make_store(dir: &Path) -> EvolutionStore {
        EvolutionStore::new(dir.to_path_buf())
    }

    fn make_skill_dir(base: &Path, name: &str, skill_md: &str) {
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), skill_md).unwrap();
    }

    fn make_proposal(id: &str, skill: &str) -> EvolutionProposal {
        EvolutionProposal {
            id: id.into(),
            signal: EvolutionSignal::ToolFailure {
                tool_name: "bash".into(),
                error_snippet: "err".into(),
                skill_context: Some(skill.into()),
                turn_id: "t1".into(),
            },
            axis: EvolutionAxis::Skill {
                skill_name: skill.into(),
                section: SkillSection::Troubleshooting,
                diff: SkillDiff::Append {
                    content: "- Handle timeout errors by retrying".into(),
                },
            },
            confidence: 0.8,
            reasoning: "test".into(),
            created_at: now_epoch(),
            status: ApprovalStatus::Pending,
        }
    }

    #[test]
    fn load_empty_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(tmp.path());
        let proposals = store.load("nonexistent").unwrap();
        assert!(proposals.is_empty());
    }

    #[test]
    fn append_and_load() {
        let tmp = TempDir::new().unwrap();
        make_skill_dir(tmp.path(), "my_skill", "# Skill");
        let store = make_store(tmp.path());

        let p = make_proposal("ev_1", "my_skill");
        store.append("my_skill", &p).unwrap();

        let loaded = store.load("my_skill").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "ev_1");
        assert_eq!(loaded[0].status, StoredStatus::Pending);
    }

    #[test]
    fn mark_applied() {
        let tmp = TempDir::new().unwrap();
        make_skill_dir(tmp.path(), "s", "# Skill");
        let store = make_store(tmp.path());

        store.append("s", &make_proposal("ev_1", "s")).unwrap();
        store.mark_applied("s", "ev_1").unwrap();

        let loaded = store.load("s").unwrap();
        assert_eq!(loaded[0].status, StoredStatus::Applied);
    }

    #[test]
    fn expired_proposals_cleaned_on_load() {
        let tmp = TempDir::new().unwrap();
        make_skill_dir(tmp.path(), "s", "# Skill");
        let store = make_store(tmp.path());

        // Manually write an expired proposal.
        let expired = StoredProposal {
            id: "ev_old".into(),
            signal_type: "tool_failure".into(),
            axis_type: "skill".into(),
            skill_name: Some("s".into()),
            section: Some("Troubleshooting".into()),
            diff_content: Some("old".into()),
            confidence: 0.5,
            reasoning: "old".into(),
            created_at: 0, // epoch 0 = definitely expired
            status: StoredStatus::Pending,
        };
        let json = serde_json::to_string_pretty(&vec![expired]).unwrap();
        fs::write(store.evolutions_path("s"), json).unwrap();

        let loaded = store.load("s").unwrap();
        assert!(loaded.is_empty(), "expired proposal should be cleaned");
    }

    #[test]
    fn applied_proposals_not_expired() {
        let tmp = TempDir::new().unwrap();
        make_skill_dir(tmp.path(), "s", "# Skill");
        let store = make_store(tmp.path());

        let old_applied = StoredProposal {
            id: "ev_applied".into(),
            signal_type: "tool_failure".into(),
            axis_type: "skill".into(),
            skill_name: Some("s".into()),
            section: None,
            diff_content: None,
            confidence: 0.8,
            reasoning: "applied".into(),
            created_at: 0,
            status: StoredStatus::Applied,
        };
        let json = serde_json::to_string_pretty(&vec![old_applied]).unwrap();
        fs::write(store.evolutions_path("s"), json).unwrap();

        let loaded = store.load("s").unwrap();
        assert_eq!(loaded.len(), 1, "applied proposals should not expire");
    }

    // ── Diff application tests ──

    #[test]
    fn append_to_existing_section() {
        let content = "# My Skill\n\n## Instructions\n\nDo stuff.\n\n## Examples\n\nExample 1.\n";
        let result = apply_diff_to_content(
            content,
            &SkillSection::Instructions,
            &SkillDiff::Append {
                content: "- New rule here".into(),
            },
        )
        .unwrap();
        assert!(result.contains("Do stuff."));
        assert!(result.contains("- New rule here"));
        assert!(result.contains("## Examples")); // other section preserved
        // New content should be before ## Examples
        let new_pos = result.find("- New rule here").unwrap();
        let examples_pos = result.find("## Examples").unwrap();
        assert!(new_pos < examples_pos);
    }

    #[test]
    fn append_to_nonexistent_section_creates_it() {
        let content = "# My Skill\n\n## Instructions\n\nDo stuff.\n";
        let result = apply_diff_to_content(
            content,
            &SkillSection::Troubleshooting,
            &SkillDiff::Append {
                content: "- Check logs".into(),
            },
        )
        .unwrap();
        assert!(result.contains("## Troubleshooting"));
        assert!(result.contains("- Check logs"));
    }

    #[test]
    fn replace_marker() {
        let content = "## Instructions\n\nOLD_RULE: do X\n\n## Examples\n";
        let result = apply_diff_to_content(
            content,
            &SkillSection::Instructions,
            &SkillDiff::Replace {
                old_marker: "OLD_RULE: do X".into(),
                new_content: "NEW_RULE: do Y".into(),
            },
        )
        .unwrap();
        assert!(!result.contains("OLD_RULE"));
        assert!(result.contains("NEW_RULE: do Y"));
    }

    #[test]
    fn replace_missing_marker_errors() {
        let content = "## Instructions\n\nSome content.\n";
        let result = apply_diff_to_content(
            content,
            &SkillSection::Instructions,
            &SkillDiff::Replace {
                old_marker: "NONEXISTENT".into(),
                new_content: "new".into(),
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("marker not found"));
    }

    #[test]
    fn remove_marker() {
        let content = "## Instructions\n\nKeep this.\nREMOVE_ME\nKeep this too.\n";
        let result = apply_diff_to_content(
            content,
            &SkillSection::Instructions,
            &SkillDiff::Remove {
                marker: "REMOVE_ME".into(),
            },
        )
        .unwrap();
        assert!(!result.contains("REMOVE_ME"));
        assert!(result.contains("Keep this."));
        assert!(result.contains("Keep this too."));
    }

    #[test]
    fn remove_missing_marker_errors() {
        let content = "## Instructions\n\nContent.\n";
        let result = apply_diff_to_content(
            content,
            &SkillSection::Instructions,
            &SkillDiff::Remove {
                marker: "NONEXISTENT".into(),
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn apply_skill_diff_writes_file() {
        let tmp = TempDir::new().unwrap();
        let skill_md = "# Skill\n\n## Instructions\n\nOriginal.\n";
        make_skill_dir(tmp.path(), "s", skill_md);
        let store = make_store(tmp.path());

        let result = store
            .apply_skill_diff(
                "s",
                &SkillSection::Instructions,
                &SkillDiff::Append {
                    content: "- Added rule".into(),
                },
            )
            .unwrap();
        assert!(result.contains("- Added rule"));

        // Verify file was actually written
        let on_disk = fs::read_to_string(store.skill_md_path("s")).unwrap();
        assert!(on_disk.contains("- Added rule"));
    }

    #[test]
    fn apply_skill_diff_missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(tmp.path());
        let result = store.apply_skill_diff(
            "nonexistent",
            &SkillSection::Instructions,
            &SkillDiff::Append {
                content: "x".into(),
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn skills_with_pending_lists_correctly() {
        let tmp = TempDir::new().unwrap();
        make_skill_dir(tmp.path(), "has_pending", "# S");
        make_skill_dir(tmp.path(), "no_pending", "# S");
        make_skill_dir(tmp.path(), "_hidden", "# S");
        let store = make_store(tmp.path());

        store
            .append("has_pending", &make_proposal("ev_1", "has_pending"))
            .unwrap();

        let pending = store.skills_with_pending();
        assert_eq!(pending, vec!["has_pending"]);
    }

    #[test]
    fn stored_proposal_roundtrip() {
        let p = make_proposal("ev_rt", "my_skill");
        let stored = StoredProposal::from_proposal(&p);
        let json = serde_json::to_string(&stored).unwrap();
        let back: StoredProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "ev_rt");
        assert_eq!(back.signal_type, "tool_failure");
        assert_eq!(back.axis_type, "skill");
        assert_eq!(back.skill_name.as_deref(), Some("my_skill"));
        assert_eq!(back.section.as_deref(), Some("Troubleshooting"));
    }

    #[test]
    fn append_to_last_section_no_next_heading() {
        let content = "# Skill\n\n## Troubleshooting\n\nExisting tip.\n";
        let result = apply_diff_to_content(
            content,
            &SkillSection::Troubleshooting,
            &SkillDiff::Append {
                content: "- New tip".into(),
            },
        )
        .unwrap();
        assert!(result.contains("Existing tip."));
        assert!(result.contains("- New tip"));
    }
}
