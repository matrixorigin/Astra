use std::any::Any;

use sqlx_core::net::{Socket, WithSocket, connect_tcp};
use tokio::net::{TcpListener, TcpStream};

struct InspectTcpNoDelay;

impl WithSocket for InspectTcpNoDelay {
    type Output = bool;

    async fn with_socket<S: Socket>(self, socket: S) -> Self::Output {
        // Inspect the actual SQLx transport before MySQL authentication or TLS.
        // The Tokio runtime is the one configured by Astra's SQLx dependency.
        let tcp = (&socket as &dyn Any)
            .downcast_ref::<TcpStream>()
            .expect("SQLx must use the configured Tokio TCP transport");
        tcp.nodelay().expect("read TCP_NODELAY")
    }
}

#[tokio::test]
async fn matrixone_sqlx_tcp_connections_disable_nagle() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // Check separate connections: replacement pool connections must receive the
    // same transport setting, without requiring a database or timing thresholds.
    for _ in 0..2 {
        let nodelay = connect_tcp("127.0.0.1", port, InspectTcpNoDelay)
            .await
            .expect("connect to the local listener");
        assert!(
            nodelay,
            "SQLx must disable Nagle before sending MySQL protocol packets"
        );
    }
}
