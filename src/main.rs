use anyhow::{Context as _, Result};
use futures::prelude::*;
use smol::Task;
use url::Url;
use std::thread;
use std::time::{Instant, Duration};
use log::*;
use simplelog::*;
use crate::fetcher::fetch;

mod parser;
mod fetcher;

#[derive(Debug)]
pub enum Event {
    Connection{
        id: usize,
        conn_time: Duration,
        tls_time: Option<Duration>,
        conn_ready: Duration
    },
    Request{
        id: usize,
        request_time: Duration
    }
}


#[derive(Clone)]
pub struct ProducerRequest {
    config: RequestConfig,
    addr: String,
    headers: Vec<String>
}

#[derive(Clone)]
pub struct RequestConfig {
    keepalive: bool,
    useragent: String
}

impl ProducerRequest {
    fn new(addr: &str, user_headers: Vec<String>, config: RequestConfig) -> Self {
        return ProducerRequest{
            addr: addr.to_string(),
            config,
            headers: user_headers
        };
    }
    fn redirect(&mut self, addr: &str) {
       self.addr = addr.to_owned();
       // Ensure we do not override Host header
       self.headers.retain(|h| !h.starts_with("Host:") );
    }
    fn get_request(&self) -> String {
        let url: Url = Url::parse(&self.addr).unwrap();
        let host = url.host().context("cannot parse host").unwrap().to_string();
        let path = url.path().to_string();
        let query = match url.query() {
            Some(q) => format!("?{}", q),
            None => String::new(),
        };

        let connection = if self.config.keepalive { "keep-alive" } else { "close" };

        let mut headers = String::new();
        if ! caseless_find(&self.headers, "Host:")   { headers.push_str(&format!("Host: {}\r\n", host)); }
        if ! caseless_find(&self.headers, "Accept:") { headers.push_str(&format!("Accept: {}\r\n", "*/*")); }
        if ! caseless_find(&self.headers, "Connection:") { headers.push_str(&format!("Connection: {}\r\n", connection)); }
        if ! caseless_find(&self.headers, "User-Agent:") { headers.push_str(&format!("User-Agent: {}\r\n", self.config.useragent)); }
        self.headers.iter().for_each(|header| headers.push_str(&format!("{}\r\n",header)));

         // Construct a request.
        let req = format!(
            "GET {}{} HTTP/1.1\r\n{}\r\n",
            path, query, headers);
        return req;
    }

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

    info!("{}:{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
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

            let req = ProducerRequest::new(&addr, user_headers, RequestConfig{
                keepalive: k,
                useragent:format!("BenchRS/{}", env!("CARGO_PKG_VERSION"))
            });
            for _ in 0..n {
                s.send(req.clone()).await;
            }
            debug!("Producer finalised");
    }});

    let executor = Task::spawn({
        async move {
            let mut all_futs = Vec::new();
            for i in 0..c {
                all_futs.push(fetch(&addr, r.clone(), i, sender.clone(), k));
            }
            let _ = futures::future::join_all(all_futs).await;
    }});

    let reporter = Task::spawn(async move {
            let mut nrequest = 0;
            let mut nconnection = 0;
            let start = Instant::now();
            let mut requests = Vec::new();
            while let Some(ev) = receiver.recv().await {
                match ev {
                    Event::Connection{ .. } => {
                        nconnection+=1;
                    },
                    Event::Request{ request_time, ..} => { 
                        requests.push(request_time.as_millis());
                        nrequest+=1;
                    }
                }
            }
            let end = Instant::now();

            let avg = requests.iter().sum::<u128>() as f32 / requests.len() as f32;
            requests.sort();
            let mid = requests.len() / 2;
            let p95 = (requests.len() as f32 * 0.95).floor() as usize;
            let p99 = (requests.len() as f32 * 0.99).floor() as usize;
            let median = requests[mid];
            let elapsed = end-start;
            info!("Ran in {}s {} connections, {} requests with avg request time: {}ms, median: {}ms, 95th percentile: {}ms and 99th percentile: {}ms", elapsed.as_secs_f32(), nconnection, nrequest, avg, median, requests[p95], requests[p99]);
    });


    smol::block_on(async {
        futures::join!(executor, producer, reporter);
    });
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
