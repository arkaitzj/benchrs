use std::net::TcpStream;
use anyhow::{bail, Context as _, Result};
use futures::prelude::*;
use smol::{Async, Task};
use url::Url;
use std::thread;
use std::time::Instant;
use log::*;
use simplelog::*;

mod parser;

#[derive(Debug)]
enum Event {
    Connection{id: usize},
    Request
}

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

async fn fetch(addr: &str, producer: piper::Receiver<String>, id: usize, event_sink: piper::Sender<Event>, keepalive: bool) -> Result<()> {
    // Parse the URL.
    let url = Url::parse(addr)?;
    let host = url.host().context("cannot parse host")?.to_string();
    let port = url.port_or_known_default().context("cannot guess port")?;

    let mut conn = Connection::Disconnected;
    while let Some(req) = producer.recv().await {
        if conn.is_disconnected() || ! keepalive {
            let stream = Async::<TcpStream>::connect(format!("{}:{}", host, port)).await?;
            conn = match url.scheme() {
                "http"  => { Connection::Plain{stream} },
                "https" => { Connection::Secure{ stream: async_native_tls::connect(&host, stream).await?}},
                scheme => bail!("unsupported scheme: {}", scheme),

            };
            event_sink.send(Event::Connection{id}).await;
        }

        trace!("Sending: \n{}", std::str::from_utf8(req.as_bytes()).unwrap());
        match conn {
            Connection::Plain{ref mut stream}=> {
                stream.write_all(req.as_bytes()).await?;
                parser::read_response(stream).await?;
            },
            Connection::Secure{ref mut stream} => {
                stream.write_all(req.as_bytes()).await.unwrap();
                parser::read_response(stream).await.context("Reading response")?;
            },
            _ => panic!("Disconnected!")
        }
        event_sink.send(Event::Request).await;
    }
    Ok(())
}

fn main() -> Result<()> {

    // Same number of threads as there are CPU cores.
    let num_threads = num_cpus::get().max(1);
    let matches = clap::App::new("Benchrs")
                          .version(env!("CARGO_PKG_VERSION"))
                          .author("Arkaitz Jimenez <arkaitzj@gmail.com>")
                          .about("Does http benchmarks")
                          .arg(clap::Arg::with_name("url")
                              .help("The url to hit")
                              .required(true))
                          .arg(clap::Arg::with_name("keepalive")
                              .short("k")
                              .help("Enables keep alive"))
                          .arg(clap::Arg::with_name("verbosity")
                              .help("Increases verbosity")
                              .short("v")
                              .multiple(true))
                          .arg(clap::Arg::with_name("request number")
                              .help("Sets the number of requests")
                              .short("n")
                              .takes_value(true)
                              .validator(|x| x.parse::<usize>().map(|_|()).map_err(|err|format!("Err is: {:?}", err))))
                          .arg(clap::Arg::with_name("header")
                              .multiple(true)
                              .help("Sets a custom header")
                              .short("H")
                              .takes_value(true))
                          .arg(clap::Arg::with_name("concurrency")
                              .help("Sets the concurrency level")
                              .short("c")
                              .takes_value(true)
                              .validator(|x| x.parse::<usize>().map(|_|()).map_err(|err|format!("Err is: {:?}", err))))
			  .get_matches();
    let n = matches.value_of("request number").map_or(1,|x| x.parse::<usize>().unwrap());
    let c = matches.value_of("concurrency").map_or(1,|x| x.parse::<usize>().unwrap());
    let k = matches.is_present("keepalive");
    let addr = matches.value_of("url").unwrap().to_owned();

    let log_level = match matches.occurrences_of("verbosity") { 
        0 => LevelFilter::Info,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace
    };

    let user_headers: Vec<String> = if matches.is_present("header") {
        matches.values_of("header").unwrap().map(|x|x.to_owned()).collect()
    } else {
        vec![]
    };

    let _ = SimpleLogger::init(log_level, Config::default());

    info!("Spawning {} threads", num_threads);
    // Run the thread-local and work-stealing executor on a thread pool.
    for _ in 0..num_threads {
	// A pending future is one that simply yields forever.
	thread::spawn(|| smol::run(future::pending::<()>()));
    }

    let (s, r) = piper::chan(1000);
    let (sender, receiver) = piper::chan(1_000_000);
    let producer = Task::spawn({
        let addr = addr.to_owned();
        async move {
            let url: Url = Url::parse(&addr).unwrap();
            let host = url.host().context("cannot parse host").unwrap().to_string();
            let path = url.path().to_string();
            let query = match url.query() {
                Some(q) => format!("?{}", q),
                None => String::new(),
            };

            let connection = if k { "keep-alive" } else { "close" };
            let user_agent = format!("BenchRS/{}", env!("CARGO_PKG_VERSION"));
            let mut headers = String::new();
            if ! caseless_find(&user_headers, "Host:")   { headers.push_str(&format!("Host: {}\r\n", host)); }
            if ! caseless_find(&user_headers, "Accept:") { headers.push_str(&format!("Accept: {}\r\n", "*/*")); }
            if ! caseless_find(&user_headers, "Connection:") { headers.push_str(&format!("Connection: {}\r\n", connection)); }
            if ! caseless_find(&user_headers, "User-Agent:") { headers.push_str(&format!("User-Agent: {}\r\n", user_agent)); }
            user_headers.into_iter().for_each(|header| headers.push_str(&format!("{}\r\n",header)));

            // Construct a request.
            let req = format!(
                "GET {}{} HTTP/1.1\r\n{}\r\n",
                path, query, headers);

            for _ in 0..n {
                s.send(req.clone()).await;
            }
            info!("Producer finalised");
    }});

    let executor = Task::spawn({
        let addr = addr.to_owned();
        async move {
            info!("Hello from an executor thread!");
            let mut all_futs = Vec::new();
            for i in 0..c {
                all_futs.push(fetch(&addr, r.clone(), i, sender.clone(), k));
            }
            let _ = futures::future::join_all(all_futs).await;
    }});

    let reporter = Task::spawn({
        async move {
            let mut nrequest = 0;
            let mut nconnection = 0;
            let start = Instant::now();
            while let Some(ev) = receiver.recv().await {
                match ev {
                    Event::Connection{..} => nconnection+=1,
                    Event::Request => nrequest+=1
                }
            }
            let end = Instant::now();

            let elapsed = end-start;
            info!("Reporter finalised: {} connections, {} requests with avg request time: {}ms", nconnection, nrequest, elapsed.as_millis() / nrequest);
    }});


    let start = Instant::now();
    smol::block_on(async {
        let what = futures::join!(executor, producer, reporter);
        info!("What was: {:?}", what);
    });
    info!("Ran in {}", start.elapsed().as_secs_f32());
    Ok(())
}

fn caseless_find<T: AsRef<str>>(haystack: &[T], needle: &str) -> bool {

    for item in haystack {
        if (*item).as_ref().to_lowercase().starts_with(&needle.to_lowercase()) {
            return true;
        }
    }
    return false;
}
