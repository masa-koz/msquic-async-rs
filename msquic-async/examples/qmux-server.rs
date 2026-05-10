use anyhow::Context;
use argh::FromArgs;
use msquic_async::msquic;
use std::env;
use std::fs::OpenOptions;
use std::future::poll_fn;
use std::mem;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, info};

#[derive(FromArgs)]
/// server args
struct CmdOptions {
    /// bind address
    #[argh(option, default = "String::from(\"[::]:4567\")")]
    bind_addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd_opts: CmdOptions = argh::from_env();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stdout)
        .init();

    let _cmd_opts: CmdOptions = argh::from_env();

    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default())?;

    let alpn = [msquic::BufferRef::from("sample")];

    // create msquic-async listener
    let configuration = msquic::Configuration::open(
        &registration,
        &alpn,
        Some(
            &&msquic::Settings::new()
                .set_IdleTimeoutMs(10000)
                .set_ServerResumptionLevel(msquic::ServerResumptionLevel::ResumeAndZerortt)
                .set_PeerBidiStreamCount(100)
                .set_PeerUnidiStreamCount(100)
                .set_DatagramReceiveEnabled()
                .set_StreamMultiReceiveEnabled(),
        ),
    )?;

    #[cfg(not(windows))]
    {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let cert = include_bytes!("../certs/server.crt");
        let key = include_bytes!("../certs/server.key");
        let ca_cert = include_bytes!("../certs/ca.crt");

        let mut cert_file = NamedTempFile::new()?;
        cert_file.write_all(cert)?;
        let cert_path = cert_file.into_temp_path();
        let cert_path = cert_path.to_string_lossy().into_owned();

        let mut key_file = NamedTempFile::new()?;
        key_file.write_all(key)?;
        let key_path = key_file.into_temp_path();
        let key_path = key_path.to_string_lossy().into_owned();

        let mut ca_cert_file = NamedTempFile::new()?;
        ca_cert_file.write_all(ca_cert)?;
        let ca_cert_path = ca_cert_file.into_temp_path();
        let ca_cert_path = ca_cert_path.to_string_lossy().into_owned();

        let cred_config = msquic::CredentialConfig::new()
            .set_credential_flags(msquic::CredentialFlags::REQUIRE_CLIENT_AUTHENTICATION)
            .set_credential(msquic::Credential::CertificateFile(
                msquic::CertificateFile::new(key_path, cert_path),
            ))
            .set_ca_certificate_file(ca_cert_path);

        configuration.load_credential(&cred_config)?;
    }

    #[cfg(windows)]
    {
        use schannel::cert_context::{CertContext, KeySpec};
        use schannel::cert_store::{CertAdd, Memory};
        use schannel::crypt_prov::{AcquireOptions, ProviderType};
        use schannel::RawPointer;

        let cert = include_str!("../certs/server.crt");
        let key = include_bytes!("../certs/server.key");

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

        let cred_config = msquic::CredentialConfig::new()
            .set_credential_flags(msquic::CredentialFlags::REQUIRE_CLIENT_AUTHENTICATION)
            .set_credential(msquic::Credential::CertificateContext(unsafe {
                context.as_ptr()
            }));

        configuration.load_credential(&cred_config)?;
    };

    let listener = msquic_async::Listener::new_qmux(&registration, configuration)?;
    if let Ok(sslkeylog_file) = env::var("SSLKEYLOGFILE") {
        info!("SSLKEYLOGFILE is set: {}", sslkeylog_file);
        listener.set_sslkeylog_file(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(sslkeylog_file)?,
        )?;
    }

    let bind_addr = cmd_opts
        .bind_addr
        .parse::<std::net::SocketAddr>()
        .context("failed to parse bind address")?;

    listener.start(&alpn, Some(bind_addr))?;
    info!("listening on address {}", listener.local_addr()?);

    // handle incoming connections and streams
    while let Ok(conn) = listener.accept().await {
        info!("new connection established");
        if let Ok(local_addr) = conn.get_local_addr() {
            info!("local address: {}", local_addr);
        }
        if let Ok(remote_addr) = conn.get_remote_addr() {
            info!("remote address: {}", remote_addr);
        }
        tokio::spawn(async move {
            poll_fn(|cx| conn.poll_wait_start(cx)).await?;
            conn.send_resumption_ticket(false, None)?;
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
                        poll_fn(|cx| stream.poll_finish_write(cx)).await?;
                        mem::drop(stream);
                    }
                    Err(err) => {
                        error!("error on accept stream: {}", err);
                        break;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        });
    }

    Ok(())
}
