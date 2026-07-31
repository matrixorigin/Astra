use sqlx::Row;

/// Read the database authority clock without depending on whether a backend
/// reports `NOW(6)` as DATETIME or TIMESTAMP. MatrixOne and MySQL expose
/// different wire types for that expression, while the explicit signed epoch
/// value has one stable SQLx representation.
pub(crate) async fn database_now_unix_ms(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
) -> Result<i64, sqlx::Error> {
    sqlx::query("SELECT CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED) AS database_now_unix_ms")
        .fetch_one(&mut **tx)
        .await?
        .try_get("database_now_unix_ms")
}

pub trait RowExt {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn i8_column(&self, column: &str) -> Result<i8, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn optional_i8_column(&self, column: &str) -> Result<Option<i8>, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn optional_i32_column(&self, column: &str) -> Result<Option<i32>, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn f32_column(&self, column: &str) -> Result<f32, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn f64_column(&self, column: &str) -> Result<f64, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn optional_f32_column(&self, column: &str) -> Result<Option<f32>, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn datetime_string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }

    fn optional_datetime_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
        Err(sqlx::Error::ColumnNotFound(column.to_string()))
    }
}

/// Typed row decoder that wraps a [`RowExt`] with a context string for
/// consistent error messages. Replaces the per-module free-function
/// patterns (`xxx_string_column`, `xxx_i64_column`, …).
///
/// # Example
///
/// ```ignore
/// let dec = RowDecoder::new(row, "artifact_retention");
/// let id: String = dec.string("artifact_id")?;
/// let count: i64 = dec.non_negative_i64("manifest_count")?;
/// ```
pub struct RowDecoder<'a, R: RowExt + ?Sized> {
    row: &'a R,
    context: &'static str,
}

impl<'a, R: RowExt + ?Sized> RowDecoder<'a, R> {
    pub fn new(row: &'a R, context: &'static str) -> Self {
        Self { row, context }
    }

    fn err(&self, column: &'static str, error: impl std::fmt::Display) -> String {
        format!("{} decode `{}`: {}", self.context, column, error)
    }

    /// Public error constructor for callers that need custom validation messages
    /// with the same formatting convention.
    pub fn err_msg(&self, column: &'static str, error: impl std::fmt::Display) -> String {
        self.err(column, error)
    }

    pub fn string(&self, column: &'static str) -> Result<String, String> {
        self.row
            .string_column(column)
            .map_err(|e| self.err(column, e))
    }

    pub fn optional_string(&self, column: &'static str) -> Result<Option<String>, String> {
        self.row
            .optional_string_column(column)
            .map_err(|e| self.err(column, e))
    }

    pub fn i64(&self, column: &'static str) -> Result<i64, String> {
        self.row.i64_column(column).map_err(|e| self.err(column, e))
    }

    pub fn optional_i64(&self, column: &'static str) -> Result<Option<i64>, String> {
        self.row
            .optional_i64_column(column)
            .map_err(|e| self.err(column, e))
    }

    pub fn optional_i32(&self, column: &'static str) -> Result<Option<i32>, String> {
        self.row
            .optional_i32_column(column)
            .map_err(|e| self.err(column, e))
    }

    pub fn non_negative_i64(&self, column: &'static str) -> Result<i64, String> {
        let value = self.i64(column)?;
        if value < 0 {
            return Err(self.err(
                column,
                format!("expected non-negative integer, got {value}"),
            ));
        }
        Ok(value)
    }

    pub fn positive_i64(&self, column: &'static str) -> Result<i64, String> {
        let value = self.i64(column)?;
        if value <= 0 {
            return Err(self.err(column, format!("expected positive integer, got {value}")));
        }
        Ok(value)
    }

    pub fn non_empty_string(&self, column: &'static str) -> Result<String, String> {
        let value = self.string(column)?;
        if value.trim().is_empty() {
            return Err(self.err(column, "expected non-empty string"));
        }
        Ok(value)
    }

    /// Read an optional string column and parse as JSON.
    pub fn optional_json(&self, column: &'static str) -> Result<Option<serde_json::Value>, String> {
        let Some(raw) = self.optional_string(column)? else {
            return Ok(None);
        };
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| self.err(column, format!("invalid JSON: {e}")))
    }
}

impl RowExt for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        self.try_get::<String, _>(column)
    }

    fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
        self.try_get::<Option<String>, _>(column)
    }

    fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
        self.try_get::<i64, _>(column)
    }

    fn optional_i64_column(&self, column: &str) -> Result<Option<i64>, sqlx::Error> {
        self.try_get::<Option<i64>, _>(column)
    }

    fn i8_column(&self, column: &str) -> Result<i8, sqlx::Error> {
        self.try_get::<i8, _>(column)
    }

    fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error> {
        self.try_get::<i32, _>(column)
    }

    fn optional_i8_column(&self, column: &str) -> Result<Option<i8>, sqlx::Error> {
        self.try_get::<Option<i8>, _>(column)
    }

    fn optional_i32_column(&self, column: &str) -> Result<Option<i32>, sqlx::Error> {
        self.try_get::<Option<i32>, _>(column)
    }

    fn f32_column(&self, column: &str) -> Result<f32, sqlx::Error> {
        self.try_get::<f32, _>(column)
    }

    fn f64_column(&self, column: &str) -> Result<f64, sqlx::Error> {
        self.try_get::<f64, _>(column)
    }

    fn optional_f32_column(&self, column: &str) -> Result<Option<f32>, sqlx::Error> {
        self.try_get::<Option<f32>, _>(column)
    }

    fn datetime_string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        self.try_get::<chrono::NaiveDateTime, _>(column)
            .map(|dt| dt.to_string())
            .or_else(|_| self.try_get::<String, _>(column))
    }

    fn optional_datetime_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
        self.try_get::<Option<chrono::NaiveDateTime>, _>(column)
            .map(|dt| dt.map(|dt| dt.to_string()))
            .or_else(|_| self.try_get::<Option<String>, _>(column))
    }
}
