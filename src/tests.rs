use super::{
    Connection, ConnectionError, CredentialConfigCertFile, ListenError, Listener, ReadError, StartError,
    WriteError, MSQUIC_API,
};

use std::future::poll_fn;
use std::io::Write;
use std::net::SocketAddr;
use std::pin::Pin;
use std::ptr;

use anyhow::Result;

use tempfile::{NamedTempFile, TempPath};

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;

#[tokio::test]
async fn connection_validation() {
    let (client_tx, mut server_rx) = mpsc::channel::<()>(1);
    let (server_tx, mut client_rx) = mpsc::channel::<()>(1);

    let registration = msquic::Registration::new(&*MSQUIC_API, ptr::null());

    let listener = new_server(&registration).expect("new_server");
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    listener
        .start(&[msquic::Buffer::from("test")], Some(addr))
        .expect("listener start");
    let server_addr = listener.local_addr().expect("listener local_addr");

    let server_task = tokio::spawn(async move {
        let res = listener.accept().await;
        assert!(res.is_ok());
        server_rx.recv().await.expect("recv");

        let conn = res.expect("accept");
        let res = conn.open_outbound_stream(crate::StreamType::Bidirectional, false).await;
        assert_eq!(res.err(), Some(StartError::ConnectionLost(ConnectionError::ShutdownByPeer(1))));

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

    let client_config = new_client_config(&registration).expect("new_client_config");
    let conn = Connection::new(msquic::Connection::new(&registration), &registration);
    let conn1 = Connection::new(msquic::Connection::new(&registration), &registration);
    let client_task = tokio::spawn(async move {
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

        if let Err(ConnectionError::ShutdownByTransport(_, _)) = conn1
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

    let res = tokio::try_join!(server_task, client_task);
    assert!(res.is_ok());
}

#[tokio::test]
async fn stream_validation() {
    let registration = msquic::Registration::new(&*MSQUIC_API, ptr::null());
    let listener = new_server(&registration).expect("new_server");
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
        let mut buf = [0; 1024];

        let stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");
        server_rx.recv().await.expect("recv");
        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Err(ReadError::Reset(0)));

        let stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");
        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Err(ReadError::Reset(0)));

        let stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");
        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));

        let res = poll_fn(|cx| stream.poll_finish_write(cx)).await;
        assert!(res.is_ok());

        let stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Err(ReadError::Reset(0)));

        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        assert_eq!(res, Ok(11));

        let stream = conn
            .accept_inbound_stream()
            .await
            .expect("accept_inbound_stream");

        let res = poll_fn(|cx| stream.poll_write(cx, b"hello world", true)).await;
        if res.is_err() {
            assert_eq!(res, Err(WriteError::Stopped(0)));
        } else {
            assert_eq!(res, Ok(11));
        }

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));

        server_tx.send(()).await.expect("send");

        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(&registration).expect("new_client_config");
    let conn = Connection::new(msquic::Connection::new(&registration), &registration);

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

        let stream = res.expect("timeout").expect("open_outbound_stream");
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

        let stream = res.expect("open_outbound_stream");
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

        let stream = res.expect("open_outbound_stream");
        let res = poll_fn(|cx| stream.poll_abort_write(cx, 0)).await;
        assert_eq!(res, Ok(()));

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(11));

        let res = poll_fn(|cx| stream.poll_read(cx, &mut buf)).await;
        assert_eq!(res, Ok(0));

        let res = conn
            .open_outbound_stream(crate::StreamType::Bidirectional, false)
            .await;
        assert!(res.is_ok());

        let stream = res.expect("open_outbound_stream");
        let res = poll_fn(|cx| stream.poll_abort_read(cx, 0)).await;
        assert_eq!(res, Ok(()));
        // not waiting for the server to receive stopping

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

fn new_server(registration: &msquic::Registration) -> Result<Listener> {
    let alpn = [msquic::Buffer::from("test")];
    let configuration = msquic::Configuration::new(
        registration,
        &alpn,
        msquic::Settings::new()
            .set_idle_timeout_ms(10000)
            .set_peer_bidi_stream_count(1)
            .set_peer_unidi_stream_count(1),
    );
    let cred_config = SelfSignedCredentialConfig::new()?;
    configuration.load_credential(cred_config.as_cred_config_ref());
    let listener = Listener::new(
        msquic::Listener::new(registration),
        registration,
        configuration,
    );
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
            .set_peer_unidi_stream_count(1),
    );
    let mut cred_config = msquic::CredentialConfig::new_client();
    cred_config.cred_flags |= msquic::CREDENTIAL_FLAG_NO_CERTIFICATE_VALIDATION;
    configuration.load_credential(&cred_config);
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
