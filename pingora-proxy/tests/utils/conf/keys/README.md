Some test certificates. The CA is specified in your package directory (grep for ca_file).

Some handy commands:
```
# Describe a pkey
openssl [ec|rsa|...] -in key.pem -noout -text
# Describe a cert
openssl x509 -in some_cert.crt -noout -text

# Generate self-signed cert
openssl ecparam -genkey -name secp256r1 -noout -out test_key.pem
openssl req -new -x509 -key test_key.pem -out test.crt -days 3650 -sha256 -subj '/CN=openrusty.org'

# Generate a cert signed by another
openssl ecparam -genkey -name secp256r1 -noout -out test_key.pem
openssl req -new -key test_key.pem -out test.csr
openssl x509 -req -in test.csr -CA server.crt -CAkey key.pem -CAcreateserial -CAserial test.srl -out test.crt -days 3650 -sha256

# Generate leaf cert
openssl x509 -req -in leaf.csr -CA intermediate.crt -CAkey intermediate.key -out leaf.crt -days 3650 -sha256 -extfile v3.ext

```

```
openssl version
# OpenSSL 3.1.1
echo '[v3_req]' > openssl.cnf
openssl req -config openssl.cnf -new -x509 -key key.pem -out server_rustls.crt -days 3650 -sha256 \
    -subj '/C=US/ST=CA/L=San Francisco/O=Cloudflare, Inc/CN=openrusty.org' \
    -addext "subjectAltName=DNS:*.openrusty.org,DNS:openrusty.org,DNS:cat.com,DNS:dog.com"
```


# Specific Examples

## Updating `intermediate.crt` certificate

```
# Generate the key file
openssl genrsa -out intermediate.key 2048

# Generate the signing request with the subject details filled in
openssl req -new -key intermediate.key -out intermediate.csr -subj '/C=US/ST=CA/O=Intermediate CA/CN=int.pingora.org'

# Evaluate the signing request with the root certificate
openssl x509 -req -in intermediate.csr -CA root.crt -CAkey root.key -CAcreateserial -CAserial intermediate.srl -extfile intermediate.cnf -extensions v3_intermediate_ca -out intermediate.crt -days 3650 -sha256 
```

## Updating `leaf.crt` certificate using the intermediate cert

```
# Generate the key file
openssl genrsa -out leaf.key 2048

# Generate the signing request with the subject details filled in
openssl req -new -key leaf.key -out leaf.csr -subj '/C=US/ST=CA/O=Internet Widgits Pty Ltd/CN=pingora.org'

# Evaluate the signing request with the root certificate
openssl x509 -req -in leaf.csr -CA intermediate.crt -CAkey intermediate.key -CAcreateserial -CAserial leaf.srl -extfile leaf.cnf -extensions v3 -out leaf.crt -days 3650 -sha256 
```
