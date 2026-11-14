//! Normalize application keys into an ephemeral, private OPC-UA certificate store.
use super::Security;
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cbc::cipher::{BlockCipher, BlockDecryptMut, KeyInit, KeyIvInit as _, block_padding::Pkcs7};
use md5::{Digest as _, Md5};
use opcua::{
    client::ClientBuilder,
    crypto::{CertificateStore, X509},
};
use rsa::{
    RsaPrivateKey,
    pkcs1::DecodeRsaPrivateKey as _,
    pkcs8::{DecodePrivateKey as _, EncodePrivateKey as _, LineEnding},
};
use std::{
    fs::{self, OpenOptions},
    io::{Read as _, Write as _},
    path::Path,
};
use tempfile::TempDir;
use x509_cert::{
    Certificate,
    der::Decode as _,
    ext::pkix::{SubjectAltName, name::GeneralName},
};
use zeroize::Zeroizing;

const MAX_FILE: u64 = 1024 * 1024;

fn unavailable() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Unavailable,
        "OPC-UA certificate or private key setup failed",
    )
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| unavailable())?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    if !metadata.is_file() || metadata.len() > MAX_FILE {
        return Err(unavailable());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(unavailable());
    }
    Ok(bytes)
}

fn certificate(bytes: &[u8]) -> Result<X509> {
    X509::from_der(bytes)
        .or_else(|_| X509::from_pem(bytes))
        .map_err(|_| unavailable())
}

fn application_uri(der: &[u8]) -> Result<String> {
    let cert = Certificate::from_der(der).map_err(|_| unavailable())?;
    let names = cert
        .tbs_certificate
        .get::<SubjectAltName>()
        .map_err(|_| unavailable())?;
    Ok(names
        .and_then(|(_, names)| {
            names.0.into_iter().find_map(|name| match name {
                GeneralName::UniformResourceIdentifier(uri) => Some(uri.as_str().to_owned()),
                _ => None,
            })
        })
        .unwrap_or_else(|| "urn:capturefab:opcua".into()))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| unavailable())?;
    file.write_all(bytes).map_err(|_| unavailable())
}

pub(super) fn configure(
    builder: ClientBuilder,
    security: &Security,
) -> Result<(ClientBuilder, TempDir, Vec<u8>)> {
    let password = security
        .private_key_password_env
        .as_ref()
        .map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .map(Zeroizing::new)
                .ok_or_else(|| {
                    Categorized::error(
                        ErrorKind::AuthRequired,
                        "OPC-UA credential environment variable is missing or empty",
                    )
                })
        })
        .transpose()?;
    let own_certificate = certificate(&read_file(&security.certificate)?)?
        .to_der()
        .map_err(|_| unavailable())?;
    let application_uri = application_uri(&own_certificate)?;
    let server_certificate = certificate(&read_file(&security.server_certificate)?)?;
    let pinned_der = server_certificate.to_der().map_err(|_| unavailable())?;
    let input = Zeroizing::new(read_file(&security.private_key)?);
    let key = private_key(&input, password.as_ref().map(|value| value.as_bytes()))
        .map_err(|()| unavailable())?;
    let pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|_| unavailable())?;
    let mut temporary = tempfile::Builder::new();
    temporary.prefix("capturefab-opcua-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary.permissions(fs::Permissions::from_mode(0o700));
    }
    let pki = temporary.tempdir().map_err(|_| unavailable())?;
    let trusted = pki.path().join("trusted");
    fs::create_dir(&trusted).map_err(|_| unavailable())?;
    let trusted_name = CertificateStore::cert_file_name(&server_certificate);
    if Path::new(&trusted_name).file_name() != Some(trusted_name.as_ref()) {
        return Err(unavailable());
    }
    write_private(&trusted.join(trusted_name), &pinned_der)?;
    let cert_path = pki.path().join("client.der");
    let key_path = pki.path().join("client.pem");
    write_private(&cert_path, &own_certificate)?;
    write_private(&key_path, pem.as_bytes())?;
    Ok((
        builder
            .application_uri(application_uri)
            .pki_dir(pki.path().to_path_buf())
            .certificate_path(cert_path)
            .private_key_path(key_path)
            .create_sample_keypair(false)
            .trust_server_certs(false)
            .verify_server_certs(true),
        pki,
        pinned_der,
    ))
}

