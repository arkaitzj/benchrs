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


pub async fn fetch(addr: &str, producer: piper::Receiver<ProducerRequest>, id: usize, event_sink: piper::Sender<Event>, keepalive: bool) -> Result<()> {
    // Parse the URL.
    let url = Url::parse(addr)?;
    let host = url.host().context("cannot parse host")?.to_string();
    let port = url.port_or_known_default().context("cannot guess port")?;

    let mut conn = Connection::Disconnected;
    debug!("Fetcher {} started", id);
    'recv_loop: while let Some(mut request) = producer.recv().await {
        let mut finished = false;
        while !finished {
            if conn.is_disconnected() || ! keepalive {
                let conn_start = Instant::now();
                let stream = Async::<TcpStream>::connect(format!("{}:{}", host, port)).await?;
                let conn_time = conn_start.elapsed();
                let mut tls_time = None;
                conn = match url.scheme() {
                    "http"  => { Connection::Plain{ stream } },
                    "https" => { 
                        let tls_neg = Instant::now();
                        let stream = async_native_tls::connect(&host, stream).await?;
                        tls_time = Some(tls_neg.elapsed());
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
                success
            } else {
                debug!("Error doing request: {:?}", req_result);
                conn = Connection::Disconnected;
                continue 'recv_loop;
            };
            event_sink.send(Event::Request{id, request_time: request_start.elapsed()}).await;
        }

    }
    debug!("Fetcher {} finished", id);
    Ok(())
}

async fn do_request<T: AsyncRead + AsyncWrite + Unpin>(stream: &mut T, request: &mut ProducerRequest) -> Result<bool> {
    
    let req = request.get_request();
    trace!("Sending: \n{}", from_utf8(req.as_bytes()).unwrap());
    stream.write_all(req.as_bytes()).await?;
    let ctx = parser::read_header(stream).await.context("Header Parsing")?;
    trace!("Response header: \n{}", from_utf8(&ctx.bytes[0..ctx.read_idx])?);

    let mut redirect_to = None;
    if let Some(resp) = ctx.response.as_ref() {
        if resp.is_redirect() {
            redirect_to = Some(resp.headers["Location"].to_owned());

        }
    }
    parser::drop_body(stream, ctx).await?;
    if let Some(addr) = redirect_to {
        request.redirect(&addr);
        trace!("Redirecting...");
        // Not finished
        return Ok(false)
    }
    return Ok(true);
}
