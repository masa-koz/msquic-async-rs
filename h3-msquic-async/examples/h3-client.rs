/// This file is based on the `client.rs` example from the `h3` crate.
use futures::future;
use h3::error::{ConnectionError, StreamError};
use h3_msquic_async::msquic;
use h3_msquic_async::msquic_async;
use std::env;
use std::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tracing::{error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();

    let registration = msquic_async::Registration::new(&msquic::RegistrationConfig::default())?;

    let alpn = [msquic::BufferRef::from("h3")];
    let configuration = registration.open_configuration(
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
    // Validate the server certificate by default. The example server uses a
    // self-signed certificate (`cert.pem`), so we pin that same certificate as
    // the trusted CA.
    //
    // Setting `H3_CLIENT_INSECURE=1` skips validation via
    // `NO_CERTIFICATE_VALIDATION`. This is for local testing ONLY: it disables
    // authentication of the server and defeats TLS, so it must never be used in
    // production or copied into real client code.
    if env::var("H3_CLIENT_INSECURE").is_ok() {
        info!("H3_CLIENT_INSECURE is set: skipping certificate validation (test only)");
        let cred_config = msquic::CredentialConfig::new_client()
            .set_credential_flags(msquic::CredentialFlags::NO_CERTIFICATE_VALIDATION);
        configuration.load_credential(&cred_config)?;
    } else {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let ca_cert = include_bytes!("cert.pem");
        let mut ca_cert_file = NamedTempFile::new()?;
        ca_cert_file.write_all(ca_cert)?;
        let ca_cert_path = ca_cert_file.into_temp_path();
        let ca_cert_path_str = ca_cert_path.to_string_lossy().into_owned();

        let cred_config =
            msquic::CredentialConfig::new_client().set_ca_certificate_file(ca_cert_path_str);
        configuration.load_credential(&cred_config)?;
        // `ca_cert_path` is dropped (and the temp file removed) only here, after
        // `load_credential` has read it.
    }

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
    conn.start(&configuration, "127.0.0.1", 8443).await?;
    let h3_conn = h3_msquic_async::Connection::new(conn);
    let (mut driver, mut send_request) = h3::client::new(h3_conn).await?;

    let drive = async move {
        Err::<(), ConnectionError>(future::poll_fn(|cx| driver.poll_close(cx)).await)
    };
    let request = async move {
        info!("sending request ...");

        let req = http::Request::builder()
            .uri("https://127.0.0.1:8443/")
            .body(())?;

        // sending request results in a bidirectional stream,
        // which is also used for receiving response
        let mut stream = send_request.send_request(req).await?;

        // finish on the sending side
        stream.finish().await?;

        info!("receiving response ...");

        let resp = stream.recv_response().await?;

        info!("response: {:?} {}", resp.version(), resp.status());
        info!("headers: {:#?}", resp.headers());

        // `recv_data()` must be called after `recv_response()` for
        // receiving potential response body
        while let Some(mut chunk) = stream.recv_data().await? {
            let mut out = tokio::io::stdout();
            out.write_all_buf(&mut chunk).await?;
            out.flush().await?;
        }

        Ok::<_, anyhow::Error>(())
    };

    let (req_res, drive_res) = tokio::join!(request, drive);
    if let Err(err) = req_res {
        match err.downcast::<StreamError>() {
            Ok(err) => {
                if err.is_h3_no_error() {
                    info!("connection closed with H3_NO_ERROR");
                } else {
                    error!("request failed: {:?}", err);
                }
            }
            Err(err) => {
                error!("request failed: {:?}", err);
            }
        }
    }
    if let Err(err) = drive_res {
        if err.is_h3_no_error() {
            info!("connection closed with H3_NO_ERROR");
        } else {
            error!("connection closed with error: {:?}", err);
            return Err(err.into());
        }
    }

    Ok(())
}
