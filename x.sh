#!/bin/bash

## copy the specific version of the binary to isolate it first
cargo build --release --bin replay
rm -rf ./tmp && mkdir -p tmp
cp ./target/release/replay ./tmp

for block in {24765791..24765890}; do bin/replay $block; done > 100.log 2>/dev/null &
## cat 100.log | grep FAIL | cut -d '=' -f 2 | cut -d ' ' -f 1 >> todo.log

for block in {24766061..24766361}; do bin/replay $block; done > 300.log 2>/dev/null &
## cat 300.log | grep FAIL | cut -d '=' -f 2 | cut -d ' ' -f 1 >> todo.log

## pgrep -af "x.sh"
## pkill -f x.sh
