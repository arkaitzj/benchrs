# benchrs
Apache Benchmark style http bench tool written in async rust
```
Benchrs 0.1.10-alpha.0
Arkaitz Jimenez <arkaitzj@gmail.com>
Does http benchmarks

USAGE:
    benchrs [OPTIONS] <url>

ARGS:
    <url>    The url to hit

OPTIONS:
    -c <concurrency>           Sets the concurrency level
    -h, --help                 Print help information
    -H <header>...             Sets a custom header
    -k                         Enables keep alive
    -m <method>                Request method: default GET
    -n <request number>        Sets the number of requests
    -p <postfile>              File attach as request body
    -v                         Increases verbosity
    -V, --version              Print version information
```
