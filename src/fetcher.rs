use anyhow::{bail, Context as _, Result};
use std::net::TcpStream;
use crate::{Event, ProducerRequest};
use log::*;
use futures::prelude::*;
use url::Url;
use std::time::Instant;
use smol::Async;
use crate::parser::Parser;
use std::str::from_utf8;
use async_native_tls::Certificate;
#[cfg(unix)]
use std::os::unix::net::{UnixStream};

enum Connection {
    Plain{ stream: Async<TcpStream>},
    Secure{ stream: async_native_tls::TlsStream<Async<TcpStream>>},
    #[cfg(unix)]
    Unix{ stream: Async<UnixStream>},
    Disconnected
}

impl Connection {
    fn is_disconnected(&self) -> bool {
        match &self {
            Connection::Disconnected => true,
            _ => false
        }
    }
}

async fn connect(addr: &str, cert: &Option<Certificate>) -> Result<Connection> {
    let url = Url::parse(addr).context("Parsing connect address")?;

    match url.scheme() {
        "http" => {
            let hostport = format!("{}:{}", url.host().context("cannot parse host")?, url.port_or_known_default().context("No port recognized")?);
            let stream = Async::<TcpStream>::connect(hostport).await?;
            Ok(Connection::Plain{stream})
        },
        "https" => {
            trace!("Negotiating TLS...");
            let tls_neg = Instant::now();
            let host = url.host().context("cannot parse host")?;
            let hostport = format!("{}:{}", host, url.port_or_known_default().context("No port recognized")?);
            let stream = Async::<TcpStream>::connect(hostport).await?;
            let mut stream_builder = async_native_tls::TlsConnector::new();
            if let Some(ref cert) = cert {
                stream_builder = stream_builder.add_root_certificate(cert.clone());
            }
            let stream = stream_builder.connect(host.to_string(), stream).await?;
            trace!("TLS negotiated in {}", tls_neg.elapsed().as_millis());
            Ok(Connection::Secure{stream})
        },
        #[cfg(unix)]
        "unix" => {
            let path = url.path();
            Ok(Connection::Unix{
                stream: Async::<UnixStream>::connect(path).await.context(format!("Connecting to: {}", path))?
            })
        },
        unknown_scheme => bail!("Unknown scheme: {}", unknown_scheme)
    }

}

