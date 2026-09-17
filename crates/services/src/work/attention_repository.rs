use super::attention::{
    WorkAttentionCursorAdvance, WorkAttentionCursorKind, WorkAttentionReceipt,
    WorkAttentionReceiptRevision,
};
use super::events::WorkEventSeq;
use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{WorkContentHash, WorkId, WorkOwnerId};
use serde::Serialize;
use sqlx::{MySql, Row, Transaction, query};

const RECEIPT_HASH_SCHEMA_VERSION: u16 = 1;

#[derive(Serialize)]
struct ReceiptHashInput<'a> {
    schema_version: u16,
    owner_id: &'a WorkOwnerId,
    work_id: &'a WorkId,
    kind: WorkAttentionCursorKind,
    through_event_seq: WorkEventSeq,
}

pub(super) async fn insert_genesis_receipt(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
) -> Result<(), WorkRepositoryError> {
    query(
        "INSERT INTO work_attention_receipts
         (owner_id, work_id, receipt_revision,
          delivered_through_event_seq, seen_through_event_seq)
         VALUES (?, ?, 1, 0, 0)",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert Work attention receipt",
            super::WorkConflictResource::WorkAttentionReceipt,
            source,
        )
    })?;
    Ok(())
}

pub(super) async fn advance_cursor(
    repository: &DatabaseWorkRepository,
    advance: WorkAttentionCursorAdvance,
) -> Result<WorkAttentionReceipt, WorkRepositoryError> {
    let receipt_hash = receipt_hash(&advance)?;
    let sequence = query(
        "SELECT last_event_seq FROM work_event_sequences
         WHERE owner_id = ? AND work_id = ? LIMIT 1",
    )
    .bind(advance.owner_id.as_str())
    .bind(advance.work_id.as_str())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work event head", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let head = sequence
        .try_get::<i64, _>("last_event_seq")
        .map_err(|source| WorkRepositoryError::corrupt("Work event sequence", source))?;
    if advance.through_event_seq.get() > head {
        return Err(WorkRepositoryError::EventCursorAhead {
            through_event_seq: advance.through_event_seq.get(),
            event_head: head,
        });
    }

    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work attention cursor transaction", source)
    })?;

    let update_sql = match advance.kind {
        WorkAttentionCursorKind::Delivered => {
            "UPDATE work_attention_receipts
             SET delivered_through_event_seq = ?, delivered_receipt_hash = ?,
                 receipt_revision = receipt_revision + 1, updated_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND delivered_through_event_seq < ?
               AND receipt_revision < ?"
        }
        WorkAttentionCursorKind::Seen => {
            "UPDATE work_attention_receipts
             SET seen_through_event_seq = ?, seen_receipt_hash = ?,
                 receipt_revision = receipt_revision + 1, updated_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND seen_through_event_seq < ?
               AND receipt_revision < ?"
        }
    };
    if let Err(source) = query(update_sql)
        .bind(advance.through_event_seq.get())
        .bind(receipt_hash.as_str())
        .bind(advance.owner_id.as_str())
        .bind(advance.work_id.as_str())
        .bind(advance.through_event_seq.get())
        .bind(i64::MAX)
        .execute(&mut *transaction)
        .await
    {
        let error = WorkRepositoryError::persistence("advance Work attention cursor", source);
        return Err(super::repository::rollback_transaction(
            transaction,
            "rollback Work attention cursor",
            error,
        )
        .await);
    }

    let receipt = match load_receipt(&mut transaction, &advance.owner_id, &advance.work_id).await {
        Ok(receipt) => receipt,
        Err(error) => {
            return Err(super::repository::rollback_transaction(
                transaction,
                "rollback unreadable Work attention receipt",
                error,
            )
            .await);
        }
    };
    let actual_cursor = match advance.kind {
        WorkAttentionCursorKind::Delivered => receipt.delivered_through_event_seq,
        WorkAttentionCursorKind::Seen => receipt.seen_through_event_seq,
    };
    if actual_cursor.map(WorkEventSeq::get).unwrap_or(0) < advance.through_event_seq.get() {
        let error = WorkRepositoryError::corrupt(
            "Work attention receipt",
            std::io::Error::other("receipt revision is exhausted before the requested cursor"),
        );
        return Err(super::repository::rollback_transaction(
            transaction,
            "rollback exhausted Work attention cursor",
            error,
        )
        .await);
    }
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work attention cursor", source)
    })?;
    Ok(receipt)
}

fn receipt_hash(
    advance: &WorkAttentionCursorAdvance,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let input = super::repository::canonical_json(
        "Work attention receipt",
        &ReceiptHashInput {
            schema_version: RECEIPT_HASH_SCHEMA_VERSION,
            owner_id: &advance.owner_id,
            work_id: &advance.work_id,
            kind: advance.kind,
            through_event_seq: advance.through_event_seq,
        },
    )?;
    WorkContentHash::parse(super::repository::content_hash(&input)).map_err(|message| {
        WorkRepositoryError::corrupt(
            "Work attention receipt hash",
            std::io::Error::other(message),
        )
    })
}

async fn load_receipt(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
) -> Result<WorkAttentionReceipt, WorkRepositoryError> {
    let row = query(
        "SELECT receipt_revision, delivered_through_event_seq, seen_through_event_seq,
                delivered_receipt_hash, seen_receipt_hash,
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at
         FROM work_attention_receipts
         WHERE owner_id = ? AND work_id = ? LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work attention receipt", source))?
    .ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "Work attention receipt",
            std::io::Error::other("receipt is missing for an existing Work"),
        )
    })?;
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work attention receipt", source))
    };
    let optional_hash = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work attention receipt", source))?
            .map(WorkContentHash::parse)
            .transpose()
            .map_err(|message| {
                WorkRepositoryError::corrupt(
                    "Work attention receipt",
                    std::io::Error::other(message),
                )
            })
    };
    let cursor = |field: &'static str| {
        let value = integer(field)?;
        if value == 0 {
            Ok(None)
        } else {
            WorkEventSeq::new(value)
                .map(Some)
                .map_err(|source| WorkRepositoryError::corrupt("Work attention receipt", source))
        }
    };
    Ok(WorkAttentionReceipt {
        owner_id: owner_id.clone(),
        work_id: work_id.clone(),
        revision: WorkAttentionReceiptRevision::new(integer("receipt_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work attention receipt", source))?,
        delivered_through_event_seq: cursor("delivered_through_event_seq")?,
        seen_through_event_seq: cursor("seen_through_event_seq")?,
        delivered_receipt_hash: optional_hash("delivered_receipt_hash")?,
        seen_receipt_hash: optional_hash("seen_receipt_hash")?,
        created_at: super::repository::decode_timestamp(
            "Work attention receipt",
            "created_at",
            row.try_get("created_at")
                .map_err(|source| WorkRepositoryError::corrupt("Work attention receipt", source))?,
        )?,
        updated_at: super::repository::decode_timestamp(
            "Work attention receipt",
            "updated_at",
            row.try_get("updated_at")
                .map_err(|source| WorkRepositoryError::corrupt("Work attention receipt", source))?,
        )?,
    })
}
