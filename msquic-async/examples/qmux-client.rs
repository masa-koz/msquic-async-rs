use anyhow::Context;
use argh::FromArgs;
use msquic_async::msquic;
use std::env;
use std::fs::OpenOptions;
use std::future::poll_fn;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;

#[derive(FromArgs)]
/// client args
struct CmdOptions {
    /// target server address
    #[argh(option, default = "String::from(\"127.0.0.1:4567\")")]
    target: String,
    /// resumption ticket in hex
    #[argh(option)]
    ticket: Option<String>,
    /// mTLS
    #[argh(switch)]
    mtls: bool,
    /// CA certificate
    #[argh(switch)]
    ca: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd_opts: CmdOptions = argh::from_env();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stdout)
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

    #[cfg(not(windows))]
    {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let cred_config = msquic::CredentialConfig::new_client();

        let cred_config = if cmd_opts.mtls {
            let cert = include_bytes!("../certs/client.crt");
            let key = include_bytes!("../certs/client.key");

            let mut cert_file = NamedTempFile::new()?;
            cert_file.write_all(cert)?;
            let cert_path = cert_file.into_temp_path();
            let cert_path = cert_path.to_string_lossy().into_owned();

            let mut key_file = NamedTempFile::new()?;
            key_file.write_all(key)?;
            let key_path = key_file.into_temp_path();
            let key_path = key_path.to_string_lossy().into_owned();
            cred_config.set_credential(msquic::Credential::CertificateFile(
                msquic::CertificateFile::new(key_path, cert_path),
            ))
        } else {
            cred_config
        };

        let cred_config = if cmd_opts.ca {
            let ca_cert = include_bytes!("../certs/ca.crt");
            let mut ca_cert_file = NamedTempFile::new()?;
            ca_cert_file.write_all(ca_cert)?;
            let ca_cert_path = ca_cert_file.into_temp_path();
            let ca_cert_path = ca_cert_path.to_string_lossy().into_owned();
            cred_config.set_ca_certificate_file(ca_cert_path)
        } else {
            cred_config.set_credential_flags(msquic::CredentialFlags::NO_CERTIFICATE_VALIDATION)
        };
        
        configuration.load_credential(&cred_config)?;
    }

    #[cfg(windows)]
    {
        use schannel::cert_context::{CertContext, KeySpec};
        use schannel::cert_store::{CertAdd, Memory};
        use schannel::crypt_prov::{AcquireOptions, ProviderType};
        use schannel::RawPointer;

        let cert = include_str!("../certs/client.crt");
        let key = include_bytes!("../certs/client.key");

        let mut store = Memory::new().unwrap().into_store();

        let name = String::from("msquic-async-example-client");

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

        let cred_config = msquic::CredentialConfig::new_client().set_credential(
            msquic::Credential::CertificateContext(unsafe { context.as_ptr() }),
        );

        configuration.load_credential(&cred_config)?;
    };

    let conn = msquic_async::Connection::new_qmux(&registration)?;
    if let Ok(sslkeylogfile) = env::var("SSLKEYLOGFILE") {
        info!("SSLKEYLOGFILE is set: {}", sslkeylogfile);
        conn.set_sslkeylog_file(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(sslkeylogfile)?,
        )?;
    }

    if let Some(ticket) = cmd_opts.ticket {
        let ticket = hex::decode(ticket)?;
        conn.set_resumption_ticket(&ticket).context("failed to set resumption ticket")?;
    }

    let target = cmd_opts
        .target
        .parse::<std::net::SocketAddr>()
        .context("failed to parse target address")?;
    conn.start(&configuration, &target.ip().to_string(), target.port())
        .await?;
    if let Ok(local_addr) = conn.get_local_addr() {
        info!("local address: {}", local_addr);
    }
    if let Ok(remote_addr) = conn.get_remote_addr() {
        info!("remote address: {}", remote_addr);
    }

    {
        let conn = conn.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(event) = poll_fn(|cx| conn.poll_event(cx)).await {
                    match event {
                        msquic_async::ConnectionEvent::ResumptionTicketReceived {
                            resumption_ticket,
                        } => {
                            info!("Resumption Ticket received, length: {:?}", resumption_ticket.len());
                            eprint!("{}", hex::encode_upper(resumption_ticket));
                        }
                        _ => {}
                    }
                } else {
                    break;
                }
            }
            anyhow::Result::<()>::Ok(())
        });
    }
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
