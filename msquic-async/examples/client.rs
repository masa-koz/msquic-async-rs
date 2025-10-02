use msquic_async::msquic;
use std::env;
use std::fs::OpenOptions;
use std::future::poll_fn;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default())?;

    let alpn = [msquic::BufferRef::from("sample")];
    let configuration = msquic::Configuration::open(
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
    )?;
    let cred_config = msquic::CredentialConfig::new_client()
        .set_credential_flags(msquic::CredentialFlags::NO_CERTIFICATE_VALIDATION);
    configuration.load_credential(&cred_config)?;

    let conn = msquic_async::Connection::new(&registration)?;
    if let Ok(sslkeylogfile) = env::var("SSLKEYLOGFILE") {
        info!("SSLKEYLOGFILE is set: {}", sslkeylogfile);
        conn.set_sslkeylog_file(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(sslkeylogfile)?,
        )?;
    }
    conn.start(&configuration, "127.0.0.1", 4567).await?;

    let mut stream = conn
        .open_outbound_stream(msquic_async::StreamType::Bidirectional, false)
        .await?;
    stream.write_all("hello".as_bytes()).await?;
    poll_fn(|cx| stream.poll_finish_write(cx)).await?;
    let mut buf = [0u8; 1024];
    let len = stream.read(&mut buf).await?;
    info!("received: {}", String::from_utf8_lossy(&buf[0..len]));
    poll_fn(|cx| conn.poll_shutdown(cx, 0)).await?;
    Ok(())
}
