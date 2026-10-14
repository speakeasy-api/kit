#!/bin/sh
# Runs a command with an empty HOME so tests never load the developer's
# global ~/.agents hooks, skills or other user configuration.
set -eu

home=$(mktemp -d)
trap 'rm -rf "$home"' EXIT
CARGO_HOME=${CARGO_HOME:-$HOME/.cargo} \
    RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup} \
    HOME=$home \
    "$@"
