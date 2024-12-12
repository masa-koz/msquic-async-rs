use futures::future;
use std::ptr;
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::TRACE)
        .init();

    let api =
        msquic::Api::new().map_err(|status| anyhow::anyhow!("Api::new failed: 0x{:x}", status))?;
    let registration = msquic::Registration::new(&api, ptr::null())
        .map_err(|status| anyhow::anyhow!("Registration::new failed: 0x{:x}", status))?;

    let alpn = [msquic::Buffer::from("h3")];
    let configuration = msquic::Configuration::new(
        &registration,
        &alpn,
        msquic::Settings::new()
            .set_idle_timeout_ms(10000)
            .set_peer_bidi_stream_count(1)
            .set_peer_unidi_stream_count(1)
            .set_datagram_receive_enabled(true)
            .set_stream_multi_receive_enabled(true),
    )
    .unwrap();
    let mut cred_config = msquic::CredentialConfig::new_client();
    cred_config.cred_flags |= msquic::CREDENTIAL_FLAG_NO_CERTIFICATE_VALIDATION;
    configuration.load_credential(&cred_config).unwrap();

    let conn =
        msquic_async::Connection::new(msquic::Connection::new(&registration), &registration, &api);
    conn.start(&configuration, "127.0.0.1", 8443)
        .await?;
    let h3_conn = h3_msquic::Connection::new(conn);
    let (mut driver, mut send_request) = h3::client::new(h3_conn).await?;

    let drive = async move {
        future::poll_fn(|cx| driver.poll_close(cx)).await?;
        Ok::<(), anyhow::Error>(())
    };
    let request = async move {
        println!("sending request ...");

        let req = http::Request::builder()
            .uri("https://127.0.0.1:8443/")
            .body(())?;

        // sending request results in a bidirectional stream,
        // which is also used for receiving response
        let mut stream = send_request.send_request(req).await?;

        // finish on the sending side
        stream.finish().await?;

        println!("receiving response ...");

        let resp = stream.recv_response().await?;

        println!("response: {:?} {}", resp.version(), resp.status());
        println!("headers: {:#?}", resp.headers());

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
    req_res?;
    drive_res?;

    // wait for the connection to be closed before exiting
    // let duration = std::time::Duration::from_millis(10000);
    // std::thread::sleep(duration);
    Ok(())
}
