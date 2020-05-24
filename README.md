# benchrs
Apache Benchmark style http bench tool written in async rust
```
Benchrs 0.1.0
Arkaitz Jimenez <arkaitzj@gmail.com>
Does http benchmarks

USAGE:
    benchrs [FLAGS] [OPTIONS] <url>

FLAGS:
    -h, --help       Prints help information
    -k               Enables keep alive
    -V, --version    Prints version information
    -v               Increases verbosity

OPTIONS:
    -c <concurrency>           Sets the concurrency level
    -n <request number>        Sets the number of requests

ARGS:
    <url>    The url to hit
```
