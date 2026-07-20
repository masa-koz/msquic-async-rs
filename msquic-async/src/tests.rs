use super::{
    Connection, ConnectionError, ConnectionStartError, ListenError, Listener, StreamRecvBuffer,
};

#[cfg(feature = "msquic-2-5")]
use msquic_v2_5 as msquic;
#[cfg(feature = "msquic-seera")]
use seera_msquic as msquic;

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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
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
    // let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
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
    let _ = conn
        .start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await;

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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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

        let _a = conn.accept_inbound_stream().await.unwrap();
        let _b = conn.accept_inbound_stream().await.unwrap();
        let _c = conn.accept_inbound_stream().await.unwrap();

        let _d = conn.accept_inbound_uni_stream().await.unwrap();
        let _e = conn.accept_inbound_uni_stream().await.unwrap();
        let _f = conn.accept_inbound_uni_stream().await.unwrap();

        server_rx.recv().await.unwrap();
        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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

        server_rx.recv().await.unwrap();
        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let res = poll_fn(|cx| stream.poll_finish_write(cx)).await;
        assert!(res.is_ok());
        client_tx.send(()).await.unwrap();
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
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
        server_rx.recv().await.unwrap();
        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let res = poll_fn(|cx| stream.poll_abort_write(cx, 0)).await;
        assert!(res.is_ok());
        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", false)).await;
        // assert_eq!(res, Err(WriteError::Finished));
        assert!(res.is_err());
        client_tx.send(()).await.unwrap();
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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
        if let Some(chunk) = res.unwrap() {
            assert!(chunk.is_empty() && chunk.fin());
        }
        server_rx.recv().await.unwrap();

        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
        let mut stream = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await
            .unwrap();

        let _ = poll_fn(|cx| stream.poll_write(cx, b"hello world", false))
            .await
            .unwrap();

        client_rx.recv().await.expect("recv");

        poll_fn(|cx| stream.poll_finish_write(cx)).await.unwrap();
        client_tx.send(()).await.unwrap();
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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .unwrap();
    set.spawn(async move {
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

/// Test for ['Connection::start()']
#[test(tokio::test)]
async fn test_get_local_addr() {
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
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
        let res = conn.get_local_addr();
        assert!(res.is_ok());
        let addr = res.unwrap();
        let addr1: SocketAddr = "127.0.0.1:0".parse().unwrap();
        assert_eq!(addr.ip(), addr1.ip());
        server_rx.recv().await.unwrap();
        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    let res = conn.get_local_addr();
    assert!(res.is_err());
    let res = conn
        .start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await;
    assert!(res.is_ok());
    set.spawn(async move {
        let res = conn.get_local_addr();
        assert!(res.is_ok());
        let addr = res.unwrap();
        let addr1: SocketAddr = "127.0.0.1:0".parse().unwrap();
        assert_eq!(addr.ip(), addr1.ip());
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

/// Test for ['Connection::start()']
#[test(tokio::test)]
async fn test_get_remote_addr() {
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
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
        let res = conn.get_remote_addr();
        assert!(res.is_ok());
        let addr = res.unwrap();
        let addr1: SocketAddr = "127.0.0.1:0".parse().unwrap();
        assert_eq!(addr.ip(), addr1.ip());
        server_rx.recv().await.unwrap();
        server_tx.send(()).await.unwrap();
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    let res = conn.get_remote_addr();
    assert!(res.is_err());
    let res = conn
        .start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await;
    assert!(res.is_ok());
    set.spawn(async move {
        let res = conn.get_remote_addr();
        assert!(res.is_ok());
        let addr = res.unwrap();
        let addr1: SocketAddr = "127.0.0.1:0".parse().unwrap();
        assert_eq!(addr.ip(), addr1.ip());
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
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
        let res = timeout(
            std::time::Duration::from_secs(10),
            poll_fn(|cx| conn.poll_receive_datagram(cx)),
        )
        .await;
        if res.is_err() {
            panic!("no datagram received");
        }
        let res = res.unwrap();
        assert_eq!(res.ok(), Some(Bytes::from("hello world")));
        server_rx.recv().await.unwrap();
        server_tx.send(()).await.expect("send");

        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    let res = conn
        .start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await;
    assert!(res.is_ok());
    set.spawn(async move {
        for _ in 0..3 {
            let res = poll_fn(|cx| conn.poll_send_datagram(cx, &Bytes::from("hello world"))).await;
            assert!(res.is_ok());
        }
        client_tx.send(()).await.unwrap();
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

#[test(tokio::test)]
async fn recv_datagram_after_peer_shutdown() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
        server_tx.send(()).await.expect("send");
        let conn = res.expect("accept");
        let res = timeout(
            std::time::Duration::from_secs(10),
            poll_fn(|cx| conn.poll_receive_datagram(cx)),
        )
        .await;
        match res {
            Err(_) => panic!("timeout waiting for datagram"),
            Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("unexpected datagram received after shutdown"),
        }

        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    let res = conn
        .start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await;
    assert!(res.is_ok());
    client_rx.recv().await.expect("recv");
    conn.shutdown(0).unwrap();

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

#[test(tokio::test)]
async fn recv_datagram_after_local_shutdown() {
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();

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
        // An accepted connection starts in `Connecting`, so wait for the
        // handshake to complete before shutting it down -- `shutdown()` rejects
        // a connection that has not started yet.
        poll_fn(|cx| conn.poll_wait_start(cx))
            .await
            .expect("wait start");
        conn.shutdown(0).unwrap();
        let res = timeout(
            std::time::Duration::from_secs(10),
            poll_fn(|cx| conn.poll_receive_datagram(cx)),
        )
        .await;
        match res {
            Err(_) => panic!("timeout waiting for datagram"),
            Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("unexpected datagram received after shutdown"),
        }
        server_tx.send(()).await.expect("send");

        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();
    let _res = conn
        .start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await;
    client_rx.recv().await.expect("recv");

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

/// Test for ['Connection::poll_event()'] - events are queued correctly
#[cfg(feature = "msquic-seera")]
#[test(tokio::test)]
async fn test_poll_event_queuing() {
    use crate::EventError;

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
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

    // Server side - accept connection
    set.spawn(async move {
        let _conn = listener.accept().await.unwrap();
        // Keep server alive for a bit
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok::<_, anyhow::Error>(())
    });

    // Client side - connect and poll events
    let client_config = new_client_config(
        &registration,
        &msquic::Settings::new().set_IdleTimeoutMs(10000),
    )
    .unwrap();
    let conn = Connection::new(&registration).unwrap();

    set.spawn(async move {
        // Try polling event before connection starts - should return ConnectionNotStarted
        let result = poll_fn(|cx| conn.poll_event(cx)).await;
        match result {
            Err(EventError::ConnectionNotStarted) => {}
            _ => panic!("Expected ConnectionNotStarted error before connection start"),
        }

        // Start the connection
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
        .unwrap();

        // After connection, poll_event should either return events or be pending
        // We don't expect specific events in this basic test, just verify it doesn't panic
        // and handles the states correctly
        let result = timeout(
            Duration::from_millis(100),
            poll_fn(|cx| conn.poll_event(cx)),
        )
        .await;
        // Either timeout (Pending) or Ok with an event is acceptable
        match result {
            Err(_) => {}         // Timeout is fine - no events queued
            Ok(Ok(_event)) => {} // Got an event - also fine
            Ok(Err(e)) => panic!("Unexpected error: {:?}", e),
        }

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

/// Test for ['Connection::poll_event()'] - wakers are notified when events arrive
#[cfg(feature = "msquic-seera")]
#[test(tokio::test)]
async fn test_poll_event_waker_notification() {
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
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

    // Server side
    set.spawn(async move {
        let _conn = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok::<_, anyhow::Error>(())
    });

    // Client side
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

        // Wait a bit for connection to stabilize
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Add a path on client side to generate path events
        let local_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let _ = conn.add_path(local_addr, server_addr);

        // Try to poll for events with timeout - this tests that the waker works
        // If there are no events, this will timeout (which is fine)
        // If there are events, we successfully receive them (also fine)
        let result = timeout(
            Duration::from_millis(200),
            poll_fn(|cx| conn.poll_event(cx)),
        )
        .await;

        match result {
            Err(_) => {}         // Timeout - no events, which is acceptable
            Ok(Ok(_event)) => {} // Got an event - waker worked correctly
            Ok(Err(e)) => panic!("Unexpected error: {:?}", e),
        }

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

/// Test for ['Connection::poll_event()'] - returns ConnectionLost on shutdown completion
#[cfg(feature = "msquic-seera")]
#[test(tokio::test)]
async fn test_poll_event_connection_lost_on_shutdown() {
    use crate::EventError;

    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
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

    // Server side
    set.spawn(async move {
        let _conn = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok::<_, anyhow::Error>(())
    });

    // Client side
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

        // Initiate shutdown
        poll_fn(|cx| conn.poll_shutdown(cx, 0)).await.unwrap();

        // Wait for shutdown to complete
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Now poll_event should return ConnectionLost error
        let result = poll_fn(|cx| conn.poll_event(cx)).await;
        match result {
            Err(EventError::ConnectionLost(_)) => {
                // This is what we expect after shutdown
            }
            other => panic!(
                "Expected ConnectionLost error after shutdown, got: {:?}",
                other
            ),
        }

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

const WAIT_IDLE_BUSY: Duration = Duration::from_millis(200);
const WAIT_IDLE_DRAIN: Duration = Duration::from_secs(5);

fn test_settings() -> msquic::Settings {
    msquic::Settings::new()
        .set_IdleTimeoutMs(10000)
        .set_PeerBidiStreamCount(1)
        .set_PeerUnidiStreamCount(1)
}

/// Start a listener on an ephemeral loopback port and return it with its address.
fn started_server(registration: &crate::Registration) -> (Listener, SocketAddr) {
    let listener = new_server(registration, &test_settings()).unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::BufferRef::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");
    (listener, server_addr)
}

async fn assert_busy(registration: &crate::Registration) {
    assert!(
        timeout(WAIT_IDLE_BUSY, registration.wait_idle())
            .await
            .is_err(),
        "wait_idle() resolved while a tracked handle was still alive"
    );
}

async fn assert_drains(registration: &crate::Registration) {
    timeout(WAIT_IDLE_DRAIN, registration.wait_idle())
        .await
        .expect("wait_idle() did not resolve after all tracked handles were dropped");
}

/// A registration with nothing derived from it is idle immediately.
#[test(tokio::test)]
async fn test_registration_wait_idle_when_empty() {
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    assert_drains(&registration).await;
}

/// A live `Listener` keeps `wait_idle()` pending; dropping it drains.
#[test(tokio::test)]
async fn test_registration_wait_idle_tracks_listener() {
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let (listener, _server_addr) = started_server(&registration);

    assert_busy(&registration).await;

    listener.stop().await.unwrap();
    // `stop()` completing is not enough: the handle is still open.
    assert_busy(&registration).await;

    drop(listener);
    assert_drains(&registration).await;
}

/// A `Connection` that was never started is still tracked.
#[test(tokio::test)]
async fn test_registration_wait_idle_tracks_unstarted_connection() {
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let conn = Connection::new(&registration).unwrap();

    assert_busy(&registration).await;

    drop(conn);
    assert_drains(&registration).await;
}

/// The full loop: connect, shut down, drop everything, and confirm the
/// registration drains. A timeout here is the `RegistrationClose` hang that
/// `wait_idle()` exists to prevent.
#[test(tokio::test)]
async fn test_registration_wait_idle_after_connection_closed() {
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let (listener, server_addr) = started_server(&registration);
    let client_config = new_client_config(&registration, &test_settings()).unwrap();

    let conn = Connection::new(&registration).unwrap();
    let (server_conn, start_res) = tokio::join!(listener.accept(), async {
        conn.start(
            &client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
    });
    let server_conn = server_conn.expect("accept");
    start_res.expect("client start");

    assert_busy(&registration).await;

    registration.shutdown();
    drop(server_conn);
    drop(conn);
    drop(listener);

    assert_drains(&registration).await;

    // Configurations are untracked, so they are dropped after wait_idle().
    drop(client_config);
    drop(registration);
}

/// The guard lives in the shared `ConnectionInstance`, so every clone must go
/// before the registration drains. This is the case that `h3-msquic-async`
/// hits, since it clones the connection into several boxed futures.
#[test(tokio::test)]
async fn test_registration_wait_idle_tracks_connection_clones() {
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let conn = Connection::new(&registration).unwrap();
    let clone = conn.clone();

    drop(conn);
    assert_busy(&registration).await;

    drop(clone);
    assert_drains(&registration).await;
}

/// An inbound connection that was accepted by MsQuic but never popped via
/// `accept()` is queued inside the listener, and must still be tracked.
#[test(tokio::test)]
async fn test_registration_wait_idle_tracks_unaccepted_connection() {
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let (listener, server_addr) = started_server(&registration);
    let client_config = new_client_config(&registration, &test_settings()).unwrap();

    let conn = Connection::new(&registration).unwrap();
    conn.start(
        &client_config,
        &format!("{}", server_addr.ip()),
        server_addr.port(),
    )
    .await
    .expect("client start");

    // Deliberately never call `listener.accept()`.
    registration.shutdown();
    drop(conn);
    assert_busy(&registration).await;

    // Dropping the listener closes the queued inbound connection too.
    drop(listener);
    assert_drains(&registration).await;

    drop(client_config);
}

/// Connect a client to `listener` and return both ends.
async fn connected_pair(
    registration: &crate::Registration,
    listener: &Listener,
    server_addr: SocketAddr,
    client_config: &msquic::Configuration,
) -> (Connection, Connection) {
    let conn = Connection::new(registration).unwrap();
    let (server_conn, start_res) = tokio::join!(listener.accept(), async {
        conn.start(
            client_config,
            &format!("{}", server_addr.ip()),
            server_addr.port(),
        )
        .await
    });
    start_res.expect("client start");
    (conn, server_conn.expect("accept"))
}

/// A locally opened stream that outlives its connection keeps `wait_idle()`
/// pending.
///
/// Streams hold no native registration rundown reference -- the connection
/// releases its own as soon as `ConnectionClose` runs -- but `StreamClose`
/// queues an operation onto the connection's worker, and the worker pool is
/// freed by `RegistrationClose`. Draining before the stream is closed would
/// therefore permit a use-after-free.
#[test(tokio::test)]
async fn test_registration_wait_idle_tracks_stream_outliving_connection() {
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let (listener, server_addr) = started_server(&registration);
    let client_config = new_client_config(&registration, &test_settings()).unwrap();
    let (conn, server_conn) =
        connected_pair(&registration, &listener, server_addr, &client_config).await;

    let stream = conn
        .open_outbound_stream(crate::StreamType::Bidirectional, false)
        .await
        .expect("open outbound stream");

    registration.shutdown();
    drop(server_conn);
    drop(conn);
    drop(listener);

    assert_busy(&registration).await;

    drop(stream);
    assert_drains(&registration).await;

    drop(client_config);
}

/// The same guarantee for a peer-initiated stream, which is constructed in the
/// connection callback rather than by the application.
#[test(tokio::test)]
async fn test_registration_wait_idle_tracks_inbound_stream() {
    let registration = crate::Registration::new(&msquic::RegistrationConfig::default()).unwrap();
    let (listener, server_addr) = started_server(&registration);
    let client_config = new_client_config(&registration, &test_settings()).unwrap();
    let (conn, server_conn) =
        connected_pair(&registration, &listener, server_addr, &client_config).await;

    let mut client_stream = conn
        .open_outbound_stream(crate::StreamType::Bidirectional, false)
        .await
        .expect("open outbound stream");
    let _ = poll_fn(|cx| client_stream.poll_write(cx, b"hello", false))
        .await
        .expect("write");

    let server_stream = server_conn
        .accept_inbound_stream()
        .await
        .expect("accept inbound stream");

    registration.shutdown();
    drop(conn);
    drop(server_conn);
    drop(listener);
    drop(client_stream);

    assert_busy(&registration).await;

    drop(server_stream);
    assert_drains(&registration).await;

    drop(client_config);
}

#[cfg(not(windows))]
fn new_server(registration: &crate::Registration, settings: &msquic::Settings) -> Result<Listener> {
    use std::io::Write;
    use tempfile::NamedTempFile;

    let alpn = [msquic::BufferRef::from("test")];
    let configuration = registration
        .open_configuration(&alpn, Some(settings))
        .unwrap();

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

#[cfg(windows)]
fn new_server(registration: &crate::Registration, settings: &msquic::Settings) -> Result<Listener> {
    use schannel::cert_context::{CertContext, KeySpec};
    use schannel::cert_store::{CertAdd, Memory};
    use schannel::crypt_prov::{AcquireOptions, ProviderType};
    use schannel::RawPointer;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let alpn = [msquic::BufferRef::from("test")];
    let configuration = registration
        .open_configuration(&alpn, Some(settings))
        .unwrap();

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
    registration: &crate::Registration,
    settings: &msquic::Settings,
) -> Result<msquic::Configuration> {
    let alpn = [msquic::BufferRef::from("test")];
    let configuration = registration
        .open_configuration(&alpn, Some(settings))
        .unwrap();
    let cred_config = msquic::CredentialConfig::new_client()
        .set_credential_flags(msquic::CredentialFlags::NO_CERTIFICATE_VALIDATION);
    configuration.load_credential(&cred_config).unwrap();
    Ok(configuration)
}
