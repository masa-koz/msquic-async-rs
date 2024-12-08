use super::{
    Connection, ConnectionError, ConnectionStartError, CredentialConfigCertFile, ListenError,
    Listener, ReadError, StreamRecvBuffer, StreamStartError, WriteError,
};

use std::future::poll_fn;
use std::io::Write;
use std::net::SocketAddr;
use std::pin::Pin;
use std::ptr;

use anyhow::Result;

use bytes::{Buf, Bytes};

use tempfile::{NamedTempFile, TempPath};

use test_log::test;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;

#[test(tokio::test)]
async fn connection_validation() -> Result<()> {
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let api =
        msquic::Api::new().map_err(|status| anyhow::anyhow!("Api::new failed: 0x{:x}", status))?;
    let registration = msquic::Registration::new(&api, ptr::null())
        .map_err(|status| anyhow::anyhow!("Registration::new failed: 0x{:x}", status))?;

    let listener = new_server(&registration, &api)?;
    let addr: SocketAddr = "127.0.0.1:0".parse()?;
    listener
        .start(&[msquic::Buffer::from("test")], Some(addr))?;
    let server_addr = listener.local_addr()?;

    let mut set = JoinSet::new();

    set.spawn(async move {
        let res = listener.accept().await;
        assert!(res.is_ok());
        server_rx.recv().await.expect("recv");

        let conn = res?;
        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, false)
            .await;
        assert_eq!(
            res.err(),
            Some(StreamStartError::ConnectionLost(
                ConnectionError::ShutdownByPeer(1)
            ))
        );

        let res = listener.stop().await;
        assert!(res.is_ok());
        if let Err(ListenError::Finished) = listener.accept().await {
            assert!(true, "ListenError::Finished");
        } else {
            assert!(false, "ListenError::Finished");
        }
        server_tx.send(()).await.expect("send");

        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(&registration)?;
    let conn = Connection::new(msquic::Connection::new(&registration), &registration, &api);
    let conn1 = Connection::new(msquic::Connection::new(&registration), &registration, &api);
    set.spawn(async move {
        let res = conn
            .start(
                &client_config,
                &format!("{}", server_addr.ip()),
                server_addr.port(),
            )
            .await;
        assert!(res.is_ok());
        let res = poll_fn(|cx| conn.poll_shutdown(cx, 1)).await;
        assert!(res.is_ok());

        client_tx.send(()).await.expect("send");

        client_rx.recv().await.expect("recv");

        if let Err(ConnectionStartError::ConnectionLost(ConnectionError::ShutdownByTransport(
            _status,
            _error_code,
        ))) = conn1
            .start(
                &client_config,
                &format!("{}", server_addr.ip()),
                server_addr.port(),
            )
            .await
        {
            assert!(true, "ConnectionError::ShutdownByTransport");
        } else {
            assert!(false, "ConnectionError::ShutdownByTransport");
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
    Ok(())
}

#[test(tokio::test)]
async fn stream_validation() {
    let api = msquic::Api::new().unwrap();
    let registration = msquic::Registration::new(&api, ptr::null()).unwrap();
    let listener = new_server(&registration, &api).expect("new_server");
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::Buffer::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);
    set.spawn(async move {
        let conn = listener.accept().await.expect("accept");

        let mut stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");

        server_rx.recv().await.expect("recv");

        let mut buf = [0; 1024];
        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Err(ReadError::Reset(0)));

        let mut stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");
        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Err(ReadError::Reset(0)));

        let mut stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");
        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));
        assert_eq!(&buf[0..11], b"hello world");

        let res = poll_fn(|cx| stream.poll_finish_write(cx)).await;
        assert!(res.is_ok());

        let mut stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Err(ReadError::Reset(0)));

        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        assert_eq!(res, Ok(11));

        let mut stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");
        assert_eq!(stream.id(), Some(16));

        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        if res.is_err() {
            assert_eq!(res, Err(WriteError::Stopped(0)));
        } else {
            assert_eq!(res, Ok(11));
        }

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));
        assert_eq!(&buf[0..11], b"hello world");

        std::mem::drop(stream);

        server_tx.send(()).await.expect("send");

        let stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");
        let (read, write) = stream.split();
        assert!(read.is_some());
        assert!(write.is_some());
        let (mut read, mut write) = (read.unwrap(), write.unwrap());

        let res = tokio::io::AsyncReadExt::read(&mut read, &mut buf).await;
        assert_eq!(res.ok(), Some(11));
        assert_eq!(&buf[0..11], b"hello world");

        let res = tokio::io::AsyncReadExt::read(&mut read, &mut buf).await;
        assert_eq!(res.ok(), Some(0));

        let res = futures::io::AsyncWriteExt::write(&mut write, b"hello world").await;
        assert_eq!(res.ok(), Some(11));

        let res = poll_fn(|cx| write.poll_finish_write(cx)).await;
        assert!(res.is_ok());

        server_rx.recv().await.expect("recv");

        std::mem::drop(read);
        std::mem::drop(write);

        let stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");
        let (read, write) = stream.split();
        assert!(read.is_some());
        assert!(write.is_some());

        let (mut read, mut write) = (read.unwrap(), write.unwrap());

        let res = tokio::io::copy(&mut read, &mut write).await;
        assert_eq!(res.ok(), Some(11));

        let res = poll_fn(|cx| write.poll_finish_write(cx)).await;
        assert!(res.is_ok());

        server_rx.recv().await.expect("recv");

        std::mem::drop(read);
        std::mem::drop(write);

        let stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");
        let (read, write) = stream.split();
        assert!(read.is_some());
        assert!(write.is_some());

        let (mut read, mut write) = (read.unwrap(), write.unwrap());

        let read_task = tokio::task::spawn(async move {
            let res = poll_fn(|cx| read.poll_read(cx, &mut buf)).await;
            assert_eq!(res, Ok(11));
            assert_eq!(&buf[0..11], b"hello world");
        });

        let res = poll_fn(|cx| write.poll_write(cx, b"hello world", true)).await;
        assert_eq!(res, Ok(11));

        let (res,) = tokio::join!(read_task);
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }

        server_rx.recv().await.expect("recv");

        std::mem::drop(write);

        let mut read = conn
            .accept_inbound_uni_stream()
            .await
            .expect("accept_inbound_stream");

        let mut buf = [0; 1024];
        let res = poll_fn(|cx| read.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));
        assert_eq!(&buf[0..11], b"hello world");

        server_tx.send(()).await.expect("send");

        std::mem::drop(read);

        let mut read = conn
            .accept_inbound_uni_stream()
            .await
            .expect("accept_inbound_uni_stream");

        let res = poll_fn(|cx| read.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));
        assert_eq!(&buf[0..11], b"hello world");

        server_tx.send(()).await.expect("send");

        std::mem::drop(read);

        let res = poll_fn(|cx| conn.poll_shutdown(cx, 0)).await;
        assert!(res.is_ok());

        server_tx.send(()).await.expect("send");

        let res = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await;
        assert_eq!(
            res.err(),
            Some(StreamStartError::ConnectionLost(
                ConnectionError::ShutdownByLocal
            ))
        );

        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(&registration).expect("new_client_config");
    let conn = Connection::new(msquic::Connection::new(&registration), &registration, &api);

    set.spawn(async move {
        let mut buf = [0; 1024];

        let res = conn
            .start(
                &client_config,
                &format!("{}", server_addr.ip()),
                server_addr.port(),
            )
            .await;
        assert!(res.is_ok());
        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Bidirectional, false),
        )
        .await;
        assert!(res.is_ok());

        let mut stream = res.expect("timeout").expect("open_outbound_stream");
        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", false)).await;
        assert!(res.is_ok());

        let res = timeout(
            std::time::Duration::from_millis(1000),
            conn.open_outbound_stream(crate::StreamType::Bidirectional, false),
        )
        .await;
        assert!(res.is_err());

        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, true)
            .await;
        assert!(res.is_err());

        std::mem::drop(stream);

        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, false)
            .await;
        assert!(res.is_ok());

        let mut stream = res.expect("open_outbound_stream");
        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        assert_eq!(res, Ok(11));

        client_tx.send(()).await.expect("send");

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(0));

        std::mem::drop(stream);

        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, false)
            .await;
        assert!(res.is_ok());

        let mut stream = res.expect("open_outbound_stream");
        let res = poll_fn(|cx| stream.poll_abort_write(cx, 0)).await;
        assert_eq!(res, Ok(()));

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));
        assert_eq!(&buf[0..11], b"hello world");

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(0));

        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, false)
            .await;
        assert!(res.is_ok());

        let mut stream = res.expect("open_outbound_stream");
        let res = poll_fn(|cx| stream.poll_abort_read(cx, 0)).await;
        assert_eq!(res, Ok(()));
        // not waiting for the server to receive stopping

        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        assert_eq!(res, Ok(11));

        client_rx.recv().await.expect("recv");

        std::mem::drop(stream);

        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, false)
            .await;
        assert!(res.is_ok());

        let stream = res.expect("open_outbound_stream");
        let (read, write) = stream.split();
        assert!(read.is_some());
        assert!(write.is_some());

        let (mut read, mut write) = (read.unwrap(), write.unwrap());

        let res = tokio::io::AsyncWriteExt::write(&mut write, b"hello world").await;
        assert_eq!(res.ok(), Some(11));

        let res = poll_fn(|cx| write.poll_finish_write(cx)).await;
        assert!(res.is_ok());

        let res = futures::io::AsyncReadExt::read(&mut read, &mut buf).await;
        assert_eq!(res.ok(), Some(11));
        assert_eq!(&buf[0..11], b"hello world");

        client_tx.send(()).await.expect("send");

        std::mem::drop(read);
        std::mem::drop(write);

        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, false)
            .await;
        assert!(res.is_ok());

        let mut stream = res.expect("open_outbound_stream");

        // echo
        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        assert_eq!(res, Ok(11));

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));
        assert_eq!(&buf[0..11], b"hello world");

        client_tx.send(()).await.expect("send");

        std::mem::drop(stream);

        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, false)
            .await;
        assert!(res.is_ok());

        let (read, write) = res.expect("open_outbound_stream").split();
        assert!(read.is_some());
        assert!(write.is_some());

        let (mut read, mut write) = (read.unwrap(), write.unwrap());

        // read/write in parallel
        let write_task = tokio::task::spawn(async move {
            let res = poll_fn(|cx| write.poll_write(cx, b"hello world", true)).await;
            assert_eq!(res, Ok(11));
            Ok::<_, anyhow::Error>(())
        });

        let res = poll_fn(|cx| read.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));
        assert_eq!(&buf[0..11], b"hello world");

        let (res,) = tokio::join!(write_task);
        if let Err(err) = res {
            if err.is_panic() {
                std::panic::resume_unwind(err.into_panic());
            }
        }
        client_tx.send(()).await.expect("send");

        std::mem::drop(read);

        let res = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await;
        assert!(res.is_ok());

        let mut stream = res.expect("open_outbound_stream");

        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        assert_eq!(res, Ok(11));

        client_rx.recv().await.expect("recv");

        std::mem::drop(stream);

        let res = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await;
        assert!(res.is_ok());

        let (read, write) = res.expect("open_outbound_stream").split();
        assert!(read.is_none());
        assert!(write.is_some());

        let mut write = write.unwrap();
        let res = poll_fn(|cx| write.poll_write(cx, b"hello world", true)).await;
        assert_eq!(res, Ok(11));

        client_rx.recv().await.expect("recv");

        std::mem::drop(write);

        client_rx.recv().await.expect("recv");

        let res = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await;
        assert_eq!(
            res.err(),
            Some(StreamStartError::ConnectionLost(
                ConnectionError::ShutdownByPeer(0)
            ))
        );

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
async fn stream_recv_buffer_validation() {
    //let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let api = msquic::Api::new().unwrap();
    let registration = msquic::Registration::new(&api, ptr::null()).unwrap();

    let listener = new_server(&registration, &api).expect("new_server");
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::Buffer::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let mut set = JoinSet::new();

    set.spawn(async move {
        let res = listener.accept().await;
        assert!(res.is_ok());

        let conn = res.expect("accept");
        let read_stream = conn
            .accept_inbound_uni_stream()
            .await
            .expect("accept_inbound_stream");

        let res = poll_fn(|cx| read_stream.poll_read_chunk(cx)).await;
        assert!(res.is_ok());
        let chunk = res.expect("poll_read_chunk");
        assert!(chunk.is_some());
        let mut chunk = chunk.unwrap();
        assert_eq!(chunk.remaining(), 11);
        let mut dst = [0; 11];
        chunk.copy_to_slice(&mut dst);
        assert_eq!(&dst, b"hello world");

        std::mem::drop(chunk);

        server_tx.send(()).await.expect("send");

        let res = poll_fn(|cx| read_stream.poll_read_chunk(cx)).await;
        assert!(res.is_ok());
        let chunk = res.expect("poll_read_chunk");
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert_eq!(chunk.remaining(), 11);

        server_tx.send(()).await.expect("send");

        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(&registration).expect("new_client_config");
    let conn = Connection::new(msquic::Connection::new(&registration), &registration, &api);
    set.spawn(async move {
        let res = conn
            .start(
                &client_config,
                &format!("{}", server_addr.ip()),
                server_addr.port(),
            )
            .await;
        assert!(res.is_ok());

        let res = conn
            .open_outbound_stream(crate::StreamType::Unidirectional, false)
            .await;
        assert!(res.is_ok());

        let mut stream = res.expect("open_outbound_stream");
        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", false)).await;
        assert_eq!(res, Ok(11));

        client_rx.recv().await.expect("recv");

        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        assert_eq!(res, Ok(11));

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

#[test]
fn test_stream_recv_buffers() {
    let buffers = vec![
        msquic::Buffer::from("hello "),
        msquic::Buffer::from("world"),
        msquic::Buffer::from("!"),
    ];
    let mut buffer = StreamRecvBuffer::new(0, &buffers, false);
    assert_eq!(buffer.remaining(), 12);
    assert_eq!(buffer.fin(), false);
    assert_eq!(buffer.get_bytes_upto_size(10), Some(&b"hello "[..]));
    assert_eq!(buffer.remaining(), 6);
    assert_eq!(buffer.get_bytes_upto_size(10), Some(&b"world"[..]));
    assert_eq!(buffer.remaining(), 1);
    assert_eq!(buffer.get_bytes_upto_size(10), Some(&b"!"[..]));
    assert_eq!(buffer.remaining(), 0);
    assert_eq!(buffer.get_bytes_upto_size(10), None);

    let mut buffer = StreamRecvBuffer::new(0, &buffers, true);
    assert_eq!(buffer.fin(), true);
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

    let api = msquic::Api::new().unwrap();
    let registration = msquic::Registration::new(&api, ptr::null()).unwrap();

    let listener = new_server(&registration, &api).expect("new_server");
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::Buffer::from("test")], Some(addr))
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

    let client_config = new_client_config(&registration).expect("new_client_config");
    let conn = Connection::new(msquic::Connection::new(&registration), &registration, &api);
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

fn new_server(registration: &msquic::Registration, api: &msquic::Api) -> Result<Listener> {
    let alpn = [msquic::Buffer::from("test")];
    let configuration = msquic::Configuration::new(
        registration,
        &alpn,
        msquic::Settings::new()
            .set_idle_timeout_ms(10000)
            .set_peer_bidi_stream_count(1)
            .set_peer_unidi_stream_count(1)
            .set_datagram_receive_enabled(true)
            .set_stream_multi_receive_enabled(false),
    )
    .unwrap();
    let cred_config = SelfSignedCredentialConfig::new()?;
    configuration
        .load_credential(cred_config.as_cred_config_ref())
        .unwrap();
    let listener = Listener::new(msquic::Listener::new(api), registration, configuration, api);
    Ok(listener)
}

fn new_client_config(registration: &msquic::Registration) -> Result<msquic::Configuration> {
    let alpn = [msquic::Buffer::from("test")];
    let configuration = msquic::Configuration::new(
        registration,
        &alpn,
        msquic::Settings::new()
            .set_idle_timeout_ms(10000)
            .set_peer_bidi_stream_count(1)
            .set_peer_unidi_stream_count(1)
            .set_datagram_receive_enabled(true)
            .set_stream_multi_receive_enabled(false),
    )
    .unwrap();
    let mut cred_config = msquic::CredentialConfig::new_client();
    cred_config.cred_flags |= msquic::CREDENTIAL_FLAG_NO_CERTIFICATE_VALIDATION;
    configuration.load_credential(&cred_config).unwrap();
    Ok(configuration)
}

struct SelfSignedCredentialConfig {
    inner: Pin<Box<CredentialConfigCertFile>>,
    _cert_path: TempPath,
    _key_path: TempPath,
}

impl SelfSignedCredentialConfig {
    fn new() -> Result<Self> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;

        let mut cert_file = NamedTempFile::new()?;
        cert_file.write_all(cert.serialize_pem()?.as_bytes())?;
        let cert_path = cert_file.into_temp_path();

        let mut key_file = NamedTempFile::new()?;
        key_file.write_all(cert.serialize_private_key_pem().as_bytes())?;
        let key_path = key_file.into_temp_path();

        Ok(Self {
            inner: CredentialConfigCertFile::new(&cert_path, &key_path),
            _cert_path: cert_path,
            _key_path: key_path,
        })
    }

    fn as_cred_config_ref(&self) -> &msquic::CredentialConfig {
        &self.inner.as_ref().get_ref().cred_config
    }
}
