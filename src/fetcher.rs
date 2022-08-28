use crate::parser::Parser;
use crate::{Event, ProducerRequest};
use anyhow::{bail, Context as _, Result};
use async_native_tls::Certificate;
use futures::pin_mut;
use futures::prelude::*;
use log::*;
use smol::future::FutureExt;
use smol::Async;
use smol::Timer;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::str::from_utf8;
use std::time::Duration;
use std::time::Instant;
use url::Url;

enum Connection {
    Plain {
        stream: Async<TcpStream>,
    },
    Secure {
        stream: async_native_tls::TlsStream<Async<TcpStream>>,
    },
    #[cfg(unix)]
    Unix {
        stream: Async<UnixStream>,
    },
    Disconnected,
}

impl Connection {
    fn is_disconnected(&self) -> bool {
        matches!(self, Connection::Disconnected)
    }
}

pub async fn open_connection(hostport: &str) -> Result<Async<TcpStream>> {
    let resolved_addrs = hostport.to_socket_addrs()?;
    debug!("Addresses resolved from {hostport} to {resolved_addrs:?}");

    let connects = stream::iter(resolved_addrs).filter_map(|sockaddr| async move {
        trace!("Connection {sockaddr} await....");
        let stream = Async::<TcpStream>::connect(sockaddr)
            .or(async {
                Timer::after(Duration::from_millis(100)).await;
                Err(std::io::ErrorKind::TimedOut.into())
            })
            .await
            .ok()?;
        stream.writable().await.ok()?;
        debug!("Connected to {sockaddr}");
        Some(stream)
    });
    pin_mut!(connects);
    connects.next().await.context("Connecting to {hostport}")
}

async fn connect(addr: &str, cert: &Option<Certificate>) -> Result<Connection> {
    let url = Url::parse(addr).context("Parsing connect address")?;

    match url.scheme() {
        "http" => {
            let hostport = format!(
                "{}:{}",
                url.host().context(format!("cannot parse host: {url}"))?,
                url.port_or_known_default().context("No port recognized")?
            );
            let stream = open_connection(&hostport).await?;
            Ok(Connection::Plain { stream })
        }
        "https" => {
            let hostport = format!(
                "{}:{}",
                url.host().context(format!("cannot parse host: {url}"))?,
                url.port_or_known_default().context("No port recognized")?
            );
            let stream = open_connection(&hostport).await?;
            trace!("Negotiating TLS...");
            let tls_neg = Instant::now();
            let host = url.host().context("cannot parse host")?;
            let mut stream_builder = async_native_tls::TlsConnector::new();
            if let Some(ref cert) = cert {
                stream_builder = stream_builder.add_root_certificate(cert.clone());
            }
            let stream = stream_builder.connect(host.to_string(), stream).await?;
            trace!("TLS negotiated in {}", tls_neg.elapsed().as_millis());
            Ok(Connection::Secure { stream })
        }
        #[cfg(unix)]
        "unix" => {
            let path = url.path();
            let stream = Async::<UnixStream>::connect(path)
                .await
                .context(format!("Connecting to unix path: {}", path))?;
            info!("Stream is connected");
            Ok(Connection::Unix { stream })
        }
        unknown_scheme => bail!("Unknown scheme: {}", unknown_scheme),
    }
}

