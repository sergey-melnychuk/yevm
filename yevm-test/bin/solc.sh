#!/bin/sh

rm ./sol/bin/*.json

## --optimize-runs=10000 \
solc --output-dir=./sol/bin/ \
--combined-json bin,bin-runtime \
--overwrite \
./sol/*.sol

