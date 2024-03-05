use super::{
    Connection, ConnectionError, CredentialConfigCertFile, ListenError, Listener, MSQUIC_API,
};

use std::io::Write;
use std::net::SocketAddr;
use std::pin::Pin;
use std::ptr;

use anyhow::Result;

use tempfile::{NamedTempFile, TempPath};

#[tokio::test]
async fn connect_and_accept() {
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
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        Ok::<_, anyhow::Error>(())
    });

    let client_config = new_client_config(&registration).expect("new_client_config");
    let conn = Connection::new(msquic::Connection::new(&registration), &registration);
    let client_task = tokio::spawn(async move {
        let res = conn
            .start(
                &client_config,
                &format!("{}", server_addr.ip()),
                server_addr.port(),
            )
            .await;
        assert!(res.is_ok());
        Ok::<_, anyhow::Error>(())
    });

    let res = tokio::try_join!(server_task, client_task);
    assert!(res.is_ok());
}

#[tokio::test]
async fn connect_and_no_accept() {
    let registration = msquic::Registration::new(&*MSQUIC_API, ptr::null());
    let client_config = new_client_config(&registration).expect("new_client_config");
    let conn = Connection::new(msquic::Connection::new(&registration), &registration);
    if let Err(ConnectionError::ShutdownByTransport(_, _)) =
        conn.start(&client_config, "127.0.0.1", 8443).await
    {
        assert!(true, "ConnectionError::ShutdownByTransport");
    } else {
        assert!(false, "ConnectionError::ShutdownByTransport");
    }
}

#[tokio::test]
async fn connect_and_accept_and_stop() {
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
        let res = listener.stop().await;
        assert!(res.is_ok());
        if let Err(ListenError::Finished) = listener.accept().await {
            assert!(true, "ListenError::Finished");
        } else {
            assert!(false, "ListenError::Finished");
        }
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
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
        std::mem::drop(conn);
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
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

fn new_server(registration: &msquic::Registration) -> Result<Listener> {
    let alpn = [msquic::Buffer::from("test")];
    let configuration = msquic::Configuration::new(
        registration,
        &alpn,
        msquic::Settings::new()
            .set_idle_timeout_ms(1000)
            .set_peer_bidi_stream_count(1),
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
            .set_idle_timeout_ms(1000)
            .set_peer_bidi_stream_count(1),
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
