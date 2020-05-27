use anyhow::{bail, Context as _, Result};
use std::net::TcpStream;
use crate::{Event, ProducerRequest};
use log::*;
use futures::prelude::*;
use url::Url;
use std::time::Instant;
use smol::Async;
use crate::parser;
use std::str::from_utf8;
use async_native_tls::Certificate;

enum Connection {
    Plain{ stream: Async<TcpStream>},
    Secure{ stream: async_native_tls::TlsStream<Async<TcpStream>>},
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


pub async fn fetch(addr: &str, producer: piper::Receiver<ProducerRequest>, id: usize, event_sink: piper::Sender<Event>, keepalive: bool, cert: Option<Certificate>) -> Result<()> {
    trace!("Fetch!");
    // Parse the URL.
    let url = Url::parse(addr)?;
    let host = url.host().context("cannot parse host")?.to_string();
    let port = url.port_or_known_default().context("cannot guess port")?;

    let mut req_handled = 0;
    let mut conn = Connection::Disconnected;
    debug!("Fetcher {} started", id);
    'recv_loop: while let Some(mut request) = producer.recv().await {
        let mut finished = false;

        while !finished {
            if conn.is_disconnected() || ! keepalive {
                let conn_start = Instant::now();
                info!("Connect!");
                let stream = Async::<TcpStream>::connect(format!("{}:{}", host, port)).await?;
                let conn_time = conn_start.elapsed();
                let mut tls_time = None;
                conn = match url.scheme() {
                    "http"  => { Connection::Plain{ stream } },
                    "https" => { 
                        trace!("Negotiating TLS...");
                        let tls_neg = Instant::now();
                        let mut stream_builder = async_native_tls::TlsConnector::new();
                        if let Some(ref cert) = cert {
                            stream_builder = stream_builder.add_root_certificate(cert.clone());
                        }
                        let stream = stream_builder.connect(&host, stream).await?;
                        tls_time = Some(tls_neg.elapsed());
                        trace!("TLS negotiated in {}", tls_neg.elapsed().as_millis());
                        Connection::Secure{ stream }
                    },
                    scheme => bail!("unsupported scheme: {}", scheme),

                };
                let conn_ready = conn_start.elapsed();
                event_sink.send(Event::Connection{id, conn_time, tls_time, conn_ready}).await;
            }

            let request_start = Instant::now();
            let req_result = match conn {
                Connection::Plain{ref mut stream}  => do_request(stream, &mut request).await,
                Connection::Secure{ref mut stream} => do_request(stream, &mut request).await,
                _ => panic!("Disconnected!")
            };
            finished = if let Ok(success) = req_result {
                trace!("Sucess handling request");
                success
            } else {
                debug!("Error doing request: {:?}", req_result);
                conn = Connection::Disconnected;
                continue 'recv_loop;
            };
            event_sink.send(Event::Request{id, request_time: request_start.elapsed()}).await;
        }
        req_handled += 1;
        debug!("[{}] handled: {}, queue size: {}", id, req_handled, producer.len());

    }
    debug!("Fetcher {} finished", id);
    Ok(())
}

async fn do_request<T: AsyncRead + AsyncWrite + Unpin>(stream: &mut T, request: &mut ProducerRequest) -> Result<bool> {
    
    let req = request.get_request();
    trace!("Sending: \n{}", from_utf8(req.as_bytes()).unwrap());
    stream.write_all(req.as_bytes()).await?;
    trace!("Reading header...");
    let ctx = parser::read_header(stream).await.context("Header Parsing")?;
    trace!("Response header: \n{}", from_utf8(&ctx.bytes[0..ctx.read_idx])?);

    let mut redirect_to = None;
    if let Some(resp) = ctx.response.as_ref() {
        if resp.is_redirect() {
            redirect_to = Some(resp.headers["Location"].to_owned());

        }
    }
    parser::drop_body(stream, ctx).await?;
    trace!("Body dropped!");
    if let Some(addr) = redirect_to {
        request.redirect(&addr);
        trace!("Redirecting...");
        // Not finished
        return Ok(false)
    }
    return Ok(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use smol::Task;
    use simplelog::*;
    use crate::producer::*;
    use async_native_tls::{Identity, TlsAcceptor, TlsStream};
	



    #[test]
    fn test_fetch() {
//        let _ = SimpleLogger::init(log::LevelFilter::Trace, Config::default());
	let (send,recv) = piper::chan(100);
	let (evsend, evrecv) = piper::chan(100);

        smol::run(async {
            let responses = [
                ServerControl::Serve(from_utf8(RESPONSE).unwrap().to_string()),
                ServerControl::CloseConnection
            ];
            server_mock(responses.to_vec()).await;

            info!("Spawning test");
            let addr = "https://127.0.0.1:8001";
            let cert = async_native_tls::Certificate::from_pem(include_bytes!("../resources/test_certificate.pem"))?;
            let fetcher = fetch(&addr, recv, 0, evsend, true, Some(cert));
            Task::spawn(async move { fetcher.await;}).detach();

            send.send(ProducerRequest::new(&addr, vec![], RequestConfig{
                keepalive: true,
                ..RequestConfig::default()
            })).await;
            info!("{:?}", evrecv.recv().await.unwrap());
            info!("{:?}", evrecv.recv().await.unwrap());
            send.send(ProducerRequest::new(&addr, vec![], RequestConfig{
                keepalive: true,
                ..RequestConfig::default()
            })).await;
            drop(send);
            Result::<()>::Ok(())
        });

    }


const RESPONSE: &[u8] = br#"
HTTP/1.1 200 OK
Content-Type: text/html
Content-Length: 47

<!DOCTYPE html><html><body>Hello!</body></html>
"#;

#[derive(Clone)]
enum ServerControl {
    Serve(String),
    CloseConnection
}




    /// Listens for incoming connections and serves them.
    async fn listen(listener: Async<TcpListener>, responses: Vec<ServerControl>, tls: Option<TlsAcceptor>) -> Result<()> {
	// Display the full host address.
	match &tls {
	    None => info!("Listening on http://{}", listener.get_ref().local_addr()?),
	    Some(_) => info!("Listening on https://{}", listener.get_ref().local_addr()?),
	}

	loop {
	    // Accept the next connection.
	    let (mut stream, _) = listener.accept().await.unwrap();
            let tls = tls.clone();	
            info!("Accepted request!");
	    match tls {
                None => serve(stream, responses.clone()).await,
                Some(tls) => serve(tls.accept(stream).await.unwrap(), responses.clone()).await
            };
	}
    }
    
    async fn serve<T: 'static + AsyncRead + AsyncWrite + Unpin + std::marker::Send>(mut stream: T, responses: Vec<ServerControl>) {

        // Spawn a background task serving this connection.
        Task::spawn(async move {
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
    

    async fn server_mock(responses: Vec<ServerControl>) -> Result<()> {

        let identity = Identity::from_pkcs12(include_bytes!("../resources/test_identity.pfx"), "password")?;
        let tls = TlsAcceptor::from(native_tls::TlsAcceptor::new(identity)?);
        let http = listen(Async::<TcpListener>::bind("127.0.0.1:8000")?, responses.clone(), None);
        let https = listen(Async::<TcpListener>::bind("127.0.0.1:8001")?, responses.clone(), Some(tls));
        Task::spawn( async {
            future::try_join(http, https).await;
        }).detach();
        Ok(())
    }
}

