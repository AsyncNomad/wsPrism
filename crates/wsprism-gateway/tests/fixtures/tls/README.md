Test-only TLS material for `tests/tls_listener.rs`: a private CA (`ca.pem`,
its key discarded) and two server certificates for 127.0.0.1 (`a`, `b`),
valid for 100 years. Never use them anywhere else.

Made with openssl (prime256v1):
    openssl req -x509 -newkey ec ... -subj /CN=wsprism-test-ca -out ca.pem
    openssl req -newkey ec ... -subj /CN=wsprism-test-<n> | openssl x509 -req -CA ca.pem ... (SAN IP:127.0.0.1)
