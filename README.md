# msquic-async
[MsQuic](https://github.com/microsoft/msquic) based quic library that supports async operation.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/masa-koz/msquic-async-rs/actions/workflows/CI.yaml/badge.svg?branch=main)](https://github.com/masa-koz/msquic-async-rs/actions/workflows/CI.yaml)

## Getting Started

Note that MsQuic, which is used to implement QUIC, needs to be built and linked. This is done automatically when building h3-msquic-async, but requires the cmake command to be available during the build process.

### Windows, Linux, MacOS
Add msquic-async in dependencies of your Cargo.toml.
```toml
msquic-async = { version = "0.3.0" }
```

The [examples](https://github.com/masa-koz/msquic-async-rs/tree/main/msquic-async/examples) directory can help get started.

### Server

```rust
    let registration = msquic::Registration::new(&msquic::RegistrationConfig::default())?;

    let alpn = [msquic::BufferRef::from("sample")];

    // create msquic-async listener
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

    #[cfg(any(not(windows), feature = "quictls"))]
    {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let cert = include_bytes!("cert.pem");
        let key = include_bytes!("key.pem");

        let mut cert_file = NamedTempFile::new()?;
        cert_file.write_all(cert)?;
        let cert_path = cert_file.into_temp_path();
        let cert_path = cert_path.to_string_lossy().into_owned();

        let mut key_file = NamedTempFile::new()?;
        key_file.write_all(key)?;
        let key_path = key_file.into_temp_path();
        let key_path = key_path.to_string_lossy().into_owned();

        let cred_config = msquic::CredentialConfig::new().set_credential(
            msquic::Credential::CertificateFile(msquic::CertificateFile::new(key_path, cert_path)),
        );

        configuration.load_credential(&cred_config)?;
    }

    #[cfg(all(windows, not(feature = "quictls")))]
    {
        use schannel::cert_context::{CertContext, KeySpec};
        use schannel::cert_store::{CertAdd, Memory};
        use schannel::crypt_prov::{AcquireOptions, ProviderType};
        use schannel::RawPointer;

        let cert = include_str!("../examples/cert.pem");
        let key = include_bytes!("../examples/key.pem");

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

        let cred_config = msquic::CredentialConfig::new().set_credential(
            msquic::Credential::CertificateContext(unsafe { context.as_ptr() }),
        );

        configuration.load_credential(&cred_config)?;
    };

    let listener = msquic_async::Listener::new(&registration, configuration)?;

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
```

You can find a full server example in [`msquic-async/examples/server.rs`](https://github.com/masa-koz/msquic-async-rs/tree/main/msquic-async/examples/server.rs)

### Client

``` rust
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
```

You can find a full client example in [`msquic-async/examples/client.rs`](https://github.com/masa-koz/msquic-async-rs/tree/main/msquic-async/examples/client.rs)

## License

msquic-async is provided under the MIT license. See [LICENSE](https://github.com/masa-koz/msquic-async-rs/blob/main/LICENSE).