#!/bin/sh

apt update && apt upgrade -y
apt install -y curl

curl https://sh.rustup.rs -sSf | sh -s -- -y

export PATH=/root/.cargo/bin:$PATH

rustup install stable

cd /workspaces/msquic-async-rs
git submodule update --init --recursive