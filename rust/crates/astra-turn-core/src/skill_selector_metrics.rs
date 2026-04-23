use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub const SKILL_SELECTOR_RECENT_WINDOW_SIZE: i64 = 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSelectorShortlistEntry {
    pub rank: i32,
    pub skill_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub description: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSelectorShortlistTrace {
    pub open_catalog: bool,
    #[serde(default)]
    pub visible_skill_count: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillSelectorShortlistEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSelectorMetricComputation {
    pub visible_skill_count: i64,
    pub chosen_skill_count: i64,
    pub shortlisted_chosen_count: i64,
    pub missed_chosen_count: i64,
    pub best_chosen_rank: Option<i64>,
    pub hit_at_1: bool,
    pub hit_at_3: bool,
    pub hit_at_5: bool,
    pub hit_at_14: bool,
}

pub fn compute_skill_selector_metric(
    shortlist: &SkillSelectorShortlistTrace,
    chosen_skills: &[String],
) -> Option<SkillSelectorMetricComputation> {
    let mut seen_chosen = HashSet::new();
    let mut chosen_skill_count = 0_i64;
    let mut shortlisted_chosen_count = 0_i64;
    let mut best_rank: Option<i64> = None;
    let mut matched_skills = HashSet::new();

    for chosen in chosen_skills {
        let chosen = chosen.trim();
        if chosen.is_empty() {
            continue;
        }
        let chosen_lower = chosen.to_lowercase();
        let matched = shortlist.skills.iter().find(|entry| {
            entry.skill_name.eq_ignore_ascii_case(chosen)
                || entry
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(chosen))
        });

        let dedup_key = matched
            .map(|entry| format!("shortlist:{}", entry.skill_name.to_lowercase()))
            .unwrap_or_else(|| format!("raw:{chosen_lower}"));
        if !seen_chosen.insert(dedup_key) {
            continue;
        }

        chosen_skill_count += 1;
        if let Some(entry) = matched {
            let matched_key = entry.skill_name.to_lowercase();
            if matched_skills.insert(matched_key) {
                shortlisted_chosen_count += 1;
                let rank = i64::from(entry.rank);
                best_rank = Some(best_rank.map_or(rank, |current| current.min(rank)));
            }
        }
    }

    if chosen_skill_count == 0 {
        return None;
    }

    let missed_chosen_count = chosen_skill_count.saturating_sub(shortlisted_chosen_count);

    Some(SkillSelectorMetricComputation {
        visible_skill_count: i64::from(shortlist.visible_skill_count.max(0)),
        chosen_skill_count,
        shortlisted_chosen_count,
        missed_chosen_count,
        best_chosen_rank: best_rank,
        hit_at_1: best_rank.is_some_and(|rank| rank <= 1),
        hit_at_3: best_rank.is_some_and(|rank| rank <= 3),
        hit_at_5: best_rank.is_some_and(|rank| rank <= 5),
        hit_at_14: best_rank.is_some_and(|rank| rank <= 14),
    })
}

pub fn skill_selector_window_overflow(total_rows: i64, window_size: i64) -> u64 {
    (total_rows - window_size.max(0))
        .max(0)
        .try_into()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortlist(names: &[(&str, &[&str])]) -> SkillSelectorShortlistTrace {
        SkillSelectorShortlistTrace {
            open_catalog: true,
            visible_skill_count: names.len() as i32,
            skills: names
                .iter()
                .enumerate()
                .map(|(idx, (name, aliases))| SkillSelectorShortlistEntry {
                    rank: idx as i32 + 1,
                    skill_name: (*name).to_string(),
                    aliases: aliases.iter().map(|alias| (*alias).to_string()).collect(),
                    description: format!("{name} description"),
                    source: "local".to_string(),
                    category: None,
                })
                .collect(),
        }
    }

    #[test]
    fn computes_best_rank_and_hits_for_multiskill_turn() {
        let trace = shortlist(&[
            ("alpha", &[]),
            ("beta", &["beta-alt"]),
            ("gamma", &[]),
            ("delta", &[]),
        ]);

        let metric = compute_skill_selector_metric(
            &trace,
            &[
                "beta-alt".to_string(),
                "outside".to_string(),
                "delta".to_string(),
            ],
        )
        .expect("metric should exist");

        assert_eq!(metric.visible_skill_count, 4);
        assert_eq!(metric.chosen_skill_count, 3);
        assert_eq!(metric.shortlisted_chosen_count, 2);
        assert_eq!(metric.missed_chosen_count, 1);
        assert_eq!(metric.best_chosen_rank, Some(2));
        assert!(!metric.hit_at_1);
        assert!(metric.hit_at_3);
        assert!(metric.hit_at_5);
        assert!(metric.hit_at_14);
    }

    #[test]
    fn dedups_alias_and_canonical_for_same_skill() {
        let trace = shortlist(&[("deploy", &["ship-it"])]);

        let metric = compute_skill_selector_metric(
            &trace,
            &[
                "ship-it".to_string(),
                "Deploy".to_string(),
                "ship-it".to_string(),
            ],
        )
        .expect("metric should exist");

        assert_eq!(metric.chosen_skill_count, 1);
        assert_eq!(metric.shortlisted_chosen_count, 1);
        assert_eq!(metric.missed_chosen_count, 0);
        assert_eq!(metric.best_chosen_rank, Some(1));
        assert!(metric.hit_at_1);
    }

    #[test]
    fn returns_none_when_no_skill_was_chosen() {
        let trace = shortlist(&[("deploy", &[])]);
        assert!(compute_skill_selector_metric(&trace, &[]).is_none());
        assert!(compute_skill_selector_metric(&trace, &[" ".to_string()]).is_none());
    }

    #[test]
    fn computes_window_overflow() {
        assert_eq!(skill_selector_window_overflow(1000, 1000), 0);
        assert_eq!(skill_selector_window_overflow(1003, 1000), 3);
        assert_eq!(skill_selector_window_overflow(500, 1000), 0);
    }
}
