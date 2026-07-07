use std::io::{BufRead, BufReader};

fn default_session_current_date() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

pub fn resolve_session_current_date(session_id: &str) -> String {
    if session_id.is_empty() {
        return default_session_current_date();
    }
    let path = astra_services::session_journal::journal_file_path(session_id);
    let Ok(file) = std::fs::File::open(path) else {
        return default_session_current_date();
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) =
            serde_json::from_str::<astra_services::session_journal::JournalEvent>(&line)
        else {
            continue;
        };
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&event.ts) else {
            continue;
        };
        return ts.date_naive().format("%Y-%m-%d").to_string();
    }
    default_session_current_date()
}

#[cfg(test)]
mod tests {
    use super::resolve_session_current_date;

    #[test]
    fn resolve_session_current_date_uses_first_journal_event_date() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000191";
        let writer = astra_services::session_journal::JournalWriter::new(session_id)
            .expect("journal writer");

        let mut start =
            astra_services::session_journal::JournalEvent::session_start(Some(session_id), None);
        start.ts = "2026-05-24T23:59:50Z".to_string();
        writer.append(&start).unwrap();

        let mut later = astra_services::session_journal::JournalEvent::llm_request_full(
            Some(session_id),
            1,
            0,
            serde_json::json!({"provider": "openai", "request": {"messages": []}}),
        );
        later.ts = "2026-05-25T00:10:00Z".to_string();
        writer.append(&later).unwrap();

        assert_eq!(
            resolve_session_current_date(session_id),
            "2026-05-24",
            "session current_date must stay anchored to the first journaled session date"
        );
    }
}
