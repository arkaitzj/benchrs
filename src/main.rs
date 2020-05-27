use anyhow::Result;
use futures::prelude::*;
use smol::Task;
use std::thread;
use std::time::{Instant, Duration};
use log::*;
use simplelog::*;
use crate::fetcher::fetch;
use crate::producer::{ProducerRequest, RequestConfig};

mod parser;
mod fetcher;
mod producer;

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
    for _ in 0..num_threads {
	thread::spawn(|| smol::run(future::pending::<()>()));
    }

    let (s, r) = piper::chan(1000);
    let (sender, receiver) = piper::chan(1_000_000);
    let producer = Task::spawn({
        let addr = addr.to_owned();
        async move {

            let req = ProducerRequest::new(&addr, user_headers, RequestConfig{
                keepalive: k,
                ..RequestConfig::default()
            });
            for _ in 0..n {
	        futures::select! {
                    _ = s.send(req.clone()).fuse() => {},
                    _ = smol::Timer::after(Duration::from_secs(5)).fuse() => {
                        error!("Producer stopped after waiting with a full queue");
                        break;
                    }
                }
            }
            debug!("Producer finalised");
    }});

    let executor = Task::spawn({
        let r = r.clone();
        async move {
            let mut all_futs = Vec::new();
            for i in 0..c {
                all_futs.push(fetch(&addr, r.clone(), i, sender.clone(), k));
            }
            let results = futures::future::join_all(all_futs).await;
            let (successes,failures): (Vec<_>,Vec<_>) = results.iter().partition(|r|r.is_ok());
            if !failures.is_empty() {
                error!("{} fetchers failed and {} succeeded", failures.len(), successes.len());
            }
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

                        if nrequest % 1000 == 0 {
                            debug!("{} requests, queue: {}", nrequest, r.len());
                        }
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


