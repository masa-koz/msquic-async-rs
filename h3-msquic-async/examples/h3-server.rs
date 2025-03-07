/// This file is based on the `server.rs` example from the `h3` crate.
use std::{net::SocketAddr, path::PathBuf, ptr, sync::Arc};

use argh::FromArgs;
use bytes::{Bytes, BytesMut};
use h3_msquic_async::msquic;
use h3_msquic_async::msquic_async;
use http::{Request, StatusCode};
use tokio::{fs::File, io::AsyncReadExt};
use tracing::{error, info};

use h3::{error::ErrorLevel, quic::BidiStream, server::RequestStream};

#[derive(FromArgs, Clone)]
/// server args
pub struct CmdOptions {
    /// root path
    #[argh(option)]
    root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();

    let cmd_opts: CmdOptions = argh::from_env();
    let root = Arc::new(cmd_opts.root);

    let registration = msquic::Registration::new(ptr::null())
        .map_err(|status| anyhow::anyhow!("Registration::new failed: {}", status))?;
    let alpn = [msquic::BufferRef::from("h3")];

    // create msquic-async listener
    let configuration = msquic::Configuration::new(
        &registration,
        &alpn,
        Some(
            &msquic::Settings::new()
                .set_IdleTimeoutMs(10000)
                .set_PeerBidiStreamCount(100)
                .set_PeerUnidiStreamCount(100)
                .set_DatagramReceiveEnabled()
                .set_StreamMultiReceiveEnabled(),
        ),
    )
    .map_err(|status| anyhow::anyhow!("Configuration::new failed: {}", status))?;

    #[cfg(not(feature = "tls-schannel"))]
    {
        use std::{ffi::CString, io::Write};
        use tempfile::NamedTempFile;

        let cert = include_bytes!("cert.pem");
        let key = include_bytes!("key.pem");

        let mut cert_file = NamedTempFile::new()?;
        cert_file.write_all(cert)?;
        let cert_path = cert_file.into_temp_path();
        let cert_path = CString::new(cert_path.to_path_buf().as_os_str().as_encoded_bytes())?;

        let mut key_file = NamedTempFile::new()?;
        key_file.write_all(key)?;
        let key_path = key_file.into_temp_path();
        let key_path = CString::new(key_path.to_path_buf().as_os_str().as_encoded_bytes())?;

        let certificate_file = msquic::CertificateFile {
            private_key_file: key_path.as_ptr(),
            certificate_file: cert_path.as_ptr(),
        };

        let cred_config = msquic::CredentialConfig {
            cred_type: msquic::CREDENTIAL_TYPE_CERTIFICATE_FILE,
            cred_flags: msquic::CREDENTIAL_FLAG_NONE,
            certificate: msquic::CertificateUnion {
                file: &certificate_file,
            },
            principle: ptr::null(),
            reserved: ptr::null(),
            async_handler: None,
            allowed_cipher_suites: 0,
        };
        configuration
            .load_credential(&cred_config)
            .map_err(|status| {
                anyhow::anyhow!("Configuration::load_credential failed: {}", status)
            })?;
    }

    #[cfg(feature = "tls-schannel")]
    {
        use schannel::cert_context::{CertContext, KeySpec};
        use schannel::cert_store::{CertAdd, Memory};
        use schannel::crypt_prov::{AcquireOptions, ProviderType};
        use schannel::RawPointer;

        let cert = include_str!("cert.pem");
        let key = include_bytes!("key.pem");

        let mut store = Memory::new().unwrap().into_store();

        let name = String::from("msquic-async-example");

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

        let cred_config = msquic::CredentialConfig {
            cred_type: msquic::CREDENTIAL_TYPE_CERTIFICATE_CONTEXT,
            cred_flags: msquic::CREDENTIAL_FLAG_NONE,
            certificate: msquic::CertificateUnion {
                context: unsafe { context.as_ptr() },
            },
            principle: ptr::null(),
            reserved: ptr::null(),
            async_handler: None,
            allowed_cipher_suites: 0,
        };

        configuration
            .load_credential(&cred_config)
            .map_err(|status| {
                anyhow::anyhow!("Configuration::load_credential failed: 0x{:x}", status)
            })?;
    };

    let listener =
        msquic_async::Listener::new(msquic::Listener::new(), &registration, configuration)?;

    let addr: SocketAddr = "127.0.0.1:8443".parse()?;
    listener.start(&[msquic::BufferRef::from("h3")], Some(addr))?;
    let server_addr = listener.local_addr()?;

    info!("listening on {}", server_addr);

    // handle incoming connections and requests

    while let Ok(conn) = listener.accept().await {
        info!("new connection established");
        let root = root.clone();
        tokio::spawn(async move {
            let mut h3_conn =
                h3::server::Connection::new(h3_msquic_async::Connection::new(conn)).await?;
            loop {
                match h3_conn.accept().await {
                    Ok(Some((req, stream))) => {
                        info!("new request: {:#?}", req);

                        let root = root.clone();

                        tokio::spawn(async {
                            if let Err(e) = handle_request(req, stream, root).await {
                                error!("handling request failed: {}", e);
                            }
                        });
                    }

                    // indicating no more streams to be received
                    Ok(None) => {
                        break;
                    }

                    Err(err) => {
                        error!("error on accept {}", err);
                        match err.get_error_level() {
                            ErrorLevel::ConnectionError => break,
                            ErrorLevel::StreamError => continue,
                        }
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        });
    }

    Ok(())
}

async fn handle_request<T>(
    req: Request<()>,
    mut stream: RequestStream<T, Bytes>,
    serve_root: Arc<Option<PathBuf>>,
) -> anyhow::Result<()>
where
    T: BidiStream<Bytes>,
{
    let (status, to_serve) = match serve_root.as_deref() {
        None => (StatusCode::OK, None),
        Some(_) if req.uri().path().contains("..") => (StatusCode::NOT_FOUND, None),
        Some(root) => {
            let to_serve = root.join(req.uri().path().strip_prefix('/').unwrap_or(""));
            match File::open(&to_serve).await {
                Ok(file) => (StatusCode::OK, Some(file)),
                Err(e) => {
                    error!("failed to open: \"{}\": {}", to_serve.to_string_lossy(), e);
                    (StatusCode::NOT_FOUND, None)
                }
            }
        }
    };

    let resp = http::Response::builder().status(status).body(())?;

    match stream.send_response(resp).await {
        Ok(_) => {
            info!("successfully respond to connection");
        }
        Err(err) => {
            error!("unable to send response to connection peer: {:?}", err);
        }
    }

    if let Some(mut file) = to_serve {
        loop {
            let mut buf = BytesMut::with_capacity(4096 * 10);
            if file.read_buf(&mut buf).await? == 0 {
                break;
            }
            stream.send_data(buf.freeze()).await?;
        }
    }

    Ok(stream.finish().await?)
}
