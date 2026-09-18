use astra_core::SharedPool;
use sqlx::{MySql, pool::PoolConnection};

/// A checked-out shared-pool connection that is reusable only after its
/// caller explicitly proves the MySQL protocol is synchronized.
///
/// Dropping a future while SQLx is awaiting a MySQL response can otherwise
/// return a connection with an in-flight exchange to the pool. Normal paths
/// call [`Self::release`] after completing every query; timeout/cancellation
/// paths reach `Drop` and close only that physical connection.
pub(crate) struct CancellationSafePoolConnection {
    connection: Option<PoolConnection<MySql>>,
}

impl CancellationSafePoolConnection {
    pub(crate) async fn acquire(pool: &SharedPool) -> Result<Self, sqlx::Error> {
        let connection = pool.get().acquire().await?;
        Ok(Self {
            connection: Some(connection),
        })
    }

    pub(crate) fn connection_mut(&mut self) -> &mut sqlx::MySqlConnection {
        self.connection
            .as_deref_mut()
            .expect("cancellation-safe connection already released")
    }

    pub(crate) fn release(mut self) {
        drop(self.connection.take());
    }
}

impl Drop for CancellationSafePoolConnection {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.as_mut() {
            connection.close_on_drop();
        }
    }
}
