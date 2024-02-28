#!/bin/bash
set -eu
for i in {0..1000}; do
    URI=$(openssl rand -hex 20)
    curl http://localhost:8080/$URI -so /dev/null || true
done