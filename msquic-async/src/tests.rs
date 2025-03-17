use super::{
    Connection, ConnectionError, ConnectionStartError, ListenError, Listener, StreamRecvBuffer,
};

use std::future::poll_fn;
use std::mem;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;

use bytes::{Buf, Bytes};

use test_log::test;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;

/// Test for ['Connection::start()']
#[test(tokio::test)]
async fn test_connection_start() {
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let listener = new_server(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");
    let mut set = JoinSet::new();

    set.spawn(async move {
        let _conn = listener.accept().await.unwrap();
        server_rx.recv().await.unwrap();

        listener.stop().await.unwrap();
        server_tx.send(()).await.unwrap();
        drop(listener);
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    let conn1 = Connection::new(&registration).unwrap();
    set.spawn(async move {
        let res = conn
            .start(
                &client_config,
                &format!("{}", server_addr.ip()),
                server_addr.port(),
            )
            .await;
        assert!(res.is_ok());
        client_tx.send(()).await.unwrap();

        client_rx.recv().await.unwrap();

        match conn1
            .start(
                &client_config,
                &format!("{}", server_addr.ip()),
                server_addr.port(),
            )
            .await
        {
            Err(ConnectionStartError::ConnectionLost(ConnectionError::ShutdownByTransport(
                _status,
                _error_code,
            ))) => {}
            _ => {
                panic!("ConnectionStartError::ConnectionLost(ConnectionError::ShutdownByTransport)")
            }
        }
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for ['Connection::shutdown()']
#[test(tokio::test)]
async fn test_connection_poll_shutdown() {
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let listener = new_server(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let conn = listener.accept().await.unwrap();
        server_rx.recv().await.unwrap();
        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, false)
            .await;
        // assert_eq!(
        //     res.err(),
        //     Some(StreamStartError::ConnectionLost(
        //         ConnectionError::ShutdownByPeer(1)
        //     ))
        // );
        assert!(res.is_err());
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();
        // conn.shutdown(1).unwrap();
        let res = poll_fn(|cx| conn.poll_shutdown(cx, 1)).await;
        assert!(res.is_ok());

        client_tx.send(()).await.expect("send");
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for ['Listener::accept()']
#[test(tokio::test)]
async fn test_listener_accept() {
    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let listener = new_server(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");
    let mut set = JoinSet::new();

    set.spawn(async move {
        let res = listener.accept().await;
        assert!(res.is_ok());

        listener.stop().await.unwrap();
        let res = listener.stop().await;
        assert!(res.is_ok());
        match listener.accept().await {
            Err(ListenError::Finished) => {}
            _ => {
                panic!("ListenError::Finished");
            }
        }
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        let _ = conn
            .start(
                &client_config,
                &format!("{}", server_addr.ip()),
                server_addr.port(),
            )
            .await;
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for ['Connection::open_outbound_stream()']
#[test(tokio::test)]
async fn test_open_outbound_stream() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerBidiStreamCount(1)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let mut buf = [0; 1024];

        let conn = listener.accept().await.unwrap();

        let mut stream = conn.accept_inbound_stream().await.unwrap();
        let _ = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await.unwrap();

        let mut stream = conn.accept_inbound_uni_stream().await.unwrap();
        let _ = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await.unwrap();

        server_tx.send(()).await.expect("send");
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();

        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Bidirectional, false),
        )
        .await;
        assert!(res.is_ok());

        let mut stream = res.expect("timeout").expect("open_outbound_stream");
        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Unidirectional, false),
        )
        .await;
        assert!(res.is_ok());

        let mut stream = res.expect("timeout").expect("open_outbound_stream");
        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        client_rx.recv().await.expect("recv");
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for ['Connection::open_outbound_stream']
#[test(tokio::test)]
async fn test_open_outbound_stream_exceed_limit() -> Result<(), anyhow::Error> {
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerBidiStreamCount(1)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let mut buf = [0; 1024];

        let conn = listener.accept().await.unwrap();

        let mut stream = conn.accept_inbound_stream().await.unwrap();
        let _ = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await.unwrap();

        let mut stream1 = conn.accept_inbound_uni_stream().await.unwrap();
        let _ = poll_fn(|cx| stream1.poll_read(cx, &mut buf)).await.unwrap();

        server_rx.recv().await.expect("recv");

        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();

        // Test for Bidirectional
        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Bidirectional, false),
        )
        .await;
        assert!(res.is_ok());

        let mut stream = res.expect("timeout").expect("open_outbound_stream");
        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false)).await?;

        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Bidirectional, false),
        )
        .await;
        assert!(res.is_err());

        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, true)
            .await;
        // assert_eq!(res.err(), Some(StreamStartError::LimitReached));
        assert!(res.is_err());

        // Test for Unidirectional
        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Unidirectional, false),
        )
        .await;
        assert!(res.is_ok());

        let mut stream = res.expect("timeout").expect("open_outbound_stream");
        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false)).await?;

        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Unidirectional, false),
        )
        .await;
        assert!(res.is_err());

        let res = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, true)
            .await;
        // assert_eq!(res.err(), Some(StreamStartError::LimitReached));
        assert!(res.is_err());

        client_tx.send(()).await.expect("send");

        Ok::<_, anyhow::Error>(())
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });

    Ok(())
}

