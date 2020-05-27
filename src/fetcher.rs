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

            debug!("Going for more!");
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
        req_handled += 1;
        debug!("[{}] handled: {}, queue size: {}", id, req_handled, producer.len());

    }
    debug!("Fetcher {} finished", id);
    Ok(())
}

async fn do_request<T: AsyncRead + AsyncWrite + Unpin>(stream: &mut T, request: &mut ProducerRequest) -> Result<bool> {
    
    let req = request.get_request();
    trace!("Sending: \n{}", from_utf8(req.as_bytes()).unwrap());
    stream.write_all(req.as_bytes()).await.unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use smol::Task;
    use simplelog::*;
    use crate::producer::*;
	



    #[test]
    fn test_fetch() {

        let _ = SimpleLogger::init(log::LevelFilter::Trace, Config::default());
	let (send,recv) = piper::chan(100);
	let (evsend, evrecv) = piper::chan(100);

        smol::run(async {
            let mut server = server_mock().boxed().fuse();

            let mut test = async move {
                info!("Spawning test");
		let fetcher = fetch("http://127.0.0.1:8000", recv, 0, evsend, true);
		Task::spawn(async move { fetcher.await;}).detach();

                send.send(ProducerRequest::new("http://127.0.0.1:8000", vec![], RequestConfig{
                    keepalive: true,
                    ..RequestConfig::default()
                })).await;
                evrecv.recv().await;

                smol::Timer::after(std::time::Duration::from_secs(1)).await;

                Result::<()>::Ok(())
            }.boxed().fuse();
	    futures::select! {
		_ = server => {}
		_ = test => {}
	    }
        });

    }


const RESPONSE: &[u8] = br#"
HTTP/1.1 200 OK
Content-Type: text/html
Content-Length: 47

<!DOCTYPE html><html><body>Hello!</body></html>
"#;


    async fn server_mock() -> Result<()> {

        let http = Async::<TcpListener>::bind("127.0.0.1:8000")?;
       // let https = (Async::<TcpListener>::bind("127.0.0.1:8001")?, Some(tls));
/*
	
	match &tls {
	    None => println!("Listening on http://{}", listener.get_ref().local_addr()?),
	    Some(_) => println!("Listening on https://{}", listener.get_ref().local_addr()?),
	}
*/

	let acceptor = async {
	    loop {
		// Accept the next connection.
		let (mut stream, _) = http.accept().await.unwrap();
//		let tls = tls.clone();

		// Spawn a background task serving this connection.
		Task::spawn(async move {
                    let mut buffer = vec![0u8;1_000_000];
                    loop {
                        info!("Server sending response");
                        stream.read(&mut buffer).await.unwrap();
                        stream.write_all(RESPONSE).await.unwrap();
                    }
		})
		.detach();
	    }
	};

        acceptor.await;
        Ok(())
    }
}

