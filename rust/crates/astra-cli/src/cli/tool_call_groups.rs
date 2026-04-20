use astra_services::session_journal::ToolCallRecord;

#[derive(Debug, Clone)]
pub(crate) struct ToolCallGroup<'a> {
    pub round: Option<u32>,
    pub batch_id: Option<&'a str>,
    pub parallel: bool,
    pub calls: Vec<&'a ToolCallRecord>,
}

impl ToolCallGroup<'_> {
    pub(crate) fn ok_count(&self) -> usize {
        self.calls.iter().filter(|c| c.ok).count()
    }

    pub(crate) fn fail_count(&self) -> usize {
        self.calls.len().saturating_sub(self.ok_count())
    }
}

pub(crate) fn group_tool_calls(calls: &[ToolCallRecord]) -> Vec<ToolCallGroup<'_>> {
    let mut groups = Vec::new();

    for call in calls {
        let can_append = groups
            .last()
            .map(|group: &ToolCallGroup<'_>| {
                group.round == call.round
                    && group.batch_id.is_some()
                    && call.batch_id.as_deref().is_some()
                    && group.batch_id == call.batch_id.as_deref()
            })
            .unwrap_or(false);

        if can_append {
            let group = groups.last_mut().expect("group exists when appending");
            group.parallel = group.parallel || call.parallel.unwrap_or(false);
            group.calls.push(call);
            continue;
        }

        groups.push(ToolCallGroup {
            round: call.round,
            batch_id: call.batch_id.as_deref(),
            parallel: call.parallel.unwrap_or(false),
            calls: vec![call],
        });
    }

    for group in &mut groups {
        if group.calls.len() > 1 {
            group.parallel = true;
        }
    }

    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_call(name: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 10,
            ..Default::default()
        }
    }

    #[test]
    fn groups_parallel_batch_with_same_round_and_batch_id() {
        let mut a = make_call("read_file");
        a.round = Some(0);
        a.batch_id = Some("b-0-0".to_string());
        a.parallel = Some(true);

        let mut b = make_call("grep");
        b.round = Some(0);
        b.batch_id = Some("b-0-0".to_string());
        b.parallel = Some(true);

        let calls = [a, b];
        let groups = group_tool_calls(&calls);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].round, Some(0));
        assert_eq!(groups[0].batch_id, Some("b-0-0"));
        assert!(groups[0].parallel);
        assert_eq!(groups[0].calls.len(), 2);
    }

    #[test]
    fn keeps_unbatched_calls_separate_even_in_same_round() {
        let mut a = make_call("read_file");
        a.round = Some(0);

        let mut b = make_call("grep");
        b.round = Some(0);

        let calls = [a, b];
        let groups = group_tool_calls(&calls);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].calls.len(), 1);
        assert_eq!(groups[1].calls.len(), 1);
    }

    #[test]
    fn separates_batches_across_rounds() {
        let mut a = make_call("read_file");
        a.round = Some(0);
        a.batch_id = Some("b-0-0".to_string());

        let mut b = make_call("grep");
        b.round = Some(1);
        b.batch_id = Some("b-1-0".to_string());

        let calls = [a, b];
        let groups = group_tool_calls(&calls);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].round, Some(0));
        assert_eq!(groups[1].round, Some(1));
    }
}