pub async fn fetch(producer: piper::Receiver<ProducerRequest>, id: usize, event_sink: piper::Sender<Event>, keepalive: bool, cert: Option<Certificate>) -> Result<()> {
    trace!("Fetch!");

    let mut req_handled = 0;
    let mut conn = Connection::Disconnected;
    let mut parser = crate::parser::Parser::new(10_000);
    debug!("Fetcher {} started", id);
    'recv_loop: while let Some(mut request) = producer.recv().await {
        let mut finished = false;

        while !finished {
            req_handled += 1;
            parser.reset();
            if conn.is_disconnected() || ! keepalive {
                let conn_start = Instant::now();
                let conn_time = conn_start.elapsed();
                conn = connect(&request.addr, &cert).await.context("Connecting")?;
                let conn_ready = conn_start.elapsed();
                event_sink.send(Event::Connection{id, conn_time, tls_time: None, conn_ready}).await;
            }
            let request_start = Instant::now();
            let req_result = match conn {
                Connection::Plain{ref mut stream}  => do_request(stream, &mut request, &mut parser).await,
                Connection::Secure{ref mut stream} => do_request(stream, &mut request, &mut parser).await,
                #[cfg(unix)]
                Connection::Unix{ref mut stream} => do_request(stream, &mut request, &mut parser).await,
                _ => panic!("Disconnected!")
            };
            if let Ok(status) = req_result {
                match status {
                    Status::Redirect => {
                        finished = false;
                    },
                    Status::Continue => {
                        finished = true;
                    },
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
            event_sink.send(Event::Request{id, request_time: request_start.elapsed()}).await;
        }
        trace!("[{}] handled: {}, queue size: {}", id, req_handled, producer.len());

    }
    debug!("Fetcher {} finished, handled {} requests, {} bytes  in buffer", id, req_handled, parser.bytes.len());
    Ok(())
}

#[derive(Debug)]
enum Status{
    Continue,
    CloseConnection,
    Redirect
}

async fn do_request<T: AsyncRead + AsyncWrite + Unpin>(stream: &mut T, request: &mut ProducerRequest, parser: &mut Parser) -> Result<Status> {

    let req = request.get_request();
    trace!("Sending: \n{}", from_utf8(req.as_bytes()).unwrap());
    stream.write_all(req.as_bytes()).await?;
    let response = parser.read_header(stream).await.context("Request Header Parsing")?;
    trace!("Response header({}): \n{}", response.status, from_utf8(&parser.bytes[0..parser.read_idx])?);

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
        parser.drop_body(stream, &response).await.context("Body parsing")?;
    }
    trace!("Body dropped!");
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use smol::Task;
    use simplelog::*;
    use crate::producer::*;
    use async_native_tls::{Identity, TlsAcceptor};
    use flate2::read::GzDecoder;
    use std::io::Read;
    use galvanic_test::test_suite;



    const RESPONSE: &[u8] = br#"
HTTP/1.1 200 OK
Content-Type: text/html
Content-Length: 47

<!DOCTYPE html><html><body>Hello!</body></html>
"#;

    test_suite!{
        name fetcher_generic;

        use super::*;

        fixture base() -> (){
            setup(&mut self) {
                let config = ConfigBuilder::new().set_thread_level(LevelFilter::Info).build();
                let _ = SimpleLogger::init(log::LevelFilter::Warn, config);
            }
        }


        test https_fetch(base) {

            let (send,recv) = piper::chan(100);
            let (evsend, evrecv) = piper::chan(100);

            smol::run(async {
                let responses = [
                    ServerControl::Serve(from_utf8(RESPONSE).unwrap().to_string()),
                    ServerControl::CloseConnection
                ];
                let addr = "https://127.0.0.1:8001";
                server_mock(&addr, responses.to_vec()).await?;

                info!("Spawning test");

                let cert = async_native_tls::Certificate::from_pem(include_bytes!("../resources/test_certificate.pem"))?;
                let fetcher = fetch(recv, 0, evsend, true, Some(cert));
                Task::local(async move {
                    info!("Fetcher running!");
                    fetcher.await.unwrap();
                }).detach();

                send.send(ProducerRequest::new(&addr, RequestMethod::Get, vec![], RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await;
                assert!(matches!(evrecv.recv().await.unwrap(),Event::Connection{..}));
                info!("Connected");
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Request{..}));
                send.send(ProducerRequest::new(&addr, RequestMethod::Get, vec![], RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await;
                drop(send);
                Result::<()>::Ok(())
            }).unwrap();
        }

        test http_fetch(base) {
            let (send,recv) = piper::chan(100);
            let (evsend, evrecv) = piper::chan(100);

            smol::run(async {
                let responses = [
                    ServerControl::Serve(from_utf8(RESPONSE).unwrap().to_string()),
                    ServerControl::CloseConnection
                ];
                let addr = "http://127.0.0.1:8000";
                server_mock(&addr, responses.to_vec()).await.context("Server mock failure")?;

                info!("Spawning test");

                let cert = async_native_tls::Certificate::from_pem(include_bytes!("../resources/test_certificate.pem"))?;
                let fetcher = fetch(recv, 0, evsend, true, Some(cert));
                Task::local(async move { fetcher.await.context("Fetcher failure").unwrap();}).detach();

                send.send(ProducerRequest::new(&addr, RequestMethod::Get, vec![], RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await;
                assert!(matches!(evrecv.recv().await.unwrap(),Event::Connection{..}));
                info!("Connected");
                assert!(matches!(evrecv.recv().await.unwrap(), Event::Request{..}));
                send.send(ProducerRequest::new(&addr, RequestMethod::Get, vec![], RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await;
                drop(send);
                info!("Test finished!");
                Result::<()>::Ok(())
            }).unwrap();
        }
    }

  #[cfg(unix)]
  test_suite!{
        name fetcher_unix;

        use super::*;

        fixture base() -> (){
            setup(&mut self) {
                let config = ConfigBuilder::new().set_thread_level(LevelFilter::Info).build();
                let _ = SimpleLogger::init(log::LevelFilter::Warn, config);
            }
        }
        test response_fetch(base) {
            let (send,recv) = piper::chan(100);
            let (evsend, evrecv) = piper::chan(100);

            let temp_file = mktemp::Temp::new_path();
            let temp_file = temp_file.release(); // See todo below
            smol::run(async {
                let responses = [
                    ServerControl::Serve(from_utf8(RESPONSE).unwrap().to_string()),
                    ServerControl::CloseConnection
                ];
                let addr = format!("unix://{}", temp_file.to_str().unwrap()).to_owned();
                server_mock(&addr,responses.to_vec()).await?;

                info!("Spawning test");
                Task::spawn({
                    info!("Spawned fetcher!");
                    async move {
                      info!("Fetcher running");
                      let res = fetch(recv, 0, evsend, true, None).await.context("Fetcher failure");
                      info!("Res is {:?}", res);

                }}).detach();
                info!("Spawned!");
                send.send(ProducerRequest::new(&addr, RequestMethod::Head, vec!["Host: localhost".to_string()], RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await;
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
           // let _ = SimpleLogger::init(log::LevelFilter::Trace, Config::default());
            let (send,recv) = piper::chan(100);
            let (evsend, evrecv) = piper::chan(100);

            let temp_file = mktemp::Temp::new_path();
            let temp_file = temp_file.release(); // See todo below
            smol::run(async {
                let mut d: GzDecoder<&[u8]> = GzDecoder::new(include_bytes!("../resources/yahoo.head.gz"));
                let mut buffer: Vec<u8> = Vec::new();
                d.read_to_end(&mut buffer).unwrap();

                let responses = [
                    ServerControl::Serve(from_utf8(&buffer).unwrap().to_string()),
                    ServerControl::CloseConnection
                ];
                let addr = format!("unix://{}", temp_file.to_str().unwrap()).to_owned();
                server_mock(&addr,responses.to_vec()).await?;

                info!("Spawning test");
                Task::spawn({
                    info!("Spawned fetcher!");
                    async move {
                      info!("Fetcher running");
                      let res = fetch(recv, 0, evsend, true, None).await.context("Fetcher failure");
                      info!("Res is {:?}", res);

                }}).detach();
                info!("Spawned!");
                send.send(ProducerRequest::new(&addr, RequestMethod::Head, vec!["Host: localhost".to_string()], RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })?).await;
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

    }

    #[derive(Clone)]
    enum ServerControl {
        Serve(String),
        CloseConnection
    }

    async fn serve<T: 'static + AsyncRead + AsyncWrite + Unpin + std::marker::Send>(mut stream: T, responses: Vec<ServerControl>) {
        // Spawn a background task serving this connection.
        Task::local(async move {
            let mut buffer = vec![0u8;1_000_000];
            for respo in responses {
                match respo {
                    ServerControl::Serve(response) => {
                        info!("Server sending response");
                        stream.read(&mut buffer).await.unwrap();
                        stream.write_all(response.as_bytes()).await.unwrap()
                    },
                    _ =>  {
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
                    let hostport = format!("{}:{}", url.host().context("cannot parse host")?, url.port_or_known_default().context("No port recognized")?);
                    let listener = Async::<TcpListener>::bind(hostport)?;
                    while let Ok((stream,_)) = listener.accept().await {
                        info!("Accepted request!");
                        serve(stream, responses.clone()).await
                    }

                },
                "https" => {
                    let identity = Identity::from_pkcs12(include_bytes!("../resources/test_identity.pfx"), "password")?;
                    let tls = TlsAcceptor::from(native_tls::TlsAcceptor::new(identity)?);
                    let hostport = format!("{}:{}", url.host().context("cannot parse host")?, url.port_or_known_default().context("No port recognized")?);
                    let listener = Async::<TcpListener>::bind(hostport)?;
                    while let Ok((stream,_)) = listener.accept().await {
                        info!("Accepted request!");
                        let stream = tls.accept(stream).await.unwrap();
                        serve(stream, responses.clone()).await
                    }
                },
                #[cfg(unix)]
                "unix" => {
                    use std::os::unix::net::UnixListener;
                    info!("Listening on unix socket: {}", url.path());
                    let listener = Async::<UnixListener>::bind(url.path()).context("Binding error")?;
                    info!("Awaiting for connection!");
                    while let Ok((stream,_)) = listener.accept().await {
                        info!("Accepted request!");
                        serve(stream, responses.clone()).await
                    }
                    info!("Listener exited");
                }
                unknown => panic!("Unknown scheme: {}", unknown)
            }
            info!("Server mock is done!");
            Result::<()>::Ok(())
        };


        Task::local( async {
            info!("Listener running async!");
            listener.await.context("Server Mock failure").unwrap();
        }).detach();
        Ok(())
    }
}

