/// This file is based on the `server.rs` example from the `h3` crate.
use std::{ffi::CString, io::Write, net::SocketAddr, path::PathBuf, ptr, sync::Arc};

use argh::FromArgs;
use bytes::{Bytes, BytesMut};
use h3_msquic::msquic;
use h3_msquic::msquic_async;
use http::{Request, StatusCode};
use tempfile::NamedTempFile;
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

    let api =
        msquic::Api::new().map_err(|status| anyhow::anyhow!("Api::new failed: 0x{:x}", status))?;
    let registration = msquic::Registration::new(&api, ptr::null())
        .map_err(|status| anyhow::anyhow!("Registration::new failed: 0x{:x}", status))?;
    let alpn = [msquic::Buffer::from("h3")];

    // create msquic-async listener
    let configuration = msquic::Configuration::new(
        &registration,
        &alpn,
        msquic::Settings::new()
            .set_idle_timeout_ms(10000)
            .set_peer_bidi_stream_count(100)
            .set_peer_unidi_stream_count(100)
            .set_datagram_receive_enabled(true)
            .set_stream_multi_receive_enabled(true),
    )
    .unwrap();

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut cert_file = NamedTempFile::new().unwrap();
    cert_file
        .write_all(cert.serialize_pem().unwrap().as_bytes())
        .unwrap();
    let cert_path = cert_file.into_temp_path();
    let cert_path = CString::new(cert_path.to_str().unwrap().as_bytes()).unwrap();

    let mut key_file = NamedTempFile::new().unwrap();
    key_file
        .write_all(cert.serialize_private_key_pem().as_bytes())
        .unwrap();
    let key_path = key_file.into_temp_path();
    let key_path = CString::new(key_path.to_str().unwrap().as_bytes()).unwrap();

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

    configuration.load_credential(&cred_config).unwrap();
    let listener = msquic_async::Listener::new(
        msquic::Listener::new(&api),
        &registration,
        configuration,
        &api,
    );

    let addr: SocketAddr = "127.0.0.1:8443".parse()?;
    listener.start(&[msquic::Buffer::from("h3")], Some(addr))?;
    let server_addr = listener.local_addr()?;

    info!("listening on {}", server_addr);

    // handle incoming connections and requests

    while let Ok(conn) = listener.accept().await {
        info!("new connection established");
        let root = root.clone();
        tokio::spawn(async move {
            let mut h3_conn = h3::server::Connection::new(h3_msquic::Connection::new(conn)).await?;
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

    let resp = http::Response::builder().status(status).body(()).unwrap();

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