/// Test for ['Connection::open_outbound_stream']
#[test(tokio::test)]
async fn test_open_outbound_stream_exceed_limit_and_accepted() {
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerBidiStreamCount(1)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let conn = listener.accept().await.unwrap();

        let _ = conn.accept_inbound_stream().await.unwrap();
        let _ = conn.accept_inbound_stream().await.unwrap();
        let _ = conn.accept_inbound_stream().await.unwrap();

        let _ = conn.accept_inbound_uni_stream().await.unwrap();
        let _ = conn.accept_inbound_uni_stream().await.unwrap();
        let _ = conn.accept_inbound_uni_stream().await.unwrap();

        server_rx.recv().await.unwrap();
        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();

        // Test for Bidirectional
        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Bidirectional, false),
        )
        .await;
        assert!(res.is_ok());

        let mut stream = res.expect("timeout").expect("open_outbound_stream");
        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Bidirectional, false),
        )
        .await;
        assert!(res.is_err());

        // Drop a stream to create capacity.
        mem::drop(stream);

        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Bidirectional, false),
        )
        .await;
        assert!(res.is_ok());

        let mut stream = res.expect("timeout").expect("open_outbound_stream");
        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        // Test for Unidirectional
        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Unidirectional, false),
        )
        .await;
        assert!(res.is_ok());

        let mut stream = res.expect("timeout").expect("open_outbound_stream");
        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Unidirectional, false),
        )
        .await;
        assert!(res.is_err());

        // Drop a stream to create capacity.
        mem::drop(stream);

        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Unidirectional, false),
        )
        .await;
        assert!(res.is_ok());

        let mut stream = res.expect("timeout").expect("open_outbound_stream");
        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        client_tx.send(()).await.unwrap();
        client_rx.recv().await.unwrap();
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for ['Stream::poll_write']
#[test(tokio::test)]
async fn test_poll_write() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let mut buf = [0; 1024];

        let conn = listener.accept().await.unwrap();
        let mut stream = conn.accept_inbound_uni_stream().await.unwrap();
        let _ = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();
        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", false)).await;
        // assert_eq!(res, Ok(11));
        assert!(res.is_ok());
        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        // assert_eq!(res, Ok(11));
        assert!(res.is_ok());
        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        // assert_eq!(res, Err(WriteError::Finished));
        assert!(res.is_err());

        client_rx.recv().await.expect("recv");
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for ['Stream::write_chunk']
#[test(tokio::test)]
async fn test_write_chunk() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let mut buf = [0; 1024];

        let conn = listener.accept().await.unwrap();
        let mut stream = conn.accept_inbound_uni_stream().await.unwrap();
        let _ = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await.unwrap();

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();
        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let chunk = Bytes::from("hello world");
        let res = stream.write_chunk(&chunk, true).await;
        // assert_eq!(res, Ok(11));
        assert!(res.is_ok());
        let res = stream.write_chunk(&chunk, true).await;
        // assert_eq!(res, Err(WriteError::Finished));
        assert!(res.is_err());

        client_rx.recv().await.unwrap();
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for ['Stream::write_chunks']
#[test(tokio::test)]
async fn test_write_chunks() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let mut buf = [0; 1024];

        let conn = listener.accept().await.unwrap();
        let mut stream = conn.accept_inbound_uni_stream().await.unwrap();
        let _ = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await.unwrap();

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();
        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let chunks = [Bytes::from("hello"), Bytes::from(" world")];
        let res = stream.write_chunks(&chunks, true).await;
        // assert_eq!(res, Ok(11));
        assert!(res.is_ok());
        let res = stream.write_chunks(&chunks, true).await;
        // assert_eq!(res, Err(WriteError::Finished));
        assert!(res.is_err());

        client_rx.recv().await.unwrap();
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for ['Stream::poll_finish_write']
#[test(tokio::test)]
async fn test_poll_finish_write() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let mut buf = [0; 1024];

        let conn = listener.accept().await.unwrap();
        let mut stream = conn.accept_inbound_uni_stream().await.unwrap();
        let _ = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();
        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let res = poll_fn(|cx| stream.poll_finish_write(cx)).await;
        assert!(res.is_ok());
        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", false)).await;
        // assert_eq!(res, Err(WriteError::Finished));
        assert!(res.is_err());

        client_rx.recv().await.expect("recv");
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for ['Stream::poll_abort_write']
#[test(tokio::test)]
async fn test_poll_abort_write() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let mut buf = [0; 1024];

        let conn = listener.accept().await.unwrap();
        let mut stream = conn.accept_inbound_uni_stream().await.unwrap();
        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        // assert_eq!(res, Err(ReadError::Reset(0)));
        assert!(res.is_err());

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();
        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let res = poll_fn(|cx| stream.poll_abort_write(cx, 0)).await;
        assert!(res.is_ok());
        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", false)).await;
        // assert_eq!(res, Err(WriteError::Finished));
        assert!(res.is_err());

        client_rx.recv().await.expect("recv");
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for [`Stream::poll_read()`]
#[test(tokio::test)]
async fn test_poll_read() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let conn = listener.accept().await.unwrap();
        let mut stream = conn.accept_inbound_uni_stream().await.unwrap();

        let mut buf = [0; 1024];
        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        // assert_eq!(res, Ok(11));
        assert!(res.is_ok());
        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        // assert_eq!(res, Ok(0));
        assert!(res.is_ok());

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();

        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", true))
            .await
            .unwrap();

        client_rx.recv().await.expect("recv");
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for [`Stream::read_chunk()`]
#[test(tokio::test)]
async fn test_read_chunk() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let conn = listener.accept().await.unwrap();
        let read_stream = conn.accept_inbound_uni_stream().await.unwrap();

        let res = read_stream.read_chunk().await;
        assert!(res.is_ok());
        let chunk = res.unwrap();
        assert!(chunk.is_some());
        let mut chunk = chunk.unwrap();
        assert_eq!(chunk.remaining(), 11);
        let mut dst = [0; 11];
        chunk.copy_to_slice(&mut dst);
        assert_eq!(&dst, b"hello world");

        mem::drop(chunk);

        server_tx.send(()).await.unwrap();

        let res = read_stream.read_chunk().await;
        assert!(res.is_ok());
        let chunk = res.unwrap();
        assert!(chunk.is_some());
        let mut chunk = chunk.unwrap();
        assert_eq!(chunk.remaining(), 11);
        let mut dst = [0; 11];
        chunk.copy_to_slice(&mut dst);
        assert_eq!(&dst, b"hello world");

        let res = read_stream.read_chunk().await;
        assert!(res.is_ok());
        let chunk = res.unwrap();
        assert!(chunk.is_none());

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();

        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        client_rx.recv().await.expect("recv");

        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", true))
            .await
            .unwrap();

        client_rx.recv().await.expect("recv");
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for [`Stream::read_chunk()`]
#[test(tokio::test)]
async fn test_read_chunk_empty_fin() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let conn = listener.accept().await.unwrap();
        let read_stream = conn.accept_inbound_uni_stream().await.unwrap();

        let res = read_stream.read_chunk().await;
        assert!(res.is_ok());
        let chunk = res.unwrap();
        assert!(chunk.is_some());
        let mut chunk = chunk.unwrap();
        assert_eq!(chunk.remaining(), 11);
        let mut dst = [0; 11];
        chunk.copy_to_slice(&mut dst);
        assert_eq!(&dst, b"hello world");

        mem::drop(chunk);

        server_tx.send(()).await.unwrap();

        let res = read_stream.read_chunk().await;
        assert!(res.is_ok());
        let chunk = res.unwrap();
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert!(chunk.is_empty() && chunk.fin());

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();

        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        client_rx.recv().await.expect("recv");

        poll_fn(|cx| stream.poll_finish_write(cx)).await.unwrap();

        client_rx.recv().await.expect("recv");
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for [`Stream::read_chunk()`]
#[test(tokio::test)]
async fn test_read_chunk_multi_recv() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1)
            .set_StreamMultiReceiveEnabled(),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let conn = listener.accept().await.unwrap();
        let read_stream = conn.accept_inbound_uni_stream().await.unwrap();

        let res = read_stream.read_chunk().await;
        assert!(res.is_ok());
        let chunk = res.unwrap();
        assert!(chunk.is_some());
        let mut chunk = chunk.unwrap();

        server_tx.send(()).await.unwrap();

        let res = read_stream.read_chunk().await;
        assert!(res.is_ok());
        let chunk1 = res.unwrap();
        assert!(chunk1.is_some());
        let mut chunk1 = chunk1.unwrap();

        assert_eq!(chunk.remaining(), 11);
        assert_eq!(chunk1.remaining(), 11);
        let mut dst = [0; 11];
        chunk.copy_to_slice(&mut dst);
        assert_eq!(&dst, b"hello world");
        chunk1.copy_to_slice(&mut dst);
        assert_eq!(&dst, b"hello world");

        let res = read_stream.read_chunk().await;
        assert!(res.is_ok());
        let chunk = res.unwrap();
        assert!(chunk.is_none());

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();

        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        client_rx.recv().await.expect("recv");

        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", true))
            .await
            .unwrap();

        client_rx.recv().await.expect("recv");
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

