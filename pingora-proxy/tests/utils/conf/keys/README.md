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
```