pub async fn fetch(
    producer: async_channel::Receiver<ProducerRequest>,
    id: usize,
    event_sink: async_channel::Sender<Event>,
    keepalive: bool,
    cert: Option<Certificate>,
) -> Result<()> {
    trace!("Fetch!");

    let mut req_handled = 0;
    let mut conn = Connection::Disconnected;
    let mut parser = crate::parser::Parser::new(10_000);
    debug!("Fetcher {} started", id);
    'recv_loop: while let Ok(mut request) = producer.recv().await {
        let mut finished = false;

        while !finished {
            req_handled += 1;
            parser.reset();
            if conn.is_disconnected() || !keepalive {
                let conn_start = Instant::now();
                let conn_time = conn_start.elapsed();
                let addr = &request.addr;
                conn = connect(addr, &cert)
                    .await
                    .context(format!("Connecting to {addr}"))?;
                let conn_ready = conn_start.elapsed();
                event_sink
                    .send(Event::Connection {
                        id,
                        conn_time,
                        tls_time: None,
                        conn_ready,
                    })
                    .await?;
            }
            let request_start = Instant::now();
            let req_result = match conn {
                Connection::Plain { ref mut stream } => {
                    do_request(stream, &mut request, &mut parser).await
                }
                Connection::Secure { ref mut stream } => {
                    do_request(stream, &mut request, &mut parser).await
                }
                #[cfg(unix)]
                Connection::Unix { ref mut stream } => {
                    do_request(stream, &mut request, &mut parser).await
                }
                _ => panic!("Disconnected!"),
            };
            if let Ok(status) = req_result {
                match status {
                    Status::Redirect => {
                        finished = false;
                    }
                    Status::Continue => {
                        finished = true;
                    }
                    Status::CloseConnection => {
                        trace!("[{}]Response closed the connection, disconnecting...", id);
                        conn = Connection::Disconnected;
                        finished = true;
                    }
                }
            } else {
                debug!("[{}]Error doing request: {:?}", id, req_result);
                conn = Connection::Disconnected;
                continue 'recv_loop;
            }
            event_sink
                .send(Event::Request {
                    id,
                    request_time: request_start.elapsed(),
                })
                .await?;
        }
        trace!(
            "[{}] handled: {}, queue size: {}",
            id,
            req_handled,
            producer.len()
        );
    }
    debug!(
        "Fetcher {} finished, handled {} requests, {} bytes  in buffer",
        id,
        req_handled,
        parser.bytes.len()
    );
    Ok(())
}

#[derive(Debug)]
enum Status {
    Continue,
    CloseConnection,
    Redirect,
}

