# Offline TLS fixtures

These certificates and the **public synthetic test private key** are used only by
`cfg(test)` loopback HTTPS servers and the explicitly enabled, fixed
`synthetic-https-fixture` offline test/example facility. They cannot authenticate a university domain.
The leaf SANs are moodle/hoppii/idp/webclass.synthetic.invalid. Validity is ten
years from generation in October 2026; renew these fixtures before October 2036.
The private test CA is never installed into an OS trust store. Only the test
client receives ca.der. Normal production constructors cannot add trust anchors. The fixed fixture
factory accepts no caller-supplied CA, DNS, hosts or peers and cannot reach a
university host; it uses only listeners it allocates itself.

expansion.gz is a valid gzip stream containing 2 MiB + 1 bytes of `x`. It is
rejected by Content-Encoding before any expansion, without adding a decoder.
