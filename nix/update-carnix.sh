#!/bin/sh

set -eux

cargo build
cd nix/carnix

carnix generate-nix --src ../..