async fn do_request<T: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut T,
    request: &mut ProducerRequest,
    parser: &mut Parser,
) -> Result<Status> {
    let req = request.get_request();
    trace!("Sending: \n{}", from_utf8(&req).unwrap());
    stream.write_all(&req).await?;
    let response = parser
        .read_header(stream)
        .await
        .context("Request Header Parsing")?;
    trace!(
        "Response header({}): \n{}",
        response.status,
        from_utf8(&parser.bytes[0..parser.read_idx])?
    );

    let mut status = Status::Continue;

    if response.is_redirect() {
        let redirect_to = response.headers["Location"].to_owned();
        trace!("Redirecting to: {}", redirect_to);
        request.redirect(&redirect_to)?;
        status = Status::Redirect;
    }
    if response.headers.contains_key("Connection") && response.headers["Connection"] == "close" {
        status = Status::CloseConnection;
    }
    if request.method != crate::producer::RequestMethod::Head {
        parser
            .drop_body(stream, &response)
            .await
            .context("Body parsing")?;
    }
    trace!("Body dropped!");
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::producer::*;
    use async_native_tls::{Identity, TlsAcceptor};
    use flate2::read::GzDecoder;
    use galvanic_test::test_suite;
    use simplelog::*;
    use std::io::Read;
    use std::net::TcpListener;

    const RESPONSE: &[u8] = br#"
HTTP/1.1 200 OK
Content-Type: text/html
Content-Length: 47

<!DOCTYPE html><html><body>Hello!</body></html>
"#;

    test_suite! {
        name fetcher_generic;

        use super::*;

        fixture base() -> (){
            setup(&mut self) {
                let level = LevelFilter::Info;
                let config = ConfigBuilder::new()
                    .set_thread_level(level)
                    .set_location_level(level)
                    .build();
                let _ = SimpleLogger::init(level, config);
            }
        }


        test https_fetch(base) {

            let (send,recv) = async_channel::bounded(100);
            let (evsend, evrecv) = async_channel::bounded(100);

            smol::block_on(async {
                let responses = [
                    ServerControl::Serve(ServeRequest::new(from_utf8(RESPONSE).unwrap().to_string())),
                    ServerControl::CloseConnection
                ];
                let addr = "https://localhost:8001";
                server_mock(&addr, responses.to_vec()).await?;
                Timer::after(Duration::from_millis(100)).await;

                info!("Spawning test");

                let cert = async_native_tls::Certificate::from_pem(include_bytes!("../resources/test_certs/root-ca.pem"))?;
                let fetcher = fetch(recv, 0, evsend, true, Some(cert));
                let fetcher_task = smol::spawn(async move {
                    info!("Fetcher running!");
                    fetcher.await
                });

                send.send(ProducerRequest::new(&addr, RequestMethod::Get, vec![], None, RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await?;
                assert!(matches!(evrecv.recv().await.unwrap(),Event::Connection{..}));
                info!("Connected");
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Request{..}));
                send.send(ProducerRequest::new(&addr, RequestMethod::Get, vec![], None, RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await?;
                drop(send);
                fetcher_task.await.context("Waiting for fetcher to finish...")?;
                Result::<()>::Ok(())
            }).unwrap();
        }

        test http_fetch(base) {
            let (send,recv) = async_channel::bounded(100);
            let (evsend, evrecv) = async_channel::bounded(100);

            smol::block_on(async {
                let responses = [
                    ServerControl::Serve(ServeRequest::new(from_utf8(RESPONSE).unwrap().to_string())),
                    ServerControl::CloseConnection
                ];
                let addr = "http://127.0.0.1:8000";
                server_mock(&addr, responses.to_vec()).await.context("Server mock failure")?;

                info!("Spawning test");

                let cert = async_native_tls::Certificate::from_pem(include_bytes!("../resources/test_certs/root-ca.pem"))?;
                let fetcher = fetch(recv, 0, evsend, true, Some(cert));
                smol::spawn(async move { fetcher.await.context("Fetcher failure")}).detach();

                send.send(ProducerRequest::new(&addr, RequestMethod::Get, vec![], None, RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await?;
                assert!(matches!(evrecv.recv().await.unwrap(),Event::Connection{..}));
                info!("Connected");
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Request{..}));
                send.send(ProducerRequest::new(&addr, RequestMethod::Get, vec![], None, RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await?;
                drop(send);
                info!("Test finished!");
                Result::<()>::Ok(())
            }).unwrap();
        }
    }

    #[cfg(unix)]
    test_suite! {
        name fetcher_unix;

        use super::*;

        fixture base() -> (){
            setup(&mut self) {
                let config = ConfigBuilder::new().set_thread_level(LevelFilter::Info).build();
                let _ = SimpleLogger::init(log::LevelFilter::Info, config);
            }
        }
        test response_fetch(base) {
            let (send,recv) = async_channel::bounded(100);
            let (evsend, evrecv) = async_channel::bounded(100);

            let temp_file = mktemp::Temp::new_path();
            let temp_file = temp_file.release(); // See todo below
            smol::block_on(async {
                let responses = [
                    ServerControl::Serve(ServeRequest::new(from_utf8(RESPONSE).unwrap().to_string())),
                    ServerControl::CloseConnection
                ];
                let addr = format!("unix://{}", temp_file.to_str().unwrap()).to_owned();
                server_mock(&addr,responses.to_vec()).await?;

                info!("Spawning test");
                smol::spawn({
                    info!("Spawned fetcher!");
                    async move {
                      info!("Fetcher running");
                      let res = fetch(recv, 0, evsend, true, None).await.context("Fetcher failure");
                      info!("Response is {:?}", res);

                }}).detach();
                info!("Spawned!");
                send.send(ProducerRequest::new(&addr, RequestMethod::Head, vec!["Host: localhost".to_string()], None, RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await?;
                info!("Receiving...");
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Connection{..}));
                info!("Connected");
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Request{..}));
                drop(send);
                info!("About to send OK back!");
                Result::<()>::Ok(())
            }).unwrap();
            // TODO: Remove when mktemp deletes unix sockets too as well as the temp_file.release() above
            std::fs::remove_file(temp_file.to_str().unwrap()).unwrap();
        }

        test response_fetch_head(base) {
            let (send,recv) = async_channel::bounded(100);
            let (evsend, evrecv) = async_channel::bounded(100);

            let temp_file = mktemp::Temp::new_path();
            let temp_file = temp_file.release(); // See todo below
            smol::block_on(async {
                let mut d: GzDecoder<&[u8]> = GzDecoder::new(include_bytes!("../resources/yahoo.head.gz"));
                let mut buffer: Vec<u8> = Vec::new();
                d.read_to_end(&mut buffer).unwrap();

                let responses = [
                    ServerControl::Serve(ServeRequest::new(from_utf8(&buffer).unwrap().to_string())),
                    ServerControl::CloseConnection
                ];
                let addr = format!("unix://{}", temp_file.to_str().unwrap()).to_owned();
                server_mock(&addr,responses.to_vec()).await?;

                info!("Spawning test");
                smol::spawn({
                    info!("Spawned fetcher!");
                    async move {
                      info!("Fetcher running");
                      let res = fetch(recv, 0, evsend, true, None).await.context("Fetcher failure");
                      info!("Res is {:?}", res);

                }}).detach();
                info!("Spawned!");
                send.send(ProducerRequest::new(&addr, RequestMethod::Head, vec!["Host: localhost".to_string()], None, RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await?;
                info!("Receiving...");
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Connection{..}));
                info!("Connected");
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Request{..}));
                drop(send);
                info!("About to send OK back!");
                Result::<()>::Ok(())
            }).unwrap();
            // TODO: Remove when mktemp deletes unix sockets too as well as the temp_file.release() above
            std::fs::remove_file(temp_file.to_str().unwrap()).unwrap();
        }

        test methods(base) {
            let (send,recv) = async_channel::bounded(100);
            let (evsend, evrecv) = async_channel::bounded(100);

            let temp_file = mktemp::Temp::new_path();
            let temp_file = temp_file.release(); // See todo below
            smol::block_on(async {
                let r201 = gz_to_string(include_bytes!("../resources/201.created.gz"));
                let r200 = gz_to_string(include_bytes!("../resources/200.ok.gz"));


                let responses = [
                    ServerControl::Serve(ServeRequest::new(r200.clone())
                                            .on_request(|request| assert!(request.starts_with("GET"), "Request does not start with GET:\n{}", request))),
                    ServerControl::Serve(ServeRequest::new(r201)
                                            .on_request(|request| assert!(request.starts_with("POST"), "Request does not start with POST:\n{}", request))),
                    ServerControl::Serve(ServeRequest::new(r200)
                                            .on_request(|request| assert!(request.starts_with("HEAD"), "Request does not start with HEAD:\n{}", request))),
                    ServerControl::CloseConnection
                ];
                let addr = format!("unix://{}", temp_file.to_str().unwrap()).to_owned();
                server_mock(&addr,responses.to_vec()).await?;

                info!("Spawning test");
                smol::spawn({
                    info!("Spawned fetcher!");
                    async move {
                      info!("Fetcher running");
                      let res = fetch(recv, 0, evsend, true, None).await.context("Fetcher failure");
                      info!("Res is {:?}", res);

                }}).detach();
                info!("Spawned!");
                send.send(ProducerRequest::new(&addr, RequestMethod::Get, vec!["Host: localhost_1".to_string()], None, RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await?;
                send.send(ProducerRequest::new(&addr, RequestMethod::Post, vec!["Host: localhost_2".to_string()], None, RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await?;
                send.send(ProducerRequest::new(&addr, RequestMethod::Head, vec!["Host: localhost_3".to_string()], None, RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await?;
                info!("Receiving...");
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Connection{..}));
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Request{..}));
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Request{..}));
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Request{..}));
                smol::Timer::after(std::time::Duration::from_millis(1)).await;
                assert!(evrecv.is_empty());
                drop(send);
                info!("About to send OK back!");
                Result::<()>::Ok(())
            }).unwrap();
            // TODO: Remove when mktemp deletes unix sockets too as well as the temp_file.release() above
            std::fs::remove_file(temp_file.to_str().unwrap()).unwrap();
        }

    }

    fn gz_to_string(bytes: &[u8]) -> String {
        let mut d: GzDecoder<&[u8]> = GzDecoder::new(bytes);
        let mut buffer: Vec<u8> = Vec::new();
        d.read_to_end(&mut buffer).unwrap();
        std::string::String::from_utf8(buffer).unwrap()
    }

    #[derive(Clone)]
    struct ServeRequest {
        content: String,
        request_verifier: Option<fn(String)>,
    }
    impl ServeRequest {
        fn new(content: String) -> Self {
            ServeRequest {
                content,
                request_verifier: None,
            }
        }
        fn on_request(mut self, request_verifier: fn(String)) -> Self {
            self.request_verifier = Some(request_verifier);
            self
        }
    }

    #[derive(Clone)]
    enum ServerControl {
        Serve(ServeRequest),
        CloseConnection,
    }

    async fn serve<T: 'static + AsyncRead + AsyncWrite + Unpin + std::marker::Send>(
        mut stream: T,
        responses: Vec<ServerControl>,
    ) {
        // Spawn a background task serving this connection.
        smol::spawn(async move {
            let mut buffer = vec![0u8; 1_000_000];
            for respo in responses {
                match respo {
                    ServerControl::Serve(response) => {
                        info!("Server sending response");
                        let ret = stream.read(&mut buffer).await.unwrap();
                        info!("Server receiver: {}", ret);
                        if let Some(verifier) = response.request_verifier {
                            verifier(from_utf8(&buffer).unwrap().to_string());
                        }
                        stream.write_all(response.content.as_bytes()).await.unwrap()
                    }
                    _ => {
                        info!("Dropping stream");
                        drop(stream);
                        break;
                    }
                }
            }
        })
        .detach();
    }

    async fn server_mock(addr: &str, responses: Vec<ServerControl>) -> Result<()> {
        let url = Url::parse(addr)?;
        info!("URL: {}", url.scheme());

        let listener = async move {
            match url.scheme() {
                "http" => {
                    let hostport = format!(
                        "{}:{}",
                        url.host().context(format!("cannot parse host: {url}"))?,
                        url.port_or_known_default().context("No port recognized")?
                    );
                    let addr = hostport.to_socket_addrs().unwrap().next().unwrap();
                    let listener = Async::<TcpListener>::bind(addr)?;
                    while let Ok((stream, _)) = listener.accept().await {
                        info!("Accepted request!");
                        serve(stream, responses.clone()).await
                    }
                }
                "https" => {
                    let identity = Identity::from_pkcs12(
                        include_bytes!("../resources/test_certs/server.pfx"),
                        "password",
                    )?;
                    let tls = TlsAcceptor::from(native_tls::TlsAcceptor::new(identity)?);
                    let hostport = format!(
                        "{}:{}",
                        url.host().context(format!("cannot parse host: {url}"))?,
                        url.port_or_known_default().context("No port recognized")?
                    );
                    let addr = hostport.to_socket_addrs().unwrap().next().unwrap();
                    let listener = Async::<TcpListener>::bind(addr)?;
                    while let Ok((stream, _)) = listener.accept().await {
                        info!("Accepted request!");
                        let stream = tls.accept(stream).await.unwrap();
                        serve(stream, responses.clone()).await
                    }
                }
                #[cfg(unix)]
                "unix" => {
                    use std::os::unix::net::UnixListener;
                    info!("Listening on unix socket: {}", url.path());
                    let listener =
                        Async::<UnixListener>::bind(url.path()).context("Binding error")?;
                    info!("Awaiting for connection!");
                    while let Ok((stream, _)) = listener.accept().await {
                        info!("Accepted request!");
                        serve(stream, responses.clone()).await
                    }
                    info!("Listener exited");
                }
                unknown => panic!("Unknown scheme: {}", unknown),
            }
            info!("Server mock is done!");
            Result::<()>::Ok(())
        };

        smol::spawn(async {
            info!("Listener running async!");
            listener.await.context("Server Mock failure").unwrap();
        })
        .detach();
        Ok(())
    }
}
