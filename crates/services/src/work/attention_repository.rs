use super::attention::{
    WorkAttentionCursorAdvance, WorkAttentionReceipt, WorkAttentionReceiptRevision,
};
use super::events::WorkEventSeq;
use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{WorkContentHash, WorkId, WorkOwnerId};
use serde::Serialize;
use sqlx::{MySqlConnection, query, query_as};

#[derive(Serialize)]
struct ReceiptHashInput<'a> {
    schema_version: u16,
    owner_id: &'a WorkOwnerId,
    work_id: &'a WorkId,
    kind: &'static str,
    through_event_seq: WorkEventSeq,
}

#[derive(sqlx::FromRow)]
struct ReadCursor {
    last_event_seq: i64,
    seen_through_event_seq: i64,
    receipt_revision: i64,
    seen_updated_at: chrono::NaiveDateTime,
}

pub(super) async fn advance_cursor(
    repository: &DatabaseWorkRepository,
    advance: WorkAttentionCursorAdvance,
) -> Result<WorkAttentionReceipt, WorkRepositoryError> {
    let mut connection = repository.pool.get().acquire().await.map_err(|source| {
        WorkRepositoryError::persistence("acquire Work attention connection", source)
    })?;
    let cursor = load_cursor(&mut connection, &advance).await?;
    let through = advance.through_event_seq.get();
    if through > cursor.last_event_seq {
        return Err(WorkRepositoryError::EventCursorAhead {
            through_event_seq: through,
            event_head: cursor.last_event_seq,
        });
    }
    if cursor.seen_through_event_seq >= through {
        return receipt(&advance, cursor);
    }

    // The committed event head is monotonic. Compare and advance with one
    // write, then read the current receipt without holding the Work row.
    query(
        "UPDATE works
         SET seen_through_event_seq = ?, receipt_revision = receipt_revision + 1,
             seen_updated_at = NOW(6)
         WHERE owner_id = ? AND work_id = ? AND seen_through_event_seq < ?
           AND receipt_revision < ?",
    )
    .bind(through)
    .bind(advance.owner_id.as_str())
    .bind(advance.work_id.as_str())
    .bind(through)
    .bind(i64::MAX)
    .execute(&mut *connection)
    .await
    .map_err(|source| WorkRepositoryError::persistence("advance Work attention cursor", source))?;
    let cursor = load_cursor(&mut connection, &advance).await?;
    if cursor.seen_through_event_seq < through {
        return Err(WorkRepositoryError::corrupt(
            "Work attention receipt",
            std::io::Error::other("receipt revision is exhausted before the requested cursor"),
        ));
    }
    receipt(&advance, cursor)
}

async fn load_cursor(
    connection: &mut MySqlConnection,
    advance: &WorkAttentionCursorAdvance,
) -> Result<ReadCursor, WorkRepositoryError> {
    query_as(
        "SELECT s.last_event_seq, w.seen_through_event_seq, w.receipt_revision, w.seen_updated_at
         FROM works w JOIN work_event_sequences s
           ON s.owner_id = w.owner_id AND s.work_id = w.work_id
         WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1",
    )
    .bind(advance.owner_id.as_str())
    .bind(advance.work_id.as_str())
    .fetch_optional(connection)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work read cursor", source))?
    .ok_or(WorkRepositoryError::NotFound)
}

fn receipt(
    advance: &WorkAttentionCursorAdvance,
    cursor: ReadCursor,
) -> Result<WorkAttentionReceipt, WorkRepositoryError> {
    let through_event_seq = WorkEventSeq::new(cursor.seen_through_event_seq)
        .map_err(|source| WorkRepositoryError::corrupt("Work attention receipt", source))?;
    let input = super::repository::canonical_json(
        "Work attention receipt",
        &ReceiptHashInput {
            schema_version: 1,
            owner_id: &advance.owner_id,
            work_id: &advance.work_id,
            kind: "seen",
            through_event_seq,
        },
    )?;
    let receipt_hash =
        WorkContentHash::parse(super::repository::content_hash(&input)).map_err(|message| {
            WorkRepositoryError::corrupt(
                "Work attention receipt hash",
                std::io::Error::other(message),
            )
        })?;
    Ok(WorkAttentionReceipt {
        revision: WorkAttentionReceiptRevision::new(cursor.receipt_revision)
            .map_err(|source| WorkRepositoryError::corrupt("Work attention receipt", source))?,
        through_event_seq,
        receipt_hash,
        updated_at: cursor.seen_updated_at.and_utc(),
    })
}