fn private_key(bytes: &[u8], password: Option<&[u8]>) -> std::result::Result<RsaPrivateKey, ()> {
    if let Ok(pem) = std::str::from_utf8(bytes) {
        if pem
            .lines()
            .any(|line| line.trim() == "Proc-Type: 4,ENCRYPTED")
        {
            return traditional_key(pem, password.ok_or(())?);
        }
        if let Ok(key) = RsaPrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem).map_err(pkcs8::Error::from))
        {
            return Ok(key);
        }
        if let Some(password) = password {
            return RsaPrivateKey::from_pkcs8_encrypted_pem(pem, password).map_err(|_| ());
        }
    }
    if let Ok(key) = RsaPrivateKey::from_pkcs8_der(bytes)
        .or_else(|_| RsaPrivateKey::from_pkcs1_der(bytes).map_err(pkcs8::Error::from))
    {
        return Ok(key);
    }
    RsaPrivateKey::from_pkcs8_encrypted_der(bytes, password.ok_or(())?).map_err(|_| ())
}

/// Older OpenSSL PEM envelopes use the file IV's first eight bytes as the
/// `EVP_BytesToKey` salt. This compatibility path never chooses these ciphers
/// for an OPC-UA connection or for writing a key.
fn traditional_key(pem: &str, password: &[u8]) -> std::result::Result<RsaPrivateKey, ()> {
    let mut lines = pem.lines();
    if lines.next().map(str::trim) != Some("-----BEGIN RSA PRIVATE KEY-----") {
        return Err(());
    }
    let mut dek = None;
    let mut encrypted = false;
    for line in lines.by_ref() {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if line == "Proc-Type: 4,ENCRYPTED" && !encrypted {
            encrypted = true;
        } else if let Some(value) = line.strip_prefix("DEK-Info: ")
            && dek.is_none()
        {
            dek = Some(value);
        } else {
            return Err(());
        }
    }
    if !encrypted {
        return Err(());
    }
    let (cipher, iv) = dek.ok_or(())?.split_once(',').ok_or(())?;
    let (key_size, iv_size) = match cipher {
        "AES-128-CBC" => (16, 16),
        "AES-192-CBC" => (24, 16),
        "AES-256-CBC" => (32, 16),
        "DES-EDE3-CBC" => (24, 8),
        _ => return Err(()),
    };
    if iv.len() != iv_size * 2 || !iv.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(());
    }
    let iv = iv
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).map_err(|_| ())?, 16).map_err(|_| ())
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut body = String::new();
    let mut ended = false;
    for line in lines.by_ref() {
        let line = line.trim();
        if line == "-----END RSA PRIVATE KEY-----" {
            ended = true;
            break;
        }
        body.push_str(line);
    }
    if !ended || lines.any(|line| !line.trim().is_empty()) {
        return Err(());
    }
    let mut encrypted = Zeroizing::new(STANDARD.decode(body).map_err(|_| ())?);
    let key = traditional_password_key(password, iv.get(..8).ok_or(())?, key_size);
    let decrypted = match cipher {
        "AES-128-CBC" => decrypt::<aes::Aes128>(&key, &iv, &mut encrypted),
        "AES-192-CBC" => decrypt::<aes::Aes192>(&key, &iv, &mut encrypted),
        "AES-256-CBC" => decrypt::<aes::Aes256>(&key, &iv, &mut encrypted),
        "DES-EDE3-CBC" => decrypt::<des::TdesEde3>(&key, &iv, &mut encrypted),
        _ => return Err(()),
    }?;
    RsaPrivateKey::from_pkcs1_der(decrypted).map_err(|_| ())
}

fn traditional_password_key(password: &[u8], salt: &[u8], size: usize) -> Zeroizing<Vec<u8>> {
    let mut key = Zeroizing::new(Vec::with_capacity(size + 16));
    let mut previous = Zeroizing::new(Vec::new());
    while key.len() < size {
        let mut digest = Md5::new();
        digest.update(&previous);
        digest.update(password);
        digest.update(salt);
        *previous = digest.finalize().to_vec();
        key.extend_from_slice(&previous);
    }
    key.truncate(size);
    key
}

