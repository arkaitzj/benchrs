export HERE=$(cargo run -- --help)
perl -0777 -i -pe 's/```\nBench.*```/```\n$ENV{HERE}\n```/igs' README.md 