/// Test for [`Stream::poll_abort_read()`]
#[test(tokio::test)]
async fn test_poll_abort_read() {
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerUnidiStreamCount(1),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let conn = listener.accept().await.unwrap();
        let mut stream = conn.accept_inbound_uni_stream().await.unwrap();

        let res = poll_fn(|cx| stream.poll_abort_read(cx, 0)).await;
        assert!(res.is_ok());

        tokio::time::sleep(Duration::from_millis(100)).await;
        server_tx.send(()).await.unwrap();

        server_rx.recv().await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();

        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();
        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        client_rx.recv().await.unwrap();

        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        // assert_eq!(res, Err(WriteError::Stopped(0)));
        assert!(res.is_err());

        client_tx.send(()).await.unwrap();
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

#[test]
fn test_stream_recv_buffers() {
    let buffers = vec![
        msquic::BufferRef::from("hello "),
        msquic::BufferRef::from("world"),
        msquic::BufferRef::from("!"),
    ];
    let mut buffer = StreamRecvBuffer::new(0, &buffers, false);
    assert_eq!(buffer.remaining(), 12);
    assert!(!buffer.fin());
    assert_eq!(buffer.get_bytes_upto_size(10), Some(&b"hello "[..]));
    assert_eq!(buffer.remaining(), 6);
    assert_eq!(buffer.get_bytes_upto_size(10), Some(&b"world"[..]));
    assert_eq!(buffer.remaining(), 1);
    assert_eq!(buffer.get_bytes_upto_size(10), Some(&b"!"[..]));
    assert_eq!(buffer.remaining(), 0);
    assert_eq!(buffer.get_bytes_upto_size(10), None);

    let mut buffer = StreamRecvBuffer::new(0, &buffers, true);
    assert!(buffer.fin());
    assert_eq!(buffer.get_bytes_upto_size(3), Some(&b"hel"[..]));
    assert_eq!(buffer.remaining(), 9);
    assert_eq!(buffer.get_bytes_upto_size(10), Some(&b"lo "[..]));
    assert_eq!(buffer.remaining(), 6);
    assert_eq!(buffer.get_bytes_upto_size(3), Some(&b"wor"[..]));
    assert_eq!(buffer.remaining(), 3);
    assert_eq!(buffer.get_bytes_upto_size(1), Some(&b"l"[..]));
    assert_eq!(buffer.remaining(), 2);
    assert_eq!(buffer.get_bytes_upto_size(10), Some(&b"d"[..]));
    assert_eq!(buffer.remaining(), 1);
    assert_eq!(buffer.get_bytes_upto_size(10), Some(&b"!"[..]));
    assert_eq!(buffer.remaining(), 0);
    assert_eq!(buffer.get_bytes_upto_size(10), None);

    let mut buffer = StreamRecvBuffer::new(0, &buffers, false);
    assert_eq!(buffer.chunk(), b"hello ");
    buffer.advance(3);
    assert_eq!(buffer.remaining(), 9);
    assert_eq!(buffer.chunk(), b"lo ");
    buffer.advance(3);
    assert_eq!(buffer.remaining(), 6);
    assert_eq!(buffer.chunk(), b"world");
    buffer.advance(6);
    assert_eq!(buffer.remaining(), 0);
    assert_eq!(buffer.chunk(), b"");

    let buffer = StreamRecvBuffer::new(0, &buffers, false);
    let mut dst = [std::io::IoSlice::new(&[]); 3];
    assert_eq!(buffer.chunks_vectored(&mut dst), 3);
    assert_eq!(dst[0].get(..), Some(&b"hello "[..]));
    assert_eq!(dst[1].get(..), Some(&b"world"[..]));
    assert_eq!(dst[2].get(..), Some(&b"!"[..]));
}

#[test(tokio::test)]
async fn datagram_validation() {
    //let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

    let listener = new_server(
        &registration,
        &msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_DatagramReceiveEnabled(),
    )
    .unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let res = listener.accept().await;
        assert!(res.is_ok());
        let conn = res.expect("accept");
        let res = poll_fn(|cx| conn.poll_receive_datagram(cx)).await;
        assert_eq!(res.ok(), Some(Bytes::from("hello world")));

        server_tx.send(()).await.expect("send");

        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    set.spawn(async move {
        let res = conn
            .start(
                &client_config,
                &format!("{}", server_addr.ip()),
                server_addr.port(),
            )
            .await;
        assert!(res.is_ok());
        let res = poll_fn(|cx| conn.poll_send_datagram(cx, &Bytes::from("hello world"))).await;
        assert!(res.is_ok());

        client_rx.recv().await.expect("recv");

        Ok::<_, anyhow::Error>(())
    });

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res);
    }
    results.into_iter().for_each(|res| {
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    });
}

