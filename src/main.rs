use crate::fetcher::fetch;
use crate::producer::{ProducerRequest, RequestConfig};
use anyhow::{bail, Result};
use futures::prelude::*;
use log::*;
use simplelog::*;
use std::thread;
use std::time::{Duration, Instant};

mod fetcher;
mod parser;
mod producer;

#[derive(Debug)]
pub enum Event {
    Connection {
        id: usize,
        conn_time: Duration,
        tls_time: Option<Duration>,
        conn_ready: Duration,
    },
    Request {
        id: usize,
        request_time: Duration,
    },
}

fn main() -> Result<()> {
    // Same number of threads as there are CPU cores.
    let num_threads = num_cpus::get().max(1);
    let matches = clap::App::new("Benchrs")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Arkaitz Jimenez <arkaitzj@gmail.com>")
        .about("Does http benchmarks")
        .arg(
            clap::Arg::with_name("url")
                .help("The url to hit")
                .required(true),
        )
        .arg(
            clap::Arg::with_name("keepalive")
                .short('k')
                .help("Enables keep alive"),
        )
        .arg(
            clap::Arg::with_name("verbosity")
                .help("Increases verbosity")
                .short('v')
                .multiple(true),
        )
        .arg(
            clap::Arg::with_name("request number")
                .help("Sets the number of requests")
                .short('n')
                .takes_value(true)
                .validator(|x| {
                    x.parse::<usize>()
                        .map(|_| ())
                        .map_err(|err| format!("Err is: {:?}", err))
                }),
        )
        .arg(
            clap::Arg::with_name("postfile")
                .help("File attach as request body")
                .short('p')
                .takes_value(true),
        )
        .arg(
            clap::Arg::with_name("header")
                .multiple(true)
                .help("Sets a custom header")
                .short('H')
                .takes_value(true),
        )
        .arg(
            clap::Arg::with_name("method")
                .help("Request method: default GET")
                .short('m')
                .takes_value(true)
                .validator(|x| {
                    if x.parse::<producer::RequestMethod>().is_ok() {
                        Ok(())
                    } else {
                        Err(format!("{} is an invalid method", x))
                    }
                }),
        )
        .arg(
            clap::Arg::with_name("concurrency")
                .help("Sets the concurrency level")
                .short('c')
                .takes_value(true)
                .validator(|x| {
                    x.parse::<usize>()
                        .map(|_| ())
                        .map_err(|err| format!("Err is: {:?}", err))
                }),
        )
        .get_matches();
    let nrequests = matches
        .value_of("request number")
        .map_or(1, |x| x.parse::<usize>().unwrap());
    let concurrency = matches
        .value_of("concurrency")
        .map_or(1, |x| x.parse::<usize>().unwrap());
    let keepalive = matches.is_present("keepalive");
    let method: producer::RequestMethod =
        matches.value_of("method").unwrap_or("GET").parse().unwrap();
    let addr = matches.value_of("url").unwrap().to_owned();
    let postfile = matches.value_of("postfile");

    let log_level = match matches.occurrences_of("verbosity") {
        0 => LevelFilter::Info,
        1 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };

    let user_headers: Vec<String> = if matches.is_present("header") {
        matches
            .values_of("header")
            .unwrap()
            .map(|x| x.to_owned())
            .collect()
    } else {
        vec![]
    };
    let mut config_builder = ConfigBuilder::new();
    config_builder.set_time_format_custom(format_description!("[hour]:[minute]:[second].[subsecond]"));
    let _ = SimpleLogger::init(log_level, config_builder.build());
    info!("{}:{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

    let mut request_body = None;
    if let Some(ref postfile) = postfile {
        if !std::path::Path::new(postfile).is_file() {
            error!("File <{}> not found", postfile);
            bail!("File not found");
        }
        let mut buffer = vec![];
        use std::io::Read;
        std::fs::File::open(postfile)
            .unwrap()
            .read_to_end(&mut buffer)
            .unwrap();
        request_body = Some(buffer);
    }

    info!("Spawning {} threads", num_threads);
    for _ in 0..num_threads {
        thread::spawn(|| smol::block_on(future::pending::<()>()));
    }

    let (s, r) = piper::chan(100_000);
    let (sender, receiver) = piper::chan(1_000_000);
    let producer = smol::spawn({
//        let addr = addr.to_owned();
        async move {
            smol::Timer::after(std::time::Duration::from_millis(10)).await; // Lets give some time for fetchers to come online
            let req = ProducerRequest::new(
                &addr,
                method,
                user_headers,
                request_body,
                RequestConfig {
                    keepalive,
                    ..RequestConfig::default()
                },
            )?;
            debug!("Producer starting..");
            for _ in 0..nrequests {
                futures::select! {
                    _ = s.send(req.clone()).fuse() => {},
                    _ = futures::FutureExt::fuse(smol::Timer::after(Duration::from_secs(10))) => {
                        error!("Producer stopped after waiting with a full queue");
                        break;
                    }
                }
            }
            debug!("Producer finalised");
            Result::<()>::Ok(())
        }
    });

    let executor = smol::spawn({
        let r = r.clone();
        async move {
            let mut all_futs = Vec::new();
            for i in 0..concurrency {
                all_futs.push(smol::spawn(fetch(
                    r.clone(),
                    i,
                    sender.clone(),
                    keepalive,
                    None,
                )));
            }
            let results = futures::future::join_all(all_futs).await;
            let (successes, failures): (Vec<_>, Vec<_>) = results.iter().partition(|r| r.is_ok());
            if !failures.is_empty() {
                error!(
                    "{} fetchers failed and {} succeeded:\n{:?}\n...",
                    failures.len(),
                    successes.len(),
                    failures[0]
                );
            }
        }
    });
    let reporter = smol::spawn(async move {
        let mut nrequest = 0;
        let mut nconnection = 0;
        let start = Instant::now();
        let mut requests = Vec::new();
        while let Some(ev) = receiver.recv().await {
            match ev {
                Event::Connection { .. } => {
                    nconnection += 1;
                }
                Event::Request { request_time, .. } => {
                    requests.push(request_time.as_millis());
                    nrequest += 1;

                    if nrequest % 1000 == 0 {
                        debug!("{} requests, queue: {}", nrequest, r.len());
                    }
                }
            }
        }
        let end = Instant::now();

        if requests.is_empty() {
            info!("No requests were executed");
            return;
        }

        let avg = requests.iter().sum::<u128>() as f32 / requests.len() as f32;
        requests.sort();
        let mid = requests.len() / 2;
        let p95 = (requests.len() as f32 * 0.95).floor() as usize;
        let p99 = (requests.len() as f32 * 0.99).floor() as usize;
        let median = requests[mid];
        let elapsed = end - start;
        info!("Ran in {}s {} connections, {} requests with avg request time: {}ms, median: {}ms, 95th percentile: {}ms and 99th percentile: {}ms", elapsed.as_secs_f32(), nconnection, nrequest, avg, median, requests[p95], requests[p99]);
    });

    smol::block_on(async {
        let ((), producer_result, ()) = futures::join!(executor, producer, reporter);
        producer_result.expect("Successful execution");
    });
    Ok(())
}
