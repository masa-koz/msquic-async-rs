# msquic-async
[MsQuic](https://github.com/microsoft/msquic) based quic library that supports async operation.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/masa-koz/msquic-async-rs/actions/workflows/CI.yaml/badge.svg?branch=main)](https://github.com/masa-koz/msquic-async-rs/actions/workflows/CI.yaml)

## Getting Started

Note that MsQuic, which is used to implement QUIC, needs to be built and linked. This is done automatically when building h3-msquic-async, but requires the cmake command to be available during the build process.

### Windows
Add msquic-async in dependencies of your Cargo.toml.
```toml
msquic-async = { version = "0.2.0", features = ["tls-schannel"] }
```

### Linux, MacOS
Add msquic-async in dependencies of your Cargo.toml.
```toml
msquic-async = { version = "0.2.0" }
```

The [examples](https://github.com/masa-koz/msquic-async-rs/tree/main/msquic-async/examples) directory can help get started.

### Server

```rust
    let registration = msquic::Registration::new(ptr::null())
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
            anyhow::anyhow!("Configuration::load_credential failed: 0x{:x}", status)
        })?;
    let listener =
        msquic_async::Listener::new(msquic::Listener::new(), &registration, configuration);

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
```

You can find a full server example in [`msquic-async/examples/server.rs`](https://github.com/masa-koz/msquic-async-rs/tree/main/msquic-async/examples/server.rs)

### Client

``` rust
    let registration = msquic::Registration::new(ptr::null())
        .map_err(|status| anyhow::anyhow!("Registration::new failed: 0x{:x}", status))?;

    let alpn = [msquic::Buffer::from("sample")];

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

    let mut cred_config = msquic::CredentialConfig::new_client();
    cred_config.cred_flags |= msquic::CREDENTIAL_FLAG_NO_CERTIFICATE_VALIDATION;
    configuration
        .load_credential(&cred_config)
        .map_err(|status| {
            anyhow::anyhow!("Configuration::load_credential failed: 0x{:x}", status)
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
```

You can find a full client example in [`msquic-async/examples/client.rs`](https://github.com/masa-koz/msquic-async-rs/tree/main/msquic-async/examples/client.rs)

## License

msquic-async is provided under the MIT license. See [LICENSE](https://github.com/masa-koz/msquic-async-rs/blob/main/LICENSE).