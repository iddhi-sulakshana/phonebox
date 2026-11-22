//! PKCS#12 (RFC 7292) as the Wi-Fi certificate needs it: read `lan-cert.pfx` whichever way it was written (the C#
//! agent on macOS/Linux writes 3DES bags and a SHA-1 MAC with a null password; Windows and OpenSSL may write RC2 or
//! PBES2/AES bags and a SHA-256 MAC), and write the .NET layout so the C# agent still reads a file the Rust agent made.

/// The certificate and its private key from a PKCS#12 file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pfx {
    /// The certificate's DER, as stored (its SHA-256 is the fingerprint the phone pins).
    pub cert_der: Vec<u8>,
    /// The P-256 private key as PKCS#8 DER, always with its public point.
    pub key_pkcs8: Vec<u8>,
}

use crate::der::{self, Reader};

const OID_DATA: &str = "1.2.840.113549.1.7.1";
const OID_ENCRYPTED_DATA: &str = "1.2.840.113549.1.7.6";
const OID_KEY_BAG: &str = "1.2.840.113549.1.12.10.1.1";
const OID_SHROUDED_KEY_BAG: &str = "1.2.840.113549.1.12.10.1.2";
const OID_CERT_BAG: &str = "1.2.840.113549.1.12.10.1.3";
const OID_X509_CERT: &str = "1.2.840.113549.1.9.22.1";
const OID_LOCAL_KEY_ID: &str = "1.2.840.113549.1.9.21";
const OID_PBE_3DES: &str = "1.2.840.113549.1.12.1.3";
const OID_PBE_RC2_128: &str = "1.2.840.113549.1.12.1.5";
const OID_PBE_RC2_40: &str = "1.2.840.113549.1.12.1.6";
const OID_PBES2: &str = "1.2.840.113549.1.5.13";
const OID_PBKDF2: &str = "1.2.840.113549.1.5.12";
const OID_HMAC_SHA1: &str = "1.2.840.113549.2.7";
const OID_HMAC_SHA256: &str = "1.2.840.113549.2.9";
const OID_AES128_CBC: &str = "2.16.840.1.101.3.4.1.2";
const OID_AES192_CBC: &str = "2.16.840.1.101.3.4.1.22";
const OID_AES256_CBC: &str = "2.16.840.1.101.3.4.1.42";
const OID_SHA1: &str = "1.3.14.3.2.26";
const OID_SHA256: &str = "2.16.840.1.101.3.4.2.1";

/// The iteration counts .NET writes.
const ITERATIONS: u64 = 2000;
/// More than this is a hostile or broken file, not a key store.
const MAX_ITERATIONS: u64 = 10_000_000;

/// The two ways an empty password reaches the PKCS#12 key derivation: zero-length (.NET's null password) or the
/// BMPString terminator (OpenSSL's empty password).
const CONVENTIONS: [&[u8]; 2] = [&[], &[0, 0]];

#[derive(Clone, Copy)]
enum Hash {
    Sha1,
    Sha256,
}

impl Hash {
    fn of(oid: &[u8]) -> Result<Hash, String> {
        if oid == der::oid(OID_SHA1) || oid == der::oid(OID_HMAC_SHA1) {
            Ok(Hash::Sha1)
        } else if oid == der::oid(OID_SHA256) || oid == der::oid(OID_HMAC_SHA256) {
            Ok(Hash::Sha256)
        } else {
            Err("an unsupported hash".into())
        }
    }

    fn len(self) -> usize {
        match self {
            Hash::Sha1 => 20,
            Hash::Sha256 => 32,
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            Hash::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Hash::Sha256 => sha2::Sha256::digest(data).to_vec(),
        }
    }

    fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        use hmac::Mac;
        match self {
            Hash::Sha1 => {
                let mut m = hmac::Hmac::<sha1::Sha1>::new_from_slice(key).expect("any key length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }
            Hash::Sha256 => {
                let mut m =
                    hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("any key length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }
        }
    }
}

fn iterations(value: Option<&[u8]>) -> Result<u64, String> {
    let n = value.map(der::uint).transpose()?.unwrap_or(1);
    if n == 0 || n > MAX_ITERATIONS {
        return Err("an implausible iteration count".into());
    }
    Ok(n)
}

/// RFC 7292 appendix B.2: `id` 1 for a key, 2 for an IV, 3 for a MAC key.
fn kdf(hash: Hash, password: &[u8], salt: &[u8], id: u8, rounds: u64, n: usize) -> Vec<u8> {
    let (u, v) = (hash.len(), 64);
    let fill = |src: &[u8]| -> Vec<u8> {
        if src.is_empty() {
            return vec![];
        }
        (0..v * src.len().div_ceil(v))
            .map(|i| src[i % src.len()])
            .collect()
    };
    let d = vec![id; v];
    let mut i_buf = [fill(salt), fill(password)].concat();
    let mut out = Vec::with_capacity(n);
    loop {
        let mut a = hash.digest(&[d.as_slice(), &i_buf].concat());
        for _ in 1..rounds {
            a = hash.digest(&a);
        }
        out.extend_from_slice(&a);
        if out.len() >= n {
            break;
        }
        let b: Vec<u8> = (0..v).map(|k| a[k % u]).collect();
        for block in i_buf.chunks_mut(v) {
            let mut carry = 1u16;
            for k in (0..v).rev() {
                let sum = u16::from(block[k]) + u16::from(b[k]) + carry;
                block[k] = sum as u8;
                carry = sum >> 8;
            }
        }
    }
    out.truncate(n);
    out
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the system random source");
    b
}

fn cbc_decrypt<C>(cipher: C, iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String>
where
    C: cbc::cipher::BlockDecryptMut + cbc::cipher::BlockCipher,
{
    use cbc::cipher::{BlockDecryptMut, InnerIvInit, block_padding::Pkcs7};
    cbc::Decryptor::<C>::inner_iv_slice_init(cipher, iv)
        .map_err(|_| "a wrong IV length".to_string())?
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| "the data does not decrypt (a password, or a damaged file)".to_string())
}

/// Decrypts with the scheme an AlgorithmIdentifier names; `password` is the PKCS#12 form, PBES2 gets the raw empty one.
fn decrypt(alg: &[u8], data: &[u8], password: &[u8]) -> Result<Vec<u8>, String> {
    use cbc::cipher::KeyInit;
    let mut r = Reader::new(alg);
    let oid = r.expect(der::OID)?;
    let params = r.expect(der::SEQUENCE)?;
    let pkcs12_pbe = |key_len: usize| -> Result<(Vec<u8>, Vec<u8>), String> {
        let mut p = Reader::new(params);
        let salt = p.expect(der::OCTET_STRING)?;
        let rounds = iterations(Some(p.expect(der::INTEGER)?))?;
        Ok((
            kdf(Hash::Sha1, password, salt, 1, rounds, key_len),
            kdf(Hash::Sha1, password, salt, 2, rounds, 8),
        ))
    };
    if oid == der::oid(OID_PBE_3DES) {
        let (key, iv) = pkcs12_pbe(24)?;
        let cipher = des::TdesEde3::new_from_slice(&key).map_err(|_| "a wrong key length")?;
        return cbc_decrypt(cipher, &iv, data);
    }
    if oid == der::oid(OID_PBE_RC2_40) || oid == der::oid(OID_PBE_RC2_128) {
        let bits = if oid == der::oid(OID_PBE_RC2_40) {
            40
        } else {
            128
        };
        let (key, iv) = pkcs12_pbe(bits / 8)?;
        let cipher = rc2::Rc2::new_with_eff_key_len(&key, bits);
        return cbc_decrypt(cipher, &iv, data);
    }
    if oid == der::oid(OID_PBES2) {
        let mut p = Reader::new(params);
        let mut kdf_alg = Reader::new(p.expect(der::SEQUENCE)?);
        if kdf_alg.expect(der::OID)? != der::oid(OID_PBKDF2) {
            return Err("an unsupported PBES2 key derivation".into());
        }
        let mut kp = Reader::new(kdf_alg.expect(der::SEQUENCE)?);
        let salt = kp.expect(der::OCTET_STRING)?;
        let rounds = iterations(Some(kp.expect(der::INTEGER)?))? as u32;
        kp.optional(der::INTEGER)?; // the key length follows from the cipher
        let prf = match kp.optional(der::SEQUENCE)? {
            Some(prf) => Hash::of(Reader::new(prf).expect(der::OID)?)?,
            None => Hash::Sha1,
        };
        let mut enc = Reader::new(p.expect(der::SEQUENCE)?);
        let cipher_oid = enc.expect(der::OID)?;
        let iv = enc.expect(der::OCTET_STRING)?;
        let key_len = if cipher_oid == der::oid(OID_AES128_CBC) {
            16
        } else if cipher_oid == der::oid(OID_AES192_CBC) {
            24
        } else if cipher_oid == der::oid(OID_AES256_CBC) {
            32
        } else {
            return Err("an unsupported PBES2 cipher".into());
        };
        let mut key = vec![0u8; key_len];
        // the PBES2 password is the text itself: empty
        match prf {
            Hash::Sha1 => pbkdf2::pbkdf2_hmac::<sha1::Sha1>(&[], salt, rounds, &mut key),
            Hash::Sha256 => pbkdf2::pbkdf2_hmac::<sha2::Sha256>(&[], salt, rounds, &mut key),
        }
        return match key_len {
            16 => cbc_decrypt(
                aes::Aes128::new_from_slice(&key).map_err(|_| "key")?,
                iv,
                data,
            ),
            24 => cbc_decrypt(
                aes::Aes192::new_from_slice(&key).map_err(|_| "key")?,
                iv,
                data,
            ),
            _ => cbc_decrypt(
                aes::Aes256::new_from_slice(&key).map_err(|_| "key")?,
                iv,
                data,
            ),
        };
    }
    Err("an unsupported encryption scheme".into())
}

/// The content of a `data` ContentInfo (its OCTET STRING, in one piece).
fn data_content(content_info: &[u8]) -> Result<Vec<u8>, String> {
    let mut r = Reader::new(content_info);
    if r.expect(der::OID)? != der::oid(OID_DATA) {
        return Err("not a data content".into());
    }
    let mut wrapped = Reader::new(r.expect(der::context(0, true))?);
    let t = wrapped.take()?;
    if t.tag & !0x20 != der::OCTET_STRING {
        return Err("not an octet string".into());
    }
    der::octets(&t)
}

/// Which empty-password convention the MAC was made with.
fn verify_mac(mac_data: &[u8], auth_safe: &[u8]) -> Result<&'static [u8], String> {
    let mut r = Reader::new(mac_data);
    let mut digest_info = Reader::new(r.expect(der::SEQUENCE)?);
    let hash = Hash::of(Reader::new(digest_info.expect(der::SEQUENCE)?).expect(der::OID)?)?;
    let digest = digest_info.expect(der::OCTET_STRING)?;
    let salt = r.expect(der::OCTET_STRING)?;
    let rounds = iterations(r.optional(der::INTEGER)?)?;
    for password in CONVENTIONS {
        let key = kdf(hash, password, salt, 3, rounds, hash.len());
        let mac = hash.hmac(&key, auth_safe);
        // constant time: the comparison is of a MAC over the whole file
        if mac.len() == digest.len()
            && mac
                .iter()
                .zip(digest)
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
        {
            return Ok(password);
        }
    }
    Err("the integrity check failed (a password, or a damaged file)".into())
}

fn local_key_id(attributes: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut r = Reader::new(attributes?);
    while !r.is_empty() {
        let mut attr = Reader::new(r.expect(der::SEQUENCE).ok()?);
        let oid = attr.expect(der::OID).ok()?;
        let mut values = Reader::new(attr.expect(der::SET).ok()?);
        if oid == der::oid(OID_LOCAL_KEY_ID) {
            return values.expect(der::OCTET_STRING).ok().map(<[u8]>::to_vec);
        }
    }
    None
}

type Found = Vec<(Vec<u8>, Option<Vec<u8>>)>;

fn read_safe_contents(
    contents: &[u8],
    password: &[u8],
    certs: &mut Found,
    keys: &mut Found,
) -> Result<(), String> {
    let mut top = Reader::new(contents);
    let mut bags = Reader::new(top.expect(der::SEQUENCE)?);
    while !bags.is_empty() {
        let mut bag = Reader::new(bags.expect(der::SEQUENCE)?);
        let id = bag.expect(der::OID)?;
        let mut value = Reader::new(bag.expect(der::context(0, true))?);
        let local = local_key_id(bag.optional(der::SET)?);
        if id == der::oid(OID_CERT_BAG) {
            let mut cert_bag = Reader::new(value.expect(der::SEQUENCE)?);
            if cert_bag.expect(der::OID)? != der::oid(OID_X509_CERT) {
                continue;
            }
            let mut wrapped = Reader::new(cert_bag.expect(der::context(0, true))?);
            certs.push((der::octets(&wrapped.take()?)?, local));
        } else if id == der::oid(OID_SHROUDED_KEY_BAG) {
            let mut epki = Reader::new(value.expect(der::SEQUENCE)?);
            let alg = epki.expect(der::SEQUENCE)?;
            let encrypted = epki.expect(der::OCTET_STRING)?;
            keys.push((decrypt(alg, encrypted, password)?, local));
        } else if id == der::oid(OID_KEY_BAG) {
            keys.push((value.take()?.raw.to_vec(), local));
        }
    }
    Ok(())
}

fn read_auth_safe(auth_safe: &[u8], password: &[u8]) -> Result<Pfx, String> {
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey};
    let mut top = Reader::new(auth_safe);
    let mut infos = Reader::new(top.expect(der::SEQUENCE)?);
    let (mut certs, mut keys) = (Found::new(), Found::new());
    while !infos.is_empty() {
        let info = infos.expect(der::SEQUENCE)?;
        let mut r = Reader::new(info);
        let oid = r.expect(der::OID)?;
        let contents = if oid == der::oid(OID_DATA) {
            data_content(info)?
        } else if oid == der::oid(OID_ENCRYPTED_DATA) {
            let mut wrapped = Reader::new(r.expect(der::context(0, true))?);
            let mut encrypted_data = Reader::new(wrapped.expect(der::SEQUENCE)?);
            encrypted_data.expect(der::INTEGER)?;
            let mut eci = Reader::new(encrypted_data.expect(der::SEQUENCE)?);
            eci.expect(der::OID)?;
            let alg = eci.expect(der::SEQUENCE)?;
            let content = eci.take()?;
            if content.tag & !0x20 != der::context(0, false) {
                return Err("no encrypted content".into());
            }
            decrypt(alg, &der::octets(&content)?, password)?
        } else {
            continue; // public-key encrypted contents: not written for this file
        };
        read_safe_contents(&contents, password, &mut certs, &mut keys)?;
    }
    let (key_der, key_id) = keys
        .into_iter()
        .next()
        .ok_or("the file holds no private key")?;
    let secret = p256::SecretKey::from_pkcs8_der(&key_der)
        .map_err(|_| "the private key is not a P-256 key")?;
    let public = secret
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();
    let cert = certs
        .iter()
        .position(|(c, _)| cert_public_key(c).is_ok_and(|p| p == public))
        .or_else(|| {
            certs
                .iter()
                .position(|(_, id)| id.is_some() && *id == key_id)
        })
        .or(if certs.is_empty() { None } else { Some(0) })
        .ok_or("the file holds no certificate")?;
    Ok(Pfx {
        cert_der: certs.swap_remove(cert).0,
        key_pkcs8: secret
            .to_pkcs8_der()
            .map_err(|_| "the private key cannot be encoded")?
            .as_bytes()
            .to_vec(),
    })
}

/// Reads a PKCS#12 file with an empty password (either convention).
pub fn read(bytes: &[u8]) -> Result<Pfx, String> {
    let mut top = Reader::new(bytes);
    let mut pfx = Reader::new(top.expect(der::SEQUENCE)?);
    if der::uint(pfx.expect(der::INTEGER)?)? != 3 {
        return Err("not a version 3 PKCS#12 file".into());
    }
    let auth_safe = data_content(pfx.expect(der::SEQUENCE)?)?;
    let passwords: Vec<&[u8]> = match pfx.optional(der::SEQUENCE)? {
        Some(mac) => vec![verify_mac(mac, &auth_safe)?],
        None => CONVENTIONS.to_vec(),
    };
    let mut last = String::new();
    for password in passwords {
        match read_auth_safe(&auth_safe, password) {
            Ok(found) => return Ok(found),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// `pbeWithSHAAnd3-KeyTripleDES-CBC` with a fresh salt: the AlgorithmIdentifier and the ciphertext.
fn encrypt_3des(data: &[u8], password: &[u8]) -> (Vec<u8>, Vec<u8>) {
    use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    let salt: [u8; 8] = random();
    let key = kdf(Hash::Sha1, password, &salt, 1, ITERATIONS, 24);
    let iv = kdf(Hash::Sha1, password, &salt, 2, ITERATIONS, 8);
    let encrypted = cbc::Encryptor::<des::TdesEde3>::new_from_slices(&key, &iv)
        .expect("24-byte key, 8-byte IV")
        .encrypt_padded_vec_mut::<Pkcs7>(data);
    let alg = der::seq(&[
        &der::write_oid(OID_PBE_3DES),
        &der::seq(&[&der::write_octets(&salt), &der::write_uint(ITERATIONS)]),
    ]);
    (alg, encrypted)
}

/// Writes the .NET layout: MAC HMAC-SHA1 (2000 iterations, 20-byte salt), the certificate in an EncryptedData and the
/// key in a shrouded key bag, both `pbeWithSHAAnd3-KeyTripleDES-CBC` with 2000 iterations, `localKeyID` 01 00 00 00,
/// and the null (zero-length) password.
pub fn write(pfx: &Pfx) -> Vec<u8> {
    let password: &[u8] = &[];
    let attributes = der::set(&[&der::seq(&[
        &der::write_oid(OID_LOCAL_KEY_ID),
        &der::set(&[&der::write_octets(&[1, 0, 0, 0])]),
    ])]);
    let cert_bag = der::seq(&[
        &der::write_oid(OID_CERT_BAG),
        &der::constructed(
            der::context(0, true),
            &[&der::seq(&[
                &der::write_oid(OID_X509_CERT),
                &der::constructed(der::context(0, true), &[&der::write_octets(&pfx.cert_der)]),
            ])],
        ),
        &attributes,
    ]);
    let (cert_alg, cert_encrypted) = encrypt_3des(&der::seq(&[&cert_bag]), password);
    let encrypted_data = der::seq(&[
        &der::write_uint(0),
        &der::seq(&[
            &der::write_oid(OID_DATA),
            &cert_alg,
            &der::tlv(der::context(0, false), &cert_encrypted),
        ]),
    ]);
    let certs = der::seq(&[
        &der::write_oid(OID_ENCRYPTED_DATA),
        &der::constructed(der::context(0, true), &[&encrypted_data]),
    ]);
    let (key_alg, key_encrypted) = encrypt_3des(&pfx.key_pkcs8, password);
    let key_bag = der::seq(&[
        &der::write_oid(OID_SHROUDED_KEY_BAG),
        &der::constructed(
            der::context(0, true),
            &[&der::seq(&[&key_alg, &der::write_octets(&key_encrypted)])],
        ),
        &attributes,
    ]);
    let keys = der::seq(&[
        &der::write_oid(OID_DATA),
        &der::constructed(
            der::context(0, true),
            &[&der::write_octets(&der::seq(&[&key_bag]))],
        ),
    ]);
    let auth_safe = der::seq(&[&certs, &keys]);
    let salt: [u8; 20] = random();
    let mac = Hash::Sha1.hmac(
        &kdf(Hash::Sha1, password, &salt, 3, ITERATIONS, 20),
        &auth_safe,
    );
    der::seq(&[
        &der::write_uint(3),
        &der::seq(&[
            &der::write_oid(OID_DATA),
            &der::constructed(der::context(0, true), &[&der::write_octets(&auth_safe)]),
        ]),
        &der::seq(&[
            &der::seq(&[
                &der::seq(&[&der::write_oid(OID_SHA1), &der::tlv(der::NULL, &[])]),
                &der::write_octets(&mac),
            ]),
            &der::write_octets(&salt),
            &der::write_uint(ITERATIONS),
        ]),
    ])
}

/// The uncompressed public point in a certificate's SubjectPublicKeyInfo.
pub fn cert_public_key(cert_der: &[u8]) -> Result<Vec<u8>, String> {
    let mut top = Reader::new(cert_der);
    let mut cert = Reader::new(top.expect(der::SEQUENCE)?);
    let mut tbs = Reader::new(cert.expect(der::SEQUENCE)?);
    tbs.optional(der::context(0, true))?; // version
    tbs.expect(der::INTEGER)?; // serial
    tbs.expect(der::SEQUENCE)?; // signature algorithm
    tbs.expect(der::SEQUENCE)?; // issuer
    tbs.expect(der::SEQUENCE)?; // validity
    tbs.expect(der::SEQUENCE)?; // subject
    let mut spki = Reader::new(tbs.expect(der::SEQUENCE)?);
    spki.expect(der::SEQUENCE)?;
    match spki.expect(der::BIT_STRING)? {
        [0, point @ ..] => Ok(point.to_vec()),
        _ => Err("a malformed public key".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::der;
    use sha2::Digest;

    const DOTNET: &[u8] = include_bytes!("testdata/lan/dotnet.pfx");
    const OPENSSL_RC2: &[u8] = include_bytes!("testdata/lan/openssl-rc2.pfx");
    const OPENSSL_AES: &[u8] = include_bytes!("testdata/lan/openssl-aes.pfx");
    const OPENSSL_NOPUB: &[u8] = include_bytes!("testdata/lan/openssl-nopub.pfx");
    const DOTNET_FP: &str = "2c642ac96f9bb9c03376e28e57e7971a1bf010ed3d54b0d4bc609bbdf209a461";
    const OPENSSL_FP: &str = "a66b25c96b59b9d7c7b5dbfb5ac75d45df46c03f1f2d664988d9779283b2527f";

    fn fp(der: &[u8]) -> String {
        sha2::Sha256::digest(der)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn key_matches_cert(pfx: &Pfx) {
        use p256::ecdsa::signature::{Signer, Verifier};
        use p256::pkcs8::DecodePrivateKey;
        let key = p256::ecdsa::SigningKey::from_pkcs8_der(&pfx.key_pkcs8).unwrap();
        let signature: p256::ecdsa::Signature = key.sign(b"phonebox");
        let public = cert_public_key(&pfx.cert_der).unwrap();
        let verifying = p256::ecdsa::VerifyingKey::from_sec1_bytes(&public).unwrap();
        verifying.verify(b"phonebox", &signature).unwrap();
    }

    #[test]
    fn the_dotnet_file_reads_with_its_fingerprint() {
        let pfx = read(DOTNET).unwrap();
        assert_eq!(fp(&pfx.cert_der), DOTNET_FP);
    }

    #[test]
    fn rc2_and_aes_files_read_with_their_fingerprints() {
        for file in [OPENSSL_RC2, OPENSSL_AES, OPENSSL_NOPUB] {
            assert_eq!(fp(&read(file).unwrap().cert_der), OPENSSL_FP);
        }
    }

    #[test]
    fn the_key_matches_the_certificate() {
        for file in [DOTNET, OPENSSL_RC2, OPENSSL_AES, OPENSSL_NOPUB] {
            key_matches_cert(&read(file).unwrap());
        }
    }

    #[test]
    fn a_key_without_its_public_point_gets_one() {
        // ring refuses a PKCS#8 EC key without the public key; the re-encoded key carries it ([1] in ECPrivateKey)
        let pfx = read(OPENSSL_NOPUB).unwrap();
        let mut r = der::Reader::new(&pfx.key_pkcs8);
        let mut info = der::Reader::new(r.expect(der::SEQUENCE).unwrap());
        info.expect(der::INTEGER).unwrap();
        info.expect(der::SEQUENCE).unwrap();
        let mut ec = der::Reader::new(info.expect(der::OCTET_STRING).unwrap());
        let mut ec = der::Reader::new(ec.expect(der::SEQUENCE).unwrap());
        ec.expect(der::INTEGER).unwrap();
        ec.expect(der::OCTET_STRING).unwrap();
        let mut found = false;
        while !ec.is_empty() {
            found |= ec.take().unwrap().tag == der::context(1, true);
        }
        assert!(found);
    }

    #[test]
    fn a_written_file_reads_back_and_has_the_dotnet_layout() {
        let original = read(DOTNET).unwrap();
        let written = write(&original);
        assert_eq!(read(&written).unwrap(), original);
        // PFX { 3, authSafe, MacData { DigestInfo { sha1 }, salt (20 bytes), 2000 } }
        let mut r = der::Reader::new(&written);
        let mut pfx = der::Reader::new(r.expect(der::SEQUENCE).unwrap());
        assert_eq!(der::uint(pfx.expect(der::INTEGER).unwrap()).unwrap(), 3);
        let auth = pfx.take().unwrap();
        let mut mac = der::Reader::new(pfx.expect(der::SEQUENCE).unwrap());
        let mut digest_info = der::Reader::new(mac.expect(der::SEQUENCE).unwrap());
        let mut alg = der::Reader::new(digest_info.expect(der::SEQUENCE).unwrap());
        assert_eq!(alg.expect(der::OID).unwrap(), der::oid("1.3.14.3.2.26"));
        assert_eq!(mac.expect(der::OCTET_STRING).unwrap().len(), 20);
        assert_eq!(der::uint(mac.expect(der::INTEGER).unwrap()).unwrap(), 2000);
        // both bags 3DES with 2000 iterations, both with localKeyID 01 00 00 00
        let triple_des = der::oid("1.2.840.113549.1.12.1.3");
        let raw = auth.raw;
        let count = |needle: &[u8]| raw.windows(needle.len()).filter(|w| *w == needle).count();
        let local_key_id = der::seq(&[
            &der::write_oid("1.2.840.113549.1.9.21"),
            &der::set(&[&der::write_octets(&[1, 0, 0, 0])]),
        ]);
        // the shrouded key bag sits in plain Data, so its parameters are visible; the cert bag's are in the EncryptedData
        assert!(count(&der::tlv(der::OID, &triple_des)) >= 2);
        assert!(count(&der::write_uint(2000)) >= 2);
        assert!(count(&local_key_id) >= 1);
    }

    #[test]
    fn both_empty_password_conventions_are_accepted() {
        // the .NET file uses the zero-length password, OpenSSL's "pass:" the BMP terminator 00 00
        assert!(read(DOTNET).is_ok());
        assert!(read(OPENSSL_RC2).is_ok());
    }

    #[test]
    fn a_tampered_file_fails_the_mac() {
        let mut bytes = DOTNET.to_vec();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
        assert!(read(&bytes).is_err());
    }

    #[test]
    fn garbage_and_truncated_files_are_errors_not_panics() {
        for bad in [
            &b"corrupt"[..],
            &[],
            &DOTNET[..DOTNET.len() / 2],
            &[0x30, 0x80],
            &[0x30, 0x03, 0x02, 0x01, 0x03],
        ] {
            assert!(read(bad).is_err(), "{bad:?}");
        }
    }
}
