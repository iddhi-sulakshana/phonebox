# Wi-Fi certificate fixtures (throwaway keys, never used anywhere)

| File | Made by | Layout | Certificate SHA-256 |
|---|---|---|---|
| `dotnet.pfx` | the C# agent (.NET 10, Linux) with Wi-Fi on, hostname `phonebox-fixture` | null password (zero-length), MAC HMAC-SHA1 2000, cert bag 3DES, shrouded key bag 3DES | `2c642ac96f9bb9c03376e28e57e7971a1bf010ed3d54b0d4bc609bbdf209a461` |
| `openssl-rc2.pfx` | OpenSSL 3.5 `-legacy -certpbe PBE-SHA1-RC2-40 -keypbe PBE-SHA1-3DES -macalg sha1 -passout pass:` | empty password, cert bag RC2-40 | `a66b25c96b59b9d7c7b5dbfb5ac75d45df46c03f1f2d664988d9779283b2527f` |
| `openssl-aes.pfx` | OpenSSL 3.5 `-certpbe AES-256-CBC -keypbe AES-256-CBC -macalg sha256 -passout pass:` | PBES2 / PBKDF2-HMAC-SHA256 / AES-256-CBC, MAC SHA-256 | same certificate as above |
| `openssl-nopub.pfx` | OpenSSL 3.5, key written with `openssl ec -no_public` | 3DES, the PKCS#8 key has no public point (as Windows CNG may write) | same certificate as above |
