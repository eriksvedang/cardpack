#!/bin/zsh

cargo build --release
cp ./target/release/cardpack ~/bin/cardpack
