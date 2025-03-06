use msquic_async::msquic;
use std::ptr;
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

    let registration = msquic::Registration::new(ptr::null())
        .map_err(|status| anyhow::anyhow!("Registration::new failed: {}", status))?;

    let alpn = [msquic::BufferRef::from("sample")];

    let configuration = msquic::Configuration::new(
        &registration,
        &alpn,
        Some(&msquic::Settings::new()
            .set_IdleTimeoutMs(10000)
            .set_PeerBidiStreamCount(100)
            .set_PeerUnidiStreamCount(100)
            .set_DatagramReceiveEnabled()
            .set_StreamMultiReceiveEnabled()),
    )
    .map_err(|status| anyhow::anyhow!("Configuration::new failed: {}", status))?;

    let mut cred_config = msquic::CredentialConfig::new_client();
    cred_config.cred_flags |= msquic::CREDENTIAL_FLAG_NO_CERTIFICATE_VALIDATION;
    configuration
        .load_credential(&cred_config)
        .map_err(|status| {
            anyhow::anyhow!("Configuration::load_credential failed: {}", status)
        })?;

    let conn = msquic_async::Connection::new(msquic::Connection::new(), &registration)?;
    conn.start(&configuration, "127.0.0.1", 4567).await?;

    let mut stream = conn
        .open_outbound_stream(msquic_async::StreamType::Bidirectional, false)
        .await?;
    stream.write_all("hello".as_bytes()).await?;
    let mut buf = [0u8; 1024];
    let len = stream.read(&mut buf).await?;
    info!("received: {}", String::from_utf8_lossy(&buf[0..len]));

    Ok(())
}
