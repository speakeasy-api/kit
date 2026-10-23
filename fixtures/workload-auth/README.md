# Local HTTPS test identity

These fixtures are exclusively for `src/workload_auth/https_tests.rs`. The
private key is a public, disposable test key, not a deployment credential.
Never trust this CA or use this identity outside tests. The issuing CA's
private key was discarded after signing.

- `ca.pem`: self-signed test CA, trusted only by the test HTTP client.
- `server.der`: leaf certificate for `localhost` and `127.0.0.1`.
- `server-key.der`: unencrypted PKCS#8 RSA key for the leaf certificate.

Generated with OpenSSL using a 2048-bit RSA CA and leaf key, a 100-year
validity period to avoid routine expiry, and leaf extensions
`subjectAltName=DNS:localhost,IP:127.0.0.1`, `basicConstraints=CA:FALSE`,
`keyUsage=digitalSignature,keyEncipherment`, and `extendedKeyUsage=serverAuth`.
The local server uses an ephemeral port and the test client still validates
the chain and hostname; it does not disable TLS verification.
