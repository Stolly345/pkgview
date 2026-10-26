#!/bin/sh
# Run a command in the Debian 12 build image with this project mounted (host has no Rust toolchain).
# If $SAMPLES names a folder of real pkgs, it is mounted read-only at /samples.
set -- ${SAMPLES:+-v "$SAMPLES:/samples:ro"} pkgview-build "$@"
exec docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -v "$(dirname "$(readlink -f "$0")")":/w \
  -v pkgview-cargo-registry:/opt/cargo/registry -w /w "$@"
