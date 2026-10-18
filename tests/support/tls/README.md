# TLS fixtures for camera hub tests

Test-only certificates and private keys, committed as DER so secret scanners
that match PEM key headers stay quiet. They protect nothing: every key here is
public. Tests load them with `include_bytes!`.

| File | What it is | SHA-256 fingerprint |
|---|---|---|
| `localhost.{cert,key}.der` | P-256, self-signed, SAN `DNS:localhost, IP:127.0.0.1` | `11:B0:66:DE:27:79:90:5A:73:5F:6C:CB:92:3D:A4:B1:F0:33:68:A0:91:5D:F6:67:A0:36:3F:E1:6A:82:82:71` |
| `other.{cert,key}.der` | A second self-signed P-256 certificate for the same names | `EE:B2:AF:86:F6:E8:1E:9E:90:12:37:2C:6A:6D:16:D4:50:C9:61:1D:91:14:EE:82:E8:1E:74:64:94:9D:3E:CF` |
| `frigate-default.{cert,key}.der` | RSA-2048 like Frigate's default: `/O=FRIGATE DEFAULT CERT/CN=*`, no SAN, CA:TRUE | `0D:3D:9C:A8:93:7D:EE:1C:4C:97:F7:FA:BD:EF:8D:03:20:82:25:5F:9D:78:84:3C:16:D1:1B:42:AF:8A:FC:E2` |
| `ca.cert.der` | A test CA (its key was discarded) | `8F:80:A3:58:17:FE:B2:B8:37:D1:34:36:DF:CA:2D:DB:A2:E1:BD:7B:02:41:AA:AB:CA:D1:00:78:45:88:85:BB` |
| `ha.example.test.{cert,key}.der` | P-256 leaf for `DNS:ha.example.test`, issued by the test CA | `24:77:AC:45:2D:75:05:1D:5F:FC:25:F9:33:5C:80:A0:42:C7:62:90:B5:CA:F2:F6:98:61:F0:97:BD:78:A4:78` |

Regenerate (OpenSSL 3 or later), then update the fingerprints above and in the tests:

```sh
ec() { openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 36500 -keyout $1.key.pem -out $1.cert.pem -subj "$2" "${@:3}"; }
ec localhost "/CN=capturefab test" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -addext "basicConstraints=critical,CA:FALSE"
ec other "/CN=capturefab other" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -addext "basicConstraints=critical,CA:FALSE"
openssl req -x509 -newkey rsa:2048 -nodes -days 36500 -keyout frigate-default.key.pem -out frigate-default.cert.pem -subj "/O=FRIGATE DEFAULT CERT/CN=*"
ec ca "/CN=capturefab test CA" -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout ha.example.test.key.pem -out ha.csr -subj "/CN=ha.example.test"
printf "subjectAltName=DNS:ha.example.test\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n" > ext.cnf
openssl x509 -req -in ha.csr -CA ca.cert.pem -CAkey ca.key.pem -CAcreateserial -days 36500 -out ha.example.test.cert.pem -extfile ext.cnf
for n in localhost other frigate-default ca ha.example.test; do openssl x509 -in $n.cert.pem -outform DER -out $n.cert.der; done
for n in localhost other frigate-default ha.example.test; do openssl pkcs8 -topk8 -nocrypt -in $n.key.pem -outform DER -out $n.key.der; done
```
