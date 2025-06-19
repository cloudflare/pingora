#!/usr/bin/env fish

# This generates as CA
cfssl gencert \
        -initca ./config/ca-csr.json \
        | cfssljson -bare ca

# CA Generates the profile certificates.
cfssl gencert \
        -ca ca.pem \
        -ca-key ca-key.pem \
        -config ./config/ca-config.json \
        -profile client ./config/client.json \
        | cfssljson -bare client

cfssl gencert \
        -ca ca.pem \
        -ca-key ca-key.pem \
        -config ./config/ca-config.json \
        -profile server ./config/server.json \
        | cfssljson -bare server
