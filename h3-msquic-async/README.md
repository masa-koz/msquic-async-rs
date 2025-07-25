# h3-msquic-async
[MsQuic-Async](https://github.com/masa-koz/msquic-async-rs/) based library for using [h3](https://github.com/hyperium/h3)

## Getting Started

Note that [MsQuic](https://github.com/microsoft/msquic), which is used to implement QUIC,
needs to be built and linked. This is done automatically when building h3-msquic-async, but
requires the cmake command to be available during the build process.

### Windows, Linux, MacOS
Add h3-msquic-async in dependencies of your Cargo.toml.
```toml
h3-msquic-async = { version = "0.2.0" }
```

The [examples](./examples/) directory can help get started.

### Server

```rust
    let listener = msquic_async::Listener::new(&registration, configuration)?;

    let addr: SocketAddr = "127.0.0.1:8443".parse()?;
    listener.start(&[msquic::BufferRef::from("h3")], Some(addr))?;
    let server_addr = listener.local_addr()?;

    info!("listening on {}", server_addr);

    // handle incoming connections and requests

    while let Ok(conn) = listener.accept().await {
        info!("new connection established");
        let root = root.clone();
        tokio::spawn(async move {
            let mut h3_conn =
                h3::server::Connection::new(h3_msquic_async::Connection::new(conn)).await?;
            loop {
                match h3_conn.accept().await {
                    Ok(Some(req_resolver)) => {
                        let (req, stream) = match req_resolver.resolve_request().await {
                            Ok(req) => req,
                            Err(err) => {
                                error!("error resolving request: {}", err);
                                continue;
                            }
                        };
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
                        break;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        });
    }
```

You can find a full server example in [`examples/h3-server.rs`](./examples/h3-server.rs)

### Client

``` rust
    let conn = msquic_async::Connection::new(&registration)?;
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
```

You can find a full client example in [`examples/h3-client.rs`](./examples/h3-client.rs)

## License

h3-msquic-async is provided under the MIT license. See [LICENSE](LICENSE).