fn decrypt<'a, C>(key: &[u8], iv: &[u8], bytes: &'a mut [u8]) -> std::result::Result<&'a [u8], ()>
where
    C: BlockCipher + BlockDecryptMut + KeyInit,
{
    cbc::Decryptor::<C>::new_from_slices(key, iv)
        .map_err(|_| ())?
        .decrypt_padded_mut::<Pkcs7>(bytes)
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::super::{Policy, Security};
    use super::{MAX_FILE, certificate, configure, private_key, read_file};
    use crate::types::{Categorized, ErrorKind};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use opcua::{client::ClientBuilder, crypto::CertificateStore};
    use rsa::{
        pkcs1::EncodeRsaPrivateKey as _,
        pkcs8::{EncodePrivateKey as _, LineEnding},
    };
    use std::{fs, io::Write as _};

    // Fixed, disposable 1024-bit fixture keys generated with OpenSSL. No
    // executable is needed to read these formats or to run these tests.
    const AES_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-256-CBC,F3A11E6F8A82435CC8101F069F14DF38

34bSMCga/66/kz8gvlPvrxi/CXFOPH5ZgFB9wHn7rMUpMg3wwF2tPKRvW+hmqoKb
IPuwSpyiDUrb3V/ua5yz5rfQMD+14kRMNjhp/QZUdofIuIHskzX0Yo0cytTtF1J2
LuMBR8+2NRCTniNMcXn3t1B7tJ+wcH5/jh2ui/TuLWRXRz7iF/3z25+989I14Jx5
QFO09/nC+2btwNajzCRVZdQen6pRCCBbujQZRaXfijd1kU/m/1CQwPVSbdF17gJ0
77neMJJUZJXnqJmoo1t1Ews3WS0yo5G6BE5RGb4XsefJj6KAinSXWltaxShYHESq
wgsTXnlhRshG8vvU/hErTD2d50oakaZ57SNILtmFQsljrebdtqEI5IUNyP3YDk9A
hAFQswL+1H24ZPoO1iiaC/P3YZtRQTNYJ56HzZ/fZ6ita6vXrcD1LjVVL1pbj+gH
lq83vNrDvRrMkCUgQB9xhKY/DW+kqL76FXCs/7YC2/2VZwFU7kREAKWIJR0oEM8D
vaReTduU/R1dXhP4vLPBOLDTBDRxrJphhfeRaGLwgcD+1Lktab58XeGduAny9bWF
LisdQCwuGfv7eSpOjmqFwD0WnkQCT6tYZHXPrbIx6IScF1m6tiNPFqEDhEX6lXhc
EyxBvh9mneougm3uzOt5viO72HlXVXNSnvXxpUa9rPomXSgoOkwGyrBZvN6Z4bP9
0dXYSQgUh5VreMgsZsWB3GpFy1Xib65lOquiWfQ5T0c7zAa0SG7fjSREm5C65ZRV
nf5NFNYhoS2xtbqMTYvJKWNlH93enU4cpALzWL2ucsUZhmm728IEPkPtAGE0K4CT
-----END RSA PRIVATE KEY-----";

    const DES_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: DES-EDE3-CBC,8243A7EA75A3F537

D5f2detTOfXfnvn3k++hfcaJQIzjBSrZmm+2hEfI1fR3K2aQG3XJoUNFiRmONvQ0
ixjkD8Oyd/J7hZwNmPHJr4WPa92Nhh4H8RVPBisYOu2wJW/uzZ/a77CDL9iFNzeN
mhNYGrf756/ys2N9aXuAwKhg76quWPqFUAFAswbmuK6jw+xVQ95K1qBUPGAwjd/A
spROgh7lB1OSeBSHf0AQ0HvCQTvibqxloVj0YbPNHuPOOMKoW/lmOk1PxFbtaN84
M5nEn66akjzCo3+VgB2atx//aXBNzpsfnD78bSGrQ/DfuSSaqvnXfPEOkqx+011a
tMz8iG7VyurY419O4AFIJ8VwpSOGlcmNINt5anal5HLUDfp1Xhv73XW3pzdIqeKD
6BMFGBTv3DC6HGziigKm9cy9Y8kM39m2URCF0ElX9bouko0hetaSws5jsdQhY2bQ
EzUqRy2jUfyzH8e4tnjPyJpbDzNXkobDx7bOTo5Ke/90cQkHfasW87iQ6b4uAlGi
aIXv57twtpQrh9m9lTKnSxUCLIR7aLMv793tGL1QZl0lWusgGG6kqMqkSJs3e0Mx
Ps/o9JTZeqPeCGOuogHD47cBp4G36dUdgq8vWzbPoDUvKlyjGjOLJ+/adVpzVT/b
HhJW2M7tuLZr1A4bgkw7pqdqTqL6BjXJeB1FDQLptAll43JRTHcnqk2TrIeuk5vN
yjyVK84G3960H0TBJ0wsLOarzZblhc48OnwgQ3TrWs+XxMeZ5LywarmygO96N3or
7xL1PM28UvUSNbng7Pbfmi9bHrE4fPmfULks6QjJXKTUmi1yR930Iw==
-----END RSA PRIVATE KEY-----";

    const PKCS8_KEY: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----
MIIC5TBfBgkqhkiG9w0BBQ0wUjAxBgkqhkiG9w0BBQwwJAQQUc5K5cethfvsaR3C
FBpV1AICCAAwDAYIKoZIhvcNAgkFADAdBglghkgBZQMEASoEEJuJ3K4HJHOfs4AL
lsufJLIEggKAZIaGPVwsLCUiiU8sLXh8uS0FpukysHjC+KUGw2hatW9biaQIAM8Q
aGRH8tPpC4pnbiIOmh78noGJyesU6jYtMcllinZgk6k0O25W3PFHVxE9rUVu2h4C
tokUo/xg1zGBuuBDaa62gPdRE7tYdQyUMSnTyonGlR/7wl8HEuWB6lOwD3HMjqcE
YEctp9wj8D4+bawDzNDQ8r1MANt6tRUGgZHtmO0WAd1oSgp1S1y00qnoH7MXb0E+
/YEKOGA1a4gD2uA5pwYp6z/FiPO7iMbMESmmoLA8pnc+tEI6hAi0fItnLWSMdVAG
rAAx5C4PzumcQrC1acNQyVSkPode0D4GeH2ycsVUjHDpGfOLpC/Y8mavnfgyBirQ
WBHuq1mvqeGsFdiHcdK7aLgeJVpfEVfjq0OB/IGcRkYXet1Qx/Zk5Dt0wbK+xkNw
f6neER/Kp8VXpR9Yq4yX6ZfZTCKd2AHIMKkIPLg/VdIDFc6OchiER36w6wxXfw6F
42LygKDOId/t7L1BMRu96SxbzRw97iVtBa3tJBbI49rk0dXvLYJst+KND+KgejbJ
4ydiznEXlgNHF/dMbBo1M2GfeUKJ335S3mNBapiY9JUoQTgBIs3qwctuJ4QJojNH
Rdhex23PgYX7zv+zRaLMJrSALAUl9Jx7BxytXkVBPkYBTbgKyGyYYSGtBAMQcf0J
tHujFqE+FUdByXuGVyQAhWiRR/gaxIlGkUoz5+f+nFzcyAgCpQjz1aB9Fpkl6Nf4
AxO01/x6kF8BUnLz+bNewhsnsYKTGwnq4mwoQsn3bd5qLI/lu+42DmLUhrsBMiGw
NCRAzMCaX7HD+VVvrUUWuzpEQQmyCy4t9A==
-----END ENCRYPTED PRIVATE KEY-----";

    const CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIIB9TCCAV6gAwIBAgIUKWeYO/I+JNmmkYr6Bt+LvFwCf2AwDQYJKoZIhvcNAQEL
BQAwHTEbMBkGA1UEAwwSQ2FwdHVyZWZhYi1maXh0dXJlMB4XDTI2MTExMDE3MDEw
OFoXDTM2MTEwNzE3MDEwOFowHTEbMBkGA1UEAwwSQ2FwdHVyZWZhYi1maXh0dXJl
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQCu+11TF+3yhbwR83ck9WM1WKwj
DRR9XxwEEfNK6xCDUcAzOg2qLSQNaaFy9YijFarxUSlZnHxtz2z9MWPLAY6iF4JN
DgME78+SOGQCNfID0b0MA7T/8AGP7sAIard6uGGGkxFpF2B76jMSIV6mPUQ+G8OD
H0zJmZG0aXsOJs0FvwIDAQABozIwMDAdBgNVHQ4EFgQU/cOXEM1h+aPQbADfm70J
s9ZAYrQwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOBgQB2xCfMmx5S
vkDfNZ3aqHYhmZK5Wc5FL5oRJzEFqkI3A7qpt5xe89DSZ2Cc5q/qRjrYu/rAdrat
MYjiT59RBBg+LSOi2HE4IMcYAklLW3WOPf857nWeymNuFUm15tNpJFjwcXKW9hqn
vGIHVabJZYC2zXYyJnZCkeqzjX8M2n+8eg==
-----END CERTIFICATE-----";

    const URI_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIICMDCCAZmgAwIBAgIUS7ys8EPi94DKvLHvOibRFs7Wn54wDQYJKoZIhvcNAQEL
BQAwHTEbMBkGA1UEAwwSQ2FwdHVyZWZhYi1maXh0dXJlMB4XDTI2MTExMDE3MDQx
MloXDTM2MTEwNzE3MDQxMlowHTEbMBkGA1UEAwwSQ2FwdHVyZWZhYi1maXh0dXJl
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQCu+11TF+3yhbwR83ck9WM1WKwj
DRR9XxwEEfNK6xCDUcAzOg2qLSQNaaFy9YijFarxUSlZnHxtz2z9MWPLAY6iF4JN
DgME78+SOGQCNfID0b0MA7T/8AGP7sAIard6uGGGkxFpF2B76jMSIV6mPUQ+G8OD
H0zJmZG0aXsOJs0FvwIDAQABo20wazAdBgNVHQ4EFgQU/cOXEM1h+aPQbADfm70J
s9ZAYrQwDwYDVR0TAQH/BAUwAwEB/zA5BgNVHREEMjAwhiN1cm46Y2FwdHVyZWZh
YjpleGlzdGluZy1hcHBsaWNhdGlvboIJbG9jYWxob3N0MA0GCSqGSIb3DQEBCwUA
A4GBAJfCimfQxZjfunu0knVldp4IBt75df0AQl01e0P6omkw0QyoQdXZ7Z7B+HFJ
gr3UJEXUPOoCyPUjixErllxoO5j+5eIMmh5GrokEnLY2ZmY8PqeNTDSgHLf2MWWC
xPTPfIYkXR73cHl6dCYON6TZtqxEzpcBp1IfcoAN3nU8sj+z
-----END CERTIFICATE-----";

    const PASSWORD: &[u8] = b"fixture-password";

    #[test]
    fn encrypted_pkcs8_and_traditional_keys_decode_identically() {
        let expected = private_key(AES_KEY.as_bytes(), Some(PASSWORD)).unwrap();
        for encoded in [DES_KEY, PKCS8_KEY] {
            assert_eq!(
                private_key(encoded.as_bytes(), Some(PASSWORD)).unwrap(),
                expected
            );
            private_key(encoded.as_bytes(), Some(b"wrong-password")).unwrap_err();
            private_key(encoded.as_bytes(), None).unwrap_err();
        }
        let der = STANDARD
            .decode(
                PKCS8_KEY
                    .lines()
                    .filter(|line| !line.starts_with('-'))
                    .collect::<String>(),
            )
            .unwrap();
        assert_eq!(private_key(&der, Some(PASSWORD)).unwrap(), expected);
        private_key(&der, Some(b"wrong-password")).unwrap_err();
        private_key(AES_KEY.as_bytes(), Some(b"wrong-password")).unwrap_err();
        private_key(AES_KEY.as_bytes(), None).unwrap_err();
        private_key(
            AES_KEY
                .replace("DEK-Info: AES-256-CBC,", "DEK-Info: AES-256-CBC,é")
                .as_bytes(),
            Some(PASSWORD),
        )
        .unwrap_err();
    }

    #[test]
    fn unencrypted_pkcs1_and_pkcs8_accept_pem_and_der() {
        let key = private_key(AES_KEY.as_bytes(), Some(PASSWORD)).unwrap();
        let pkcs1_pem = key.to_pkcs1_pem(LineEnding::LF).unwrap();
        let pkcs8_pem = key.to_pkcs8_pem(LineEnding::LF).unwrap();
        let pkcs1_der = key.to_pkcs1_der().unwrap();
        let pkcs8_der = key.to_pkcs8_der().unwrap();
        for bytes in [
            pkcs1_pem.as_bytes(),
            pkcs8_pem.as_bytes(),
            pkcs1_der.as_bytes(),
            pkcs8_der.as_bytes(),
        ] {
            assert_eq!(private_key(bytes, None).unwrap(), key);
        }
    }

    fn files() -> (tempfile::TempDir, Security) {
        let dir = tempfile::tempdir().unwrap();
        let key = private_key(AES_KEY.as_bytes(), Some(PASSWORD)).unwrap();
        let key_path = dir.path().join("client.pem");
        let cert_path = dir.path().join("client.pem.crt");
        fs::write(
            &key_path,
            key.to_pkcs8_pem(LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap();
        fs::write(&cert_path, CERTIFICATE).unwrap();
        let security = Security {
            policy: Policy::Basic256Sha256,
            certificate: cert_path.clone(),
            private_key: key_path,
            server_certificate: cert_path,
            private_key_password_env: None,
        };
        (dir, security)
    }

    #[test]
    fn certificate_store_contains_only_the_exact_pin_and_private_normalized_key() {
        let (_source, security) = files();
        let (builder, pki, pin) = configure(ClientBuilder::new(), &security).unwrap();
        let expected = certificate(CERTIFICATE.as_bytes()).unwrap();
        assert_eq!(pin, expected.to_der().unwrap());
        let config = serde_json::to_value(builder.config()).unwrap();
        assert_eq!(config["trust_server_certs"], false);
        assert_eq!(config["verify_server_certs"], true);
        assert_eq!(config["application_uri"], "urn:capturefab:opcua");
        assert_eq!(
            fs::read(
                pki.path()
                    .join("trusted")
                    .join(CertificateStore::cert_file_name(&expected))
            )
            .unwrap(),
            pin
        );
        assert_eq!(fs::read_dir(pki.path().join("trusted")).unwrap().count(), 1);
        assert_eq!(fs::read(pki.path().join("client.der")).unwrap(), pin);
        let normalized = fs::read(pki.path().join("client.pem")).unwrap();
        assert_eq!(
            private_key(&normalized, None).unwrap(),
            private_key(AES_KEY.as_bytes(), Some(PASSWORD)).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(pki.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(pki.path().join("client.pem"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let path = pki.path().to_path_buf();
        drop(pki);
        assert!(!path.exists());
    }

    #[test]
    fn missing_password_is_typed_and_file_errors_are_sanitized() {
        let (_source, mut security) = files();
        security.private_key_password_env =
            Some("CAPTUREFAB_TEST_MISSING_OPCUA_PASSWORD_E01C47D4".into());
        let Err(error) = configure(ClientBuilder::new(), &security) else {
            panic!("missing password was accepted");
        };
        assert!(matches!(
            error.downcast_ref::<Categorized>().unwrap().kind,
            ErrorKind::AuthRequired
        ));
        security.private_key_password_env = None;
        fs::write(&security.private_key, b"private-credential-content").unwrap();
        let Err(error) = configure(ClientBuilder::new(), &security) else {
            panic!("malformed key was accepted");
        };
        assert_eq!(
            format!("{error:#}"),
            "OPC-UA certificate or private key setup failed"
        );
    }

    #[test]
    fn existing_client_certificate_application_uri_is_preserved() {
        let (_source, security) = files();
        fs::write(&security.certificate, URI_CERTIFICATE).unwrap();
        let (builder, _pki, _pin) = configure(ClientBuilder::new(), &security).unwrap();
        let config = serde_json::to_value(builder.config()).unwrap();
        assert_eq!(
            config["application_uri"],
            "urn:capturefab:existing-application"
        );
    }

    #[test]
    fn files_are_bounded_before_their_contents_are_allocated() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"secret").unwrap();
        file.as_file().set_len(MAX_FILE + 1).unwrap();
        assert_eq!(
            format!("{:#}", read_file(file.path()).unwrap_err()),
            "OPC-UA certificate or private key setup failed"
        );
    }
}
