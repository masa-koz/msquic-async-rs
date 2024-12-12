use std::{ffi::CString, io::Write, mem, net::SocketAddr, ptr};

use argh::FromArgs;
use msquic_async::msquic;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, info};

#[derive(FromArgs, Clone)]
/// server args
pub struct CmdOptions {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();

    let _cmd_opts: CmdOptions = argh::from_env();

    let api =
        msquic::Api::new().map_err(|status| anyhow::anyhow!("Api::new failed: 0x{:x}", status))?;
    let registration = msquic::Registration::new(&api, ptr::null())
        .map_err(|status| anyhow::anyhow!("Registration::new failed: 0x{:x}", status))?;

    let alpn = [msquic::Buffer::from("sample")];

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
    .map_err(|status| anyhow::anyhow!("Configuration::new failed: 0x{:x}", status))?;

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

    let addr: SocketAddr = "127.0.0.1:4567".parse()?;
    listener.start(&alpn, Some(addr))?;
    let server_addr = listener.local_addr()?;

    info!("listening on {}", server_addr);

    // handle incoming connections and streams
    while let Ok(conn) = listener.accept().await {
        info!("new connection established");
        tokio::spawn(async move {
            loop {
                match conn.accept_inbound_stream().await {
                    Ok(mut stream) => {
                        info!("new stream id: {}", stream.id().expect("stream id"));
                        let mut buf = [0u8; 1024];
                        let len = stream.read(&mut buf).await?;
                        info!(
                            "reading from stream: {}",
                            String::from_utf8_lossy(&buf[0..len])
                        );
                        stream.write_all(&buf[0..len]).await?;
                        mem::drop(stream);
                    }
                    Err(err) => {
                        error!("error on accept {}", err);
                        break;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        });
    }

    Ok(())
}
