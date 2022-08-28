#!/bin/bash
set -euo pipefail
HERE=$(cargo run -- --help)
export HERE
perl -0777 -i -pe 's/```\nBench.*```/```\n$ENV{HERE}\n```/igs' README.md 

