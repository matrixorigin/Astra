use sqlx::{Connection, MySql, pool::PoolConnection};

/// A checked-out shared-pool connection that is reusable only after its
/// caller explicitly proves the MySQL protocol is synchronized.
///
/// Dropping a future while SQLx is awaiting a MySQL response can otherwise
/// return a connection with an in-flight exchange to the pool. Normal paths
/// call [`Self::release`] after completing every query; timeout/cancellation
/// paths reach `Drop` and close only that physical connection.
pub struct CancellationSafePoolConnection {
    connection: Option<PoolConnection<MySql>>,
}

impl CancellationSafePoolConnection {
    pub async fn acquire(pool: &sqlx::Pool<MySql>) -> Result<Self, sqlx::Error> {
        let connection = pool.acquire().await?;
        Ok(Self {
            connection: Some(connection),
        })
    }

    pub fn connection_mut(&mut self) -> &mut sqlx::MySqlConnection {
        self.connection
            .as_deref_mut()
            .expect("cancellation-safe connection already released")
    }

    pub async fn begin(&mut self) -> Result<sqlx::Transaction<'_, MySql>, sqlx::Error> {
        self.connection_mut().begin().await
    }

    pub fn release(mut self) {
        drop(self.connection.take());
    }

    /// Remove this checkout from pool reuse without waiting for a graceful
    /// protocol close.
    ///
    /// Use this after a cancelled or timed-out MySQL exchange: the client no
    /// longer knows whether a response is still in flight, so returning the
    /// physical connection to the idle pool would be unsafe. `detach` restores
    /// the pool's logical capacity; immediately dropping the detached
    /// connection closes the socket instead of leaking a pool permit.
    pub(crate) fn discard(mut self) {
        if let Some(connection) = self.connection.take() {
            drop(connection.detach());
        }
    }
}

impl Drop for CancellationSafePoolConnection {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.as_mut() {
            connection.close_on_drop();
        }
    }
}
