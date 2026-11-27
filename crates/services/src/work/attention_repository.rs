use super::attention::{
    WorkAttentionCursorAdvance, WorkAttentionReceipt, WorkAttentionReceiptRevision,
};
use super::events::WorkEventSeq;
use super::repository::{DatabaseWorkRepository, WorkRepositoryError, rollback_transaction};
use super::{WorkContentHash, WorkId, WorkOwnerId};
use serde::Serialize;
use sqlx::{MySql, query, query_as};

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
    let cursor = load_cursor(repository.pool.get(), &advance).await?;
    let through = advance.through_event_seq.get();
    if through > cursor.last_event_seq {
        return Err(WorkRepositoryError::EventCursorAhead {
            through_event_seq: through,
            event_head: cursor.last_event_seq,
        });
    }
    // Already observed cursors never need a transaction or a write lock.
    if cursor.seen_through_event_seq >= through {
        return receipt(&advance, cursor);
    }

    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work attention cursor transaction", source)
    })?;
    let result = async {
        query(
            "UPDATE work_event_sequences
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
        .execute(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("advance Work attention cursor", source)
        })?;
        let cursor = load_cursor(&mut *transaction, &advance).await?;
        if cursor.seen_through_event_seq < through {
            return Err(WorkRepositoryError::corrupt(
                "Work attention receipt",
                std::io::Error::other("receipt revision is exhausted before the requested cursor"),
            ));
        }
        receipt(&advance, cursor)
    }
    .await;
    let receipt = match result {
        Ok(receipt) => receipt,
        Err(error) => {
            return Err(
                rollback_transaction(transaction, "rollback Work attention cursor", error).await,
            );
        }
    };
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work attention cursor", source)
    })?;
    Ok(receipt)
}

async fn load_cursor<'e>(
    executor: impl sqlx::Executor<'e, Database = MySql>,
    advance: &'e WorkAttentionCursorAdvance,
) -> Result<ReadCursor, WorkRepositoryError> {
    query_as(
        "SELECT last_event_seq, seen_through_event_seq, receipt_revision, seen_updated_at
         FROM work_event_sequences WHERE owner_id = ? AND work_id = ? LIMIT 1",
    )
    .bind(advance.owner_id.as_str())
    .bind(advance.work_id.as_str())
    .fetch_optional(executor)
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