#[cfg(any(not(windows), feature = "openssl"))]
fn new_server(
    registration: &msquic::Registration,
    settings: &msquic::Settings,
) -> Result<Listener> {
    use std::io::Write;
    use tempfile::NamedTempFile;

    let alpn = [msquic::BufferRef::from("test")];
    let configuration = msquic::Configuration::new(registration, &alpn, Some(settings)).unwrap();

    let cert = include_bytes!("../examples/cert.pem");
    let key = include_bytes!("../examples/key.pem");

    let mut cert_file = NamedTempFile::new().unwrap();
    cert_file.write_all(cert).unwrap();
    let cert_path = cert_file.into_temp_path();
    let cert_path = cert_path.to_string_lossy().into_owned();

    let mut key_file = NamedTempFile::new().unwrap();
    key_file.write_all(key).unwrap();
    let key_path = key_file.into_temp_path();
    let key_path = key_path.to_string_lossy().into_owned();

    let cred_config = msquic::CredentialConfig::new().set_credential(
        msquic::Credential::CertificateFile(msquic::CertificateFile::new(key_path, cert_path)),
    );

    configuration.load_credential(&cred_config).unwrap();
    let listener = Listener::new(registration, configuration)?;
    Ok(listener)
}

#[cfg(all(windows, not(feature = "openssl")))]
fn new_server(
    registration: &msquic::Registration,
    settings: &msquic::Settings,
) -> Result<Listener> {
    use schannel::cert_context::{CertContext, KeySpec};
    use schannel::cert_store::{CertAdd, Memory};
    use schannel::crypt_prov::{AcquireOptions, ProviderType};
    use schannel::RawPointer;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let alpn = [msquic::BufferRef::from("test")];
    let configuration = msquic::Configuration::new(registration, &alpn, Some(settings)).unwrap();

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        "msquic-async-test-{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );

    let cert = include_str!("../examples/cert.pem");
    let key = include_bytes!("../examples/key.pem");

    let mut store = Memory::new().unwrap().into_store();

    let cert_ctx = CertContext::from_pem(cert).unwrap();

    let mut options = AcquireOptions::new();
    options.container(&name);

    let type_ = ProviderType::rsa_full();

    let mut container = match options.acquire(type_) {
        Ok(container) => container,
        Err(_) => options.new_keyset(true).acquire(type_).unwrap(),
    };
    container.import().import_pkcs8_pem(key).unwrap();

    cert_ctx
        .set_key_prov_info()
        .container(&name)
        .type_(type_)
        .keep_open(true)
        .key_spec(KeySpec::key_exchange())
        .set()
        .unwrap();

    let context = store.add_cert(&cert_ctx, CertAdd::Always).unwrap();

    let cred_config = msquic::CredentialConfig::new().set_credential(
        msquic::Credential::CertificateContext(unsafe { context.as_ptr() }),
    );

    configuration.load_credential(&cred_config).unwrap();
    let listener = Listener::new(registration, configuration)?;
    Ok(listener)
}

fn new_client_config(
    registration: &msquic::Registration,
    settings: &msquic::Settings,
) -> Result<msquic::Configuration> {
    let alpn = [msquic::BufferRef::from("test")];
    let configuration = msquic::Configuration::new(registration, &alpn, Some(settings)).unwrap();
    let cred_config = msquic::CredentialConfig::new_client()
        .set_credential_flags(msquic::CredentialFlags::NO_CERTIFICATE_VALIDATION);
    configuration.load_credential(&cred_config).unwrap();
    Ok(configuration)
}
