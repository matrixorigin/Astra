use astra_services::{session_analytics, session_journal};

#[derive(Debug, Clone)]
pub(crate) struct UnreadableSessionJournal {
    pub(crate) session_id: String,
    pub(crate) error: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RecentSessionStatsScan {
    pub(crate) stats: Vec<astra_services::session_analytics::SessionStats>,
    pub(crate) unreadable: Vec<UnreadableSessionJournal>,
}

pub(crate) fn read_session_journal_for_stats(
    session_id: &str,
) -> Result<Vec<session_journal::JournalEvent>, String> {
    session_journal::read_journal(session_id)
        .map_err(|error| format!("failed to read session journal for {session_id}: {error}"))
}

pub(crate) fn list_recent_session_ids_for_stats(limit: usize) -> Result<Vec<String>, String> {
    session_journal::list_sessions_by_time(limit.max(1))
        .map_err(|error| format!("failed to scan local sessions: {error}"))
}

pub(crate) fn collect_recent_session_stats(limit: usize) -> Result<RecentSessionStatsScan, String> {
    let session_ids = list_recent_session_ids_for_stats(limit)?;
    let mut scan = RecentSessionStatsScan::default();

    for session_id in &session_ids {
        match read_session_journal_for_stats(session_id) {
            Ok(events) => scan.stats.push(session_analytics::compute_session_stats(
                session_id, &events,
            )),
            Err(error) => scan.unreadable.push(UnreadableSessionJournal {
                session_id: session_id.clone(),
                error,
            }),
        }
    }

    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::{
        collect_recent_session_stats, list_recent_session_ids_for_stats,
        read_session_journal_for_stats,
    };
    use astra_services::session_journal::{self, JournalDirGuard};

    fn write_stats_session(session_id: &str) {
        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(session_id),
                1,
                Some("gpt-5"),
                "continue",
                "restored",
                0,
                15,
                7,
                8,
            ))
            .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn list_recent_session_ids_for_stats_surfaces_scan_error() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let owner_sessions_root = session_journal::local_owner_sessions_dir();
        std::fs::create_dir_all(owner_sessions_root.parent().unwrap()).unwrap();
        std::fs::write(&owner_sessions_root, "not-a-directory").unwrap();

        let error =
            list_recent_session_ids_for_stats(10).expect_err("session scan failure should surface");

        assert!(error.contains("failed to scan local sessions"), "{error}");
    }

    #[test]
    #[serial_test::serial]
    fn collect_recent_session_stats_marks_unreadable_journals() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let good_session = format!("stats-good-{}", uuid::Uuid::new_v4());
        let bad_session = format!("stats-bad-{}", uuid::Uuid::new_v4());
        write_stats_session(&good_session);
        std::fs::create_dir_all(session_journal::journal_file_path(&bad_session)).unwrap();

        let scan = collect_recent_session_stats(10).expect("scan should succeed");

        assert_eq!(scan.stats.len(), 1);
        assert_eq!(scan.stats[0].session_id, good_session);
        assert_eq!(scan.unreadable.len(), 1);
        assert_eq!(scan.unreadable[0].session_id, bad_session);
        assert!(
            scan.unreadable[0]
                .error
                .contains("failed to read session journal"),
            "{}",
            scan.unreadable[0].error
        );
    }

    #[test]
    #[serial_test::serial]
    fn read_session_journal_for_stats_surfaces_directory_error() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("stats-dir-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&session_id)).unwrap();

        let error = read_session_journal_for_stats(&session_id)
            .expect_err("directory journal path should fail to read");

        assert!(error.contains("failed to read session journal"), "{error}");
    }
}
