#!/bin/bash

set -eux

mkdir -p ./certs

echo "subjectAltName = DNS:localhost" > certs/v3.txt

# Create CA key and certificate
openssl req -x509 -newkey rsa:4096 -keyout ./certs/ca.key -out ./certs/ca.crt -nodes -days 365 -subj "/CN=MyCA"

# Create server key and CSR
openssl req -newkey rsa:4096 -keyout ./certs/server.key -out ./certs/server.csr -nodes -subj "/CN=localhost"

# Sign server certificate with CA
openssl x509 -req -in ./certs/server.csr -CA ./certs/ca.crt -CAkey ./certs/ca.key -CAcreateserial -out ./certs/server.crt -days 365 -extfile ./certs/v3.txt

# Create client key and CSR
openssl req -newkey rsa:4096 -keyout ./certs/client.key -out ./certs/client.csr -nodes -subj "/CN=localhost"

# Sign client certificate with CA
openssl x509 -req -in ./certs/client.csr -CA ./certs/ca.crt -CAkey ./certs/ca.key -CAcreateserial -out ./certs/client.crt -days 365 -extfile ./certs/v3.txt