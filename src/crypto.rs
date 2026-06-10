//! `wandr:crypto` host backend — symmetric core (task 93 Phase A).
//!
//! Pure-Rust RustCrypto, IN-PROCESS (no binder): on aarch64 the `aes`/`sha2` crates
//! runtime-detect + use the ARMv8 Crypto Extensions (HWCAP `aes`/`sha2`), so AES
//! (GCM/CTR/CBC/CCM/CMAC) + SHA-1/2 + HMAC-SHA are hardware-accelerated; ChaCha20/
//! Poly1305 are software (fast). This module is the algorithm dispatch the WIT
//! `crypto_host_impl` resources call; it's also exercised directly by
//! `wandr-host --probe-crypto` (correctness vectors + per-algorithm throughput).
//!
//! Algorithm enums mirror `wit/crypto.wit`; the WIT Host impl maps bindgen enums to
//! these. Portable (no `cfg`) so it builds + tests on the dev box too.

#![allow(clippy::result_large_err)]

use digest::Digest;
use hmac::Mac as _;

use aes::{Aes128, Aes256};
use sha1::Sha1;
use sha2::{Sha256, Sha384, Sha512, Sha512_256};

// ── algorithm identifiers (mirror the WIT) ───────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashAlgo { Sha1, Sha256, Sha384, Sha512, Sha512_256 }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacAlgo { HmacSha1, HmacSha256, HmacSha384, HmacSha512, AesCmac, Poly1305 }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AeadAlgo { Aes128Gcm, Aes256Gcm, Aes128Ccm, Aes256Ccm, ChaCha20Poly1305, XChaCha20Poly1305 }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CipherAlgo { Aes128Ctr, Aes256Ctr, Aes128Cbc, Aes256Cbc }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KdfAlgo { HkdfSha256, HkdfSha512, Pbkdf2HmacSha256 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CryptoError {
    UnsupportedAlgorithm,
    InvalidKeyLength,
    InvalidNonceLength,
    InvalidLength,
    AuthFailed,
}

// ── HW capability detection (HWCAP via cpufeatures) ──────────────────────────

cpufeatures::new!(cpu_aes, "aes");
cpufeatures::new!(cpu_sha2, "sha2");

pub fn hw_aes() -> bool { cpu_aes::get() }
pub fn hw_sha2() -> bool { cpu_sha2::get() }
/// ARMv8-A SHA-1 and SHA-2 ship as one crypto-extension group; `cpufeatures` only
/// exposes a `sha2` aarch64 token, so we report SHA-1 HW by the same flag.
pub fn hw_sha1() -> bool { cpu_sha2::get() }

/// Constant-time byte compare (length leak only — acceptable for MAC tags).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    d == 0
}

// ── hash ─────────────────────────────────────────────────────────────────────

pub fn digest(algo: HashAlgo, data: &[u8]) -> Vec<u8> {
    match algo {
        HashAlgo::Sha1 => Sha1::digest(data).to_vec(),
        HashAlgo::Sha256 => Sha256::digest(data).to_vec(),
        HashAlgo::Sha384 => Sha384::digest(data).to_vec(),
        HashAlgo::Sha512 => Sha512::digest(data).to_vec(),
        HashAlgo::Sha512_256 => Sha512_256::digest(data).to_vec(),
    }
}

// ── mac ──────────────────────────────────────────────────────────────────────

pub fn mac_compute(algo: MacAlgo, key: &[u8], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
    // HMAC accepts any key length (long keys are hashed) → `new_from_slice` never errors.
    macro_rules! hmac { ($h:ty) => {{
        let mut m = <hmac::Hmac<$h> as hmac::Mac>::new_from_slice(key).unwrap();
        m.update(data);
        m.finalize().into_bytes().to_vec()
    }}; }
    Ok(match algo {
        MacAlgo::HmacSha1 => hmac!(Sha1),
        MacAlgo::HmacSha256 => hmac!(Sha256),
        MacAlgo::HmacSha384 => hmac!(Sha384),
        MacAlgo::HmacSha512 => hmac!(Sha512),
        MacAlgo::AesCmac => match key.len() {
            16 => { let mut m = <cmac::Cmac<Aes128> as hmac::Mac>::new_from_slice(key).unwrap(); m.update(data); m.finalize().into_bytes().to_vec() }
            32 => { let mut m = <cmac::Cmac<Aes256> as hmac::Mac>::new_from_slice(key).unwrap(); m.update(data); m.finalize().into_bytes().to_vec() }
            _ => return Err(CryptoError::InvalidKeyLength),
        },
        MacAlgo::Poly1305 => {
            if key.len() != 32 {
                return Err(CryptoError::InvalidKeyLength);
            }
            use poly1305::universal_hash::KeyInit;
            let tag = poly1305::Poly1305::new(poly1305::Key::from_slice(key)).compute_unpadded(data);
            tag.to_vec()
        }
    })
}

pub fn mac_verify(algo: MacAlgo, key: &[u8], data: &[u8], tag: &[u8]) -> bool {
    match mac_compute(algo, key, data) {
        Ok(expected) => ct_eq(&expected, tag),
        Err(_) => false,
    }
}

// ── aead ─────────────────────────────────────────────────────────────────────

use aes_gcm::aead::{Aead, KeyInit as AeadKeyInit, Payload};

type Aes128Ccm = ccm::Ccm<Aes128, ccm::consts::U16, ccm::consts::U12>;
type Aes256Ccm = ccm::Ccm<Aes256, ccm::consts::U16, ccm::consts::U12>;

fn aead_nonce_len(algo: AeadAlgo) -> usize {
    match algo {
        AeadAlgo::XChaCha20Poly1305 => 24,
        _ => 12,
    }
}

/// Build the AEAD cipher + run `op` (seal/open closures returning aead::Result).
macro_rules! aead_dispatch {
    ($algo:expr, $key:expr, $cipher:ident => $body:expr) => {{
        match $algo {
            AeadAlgo::Aes128Gcm => { let $cipher = aes_gcm::Aes128Gcm::new_from_slice($key).map_err(|_| CryptoError::InvalidKeyLength)?; $body }
            AeadAlgo::Aes256Gcm => { let $cipher = aes_gcm::Aes256Gcm::new_from_slice($key).map_err(|_| CryptoError::InvalidKeyLength)?; $body }
            AeadAlgo::Aes128Ccm => { let $cipher = <Aes128Ccm as AeadKeyInit>::new_from_slice($key).map_err(|_| CryptoError::InvalidKeyLength)?; $body }
            AeadAlgo::Aes256Ccm => { let $cipher = <Aes256Ccm as AeadKeyInit>::new_from_slice($key).map_err(|_| CryptoError::InvalidKeyLength)?; $body }
            AeadAlgo::ChaCha20Poly1305 => { let $cipher = chacha20poly1305::ChaCha20Poly1305::new_from_slice($key).map_err(|_| CryptoError::InvalidKeyLength)?; $body }
            AeadAlgo::XChaCha20Poly1305 => { let $cipher = chacha20poly1305::XChaCha20Poly1305::new_from_slice($key).map_err(|_| CryptoError::InvalidKeyLength)?; $body }
        }
    }};
}

pub fn aead_seal(algo: AeadAlgo, key: &[u8], nonce: &[u8], aad: &[u8], pt: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if nonce.len() != aead_nonce_len(algo) {
        return Err(CryptoError::InvalidNonceLength);
    }
    aead_dispatch!(algo, key, c => c.encrypt(nonce.into(), Payload { msg: pt, aad }).map_err(|_| CryptoError::InvalidLength))
}

pub fn aead_open(algo: AeadAlgo, key: &[u8], nonce: &[u8], aad: &[u8], ct: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if nonce.len() != aead_nonce_len(algo) {
        return Err(CryptoError::InvalidNonceLength);
    }
    aead_dispatch!(algo, key, c => c.decrypt(nonce.into(), Payload { msg: ct, aad }).map_err(|_| CryptoError::AuthFailed))
}

// ── raw cipher (CTR / CBC) ────────────────────────────────────────────────────

use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, StreamCipher};

pub fn cipher_encrypt(algo: CipherAlgo, key: &[u8], iv: &[u8], input: &[u8]) -> Result<Vec<u8>, CryptoError> {
    match algo {
        CipherAlgo::Aes128Ctr | CipherAlgo::Aes256Ctr => {
            let mut buf = input.to_vec();
            ctr_apply(algo, key, iv, &mut buf)?;
            Ok(buf)
        }
        CipherAlgo::Aes128Cbc => cbc::Encryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::InvalidKeyLength)?
            .encrypt_padded_vec_mut::<cbc::cipher::block_padding::Pkcs7>(input)
            .pipe(Ok),
        CipherAlgo::Aes256Cbc => cbc::Encryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::InvalidKeyLength)?
            .encrypt_padded_vec_mut::<cbc::cipher::block_padding::Pkcs7>(input)
            .pipe(Ok),
    }
}

pub fn cipher_decrypt(algo: CipherAlgo, key: &[u8], iv: &[u8], input: &[u8]) -> Result<Vec<u8>, CryptoError> {
    match algo {
        CipherAlgo::Aes128Ctr | CipherAlgo::Aes256Ctr => {
            let mut buf = input.to_vec();
            ctr_apply(algo, key, iv, &mut buf)?; // CTR: decrypt == encrypt
            Ok(buf)
        }
        CipherAlgo::Aes128Cbc => cbc::Decryptor::<Aes128>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::InvalidKeyLength)?
            .decrypt_padded_vec_mut::<cbc::cipher::block_padding::Pkcs7>(input)
            .map_err(|_| CryptoError::InvalidLength),
        CipherAlgo::Aes256Cbc => cbc::Decryptor::<Aes256>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::InvalidKeyLength)?
            .decrypt_padded_vec_mut::<cbc::cipher::block_padding::Pkcs7>(input)
            .map_err(|_| CryptoError::InvalidLength),
    }
}

fn ctr_apply(algo: CipherAlgo, key: &[u8], iv: &[u8], buf: &mut [u8]) -> Result<(), CryptoError> {
    match algo {
        CipherAlgo::Aes128Ctr => ctr::Ctr128BE::<Aes128>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::InvalidKeyLength)?
            .apply_keystream(buf),
        CipherAlgo::Aes256Ctr => ctr::Ctr128BE::<Aes256>::new_from_slices(key, iv)
            .map_err(|_| CryptoError::InvalidKeyLength)?
            .apply_keystream(buf),
        _ => return Err(CryptoError::UnsupportedAlgorithm),
    }
    Ok(())
}

// ── kdf ──────────────────────────────────────────────────────────────────────

pub fn kdf_derive(algo: KdfAlgo, ikm: &[u8], salt: &[u8], info: &[u8], iterations: u32, length: u32) -> Result<Vec<u8>, CryptoError> {
    let mut out = vec![0u8; length as usize];
    match algo {
        KdfAlgo::HkdfSha256 => hkdf::Hkdf::<Sha256>::new(Some(salt), ikm)
            .expand(info, &mut out)
            .map_err(|_| CryptoError::InvalidLength)?,
        KdfAlgo::HkdfSha512 => hkdf::Hkdf::<Sha512>::new(Some(salt), ikm)
            .expand(info, &mut out)
            .map_err(|_| CryptoError::InvalidLength)?,
        KdfAlgo::Pbkdf2HmacSha256 => {
            pbkdf2::pbkdf2_hmac::<Sha256>(ikm, salt, iterations.max(1), &mut out);
        }
    }
    Ok(out)
}

// Tiny `.pipe()` so the CBC arms read cleanly.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T { f(self) }
}
impl<T> Pipe for T {}

// ── probe (`wandr-host --probe-crypto`) ───────────────────────────────────────

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b { s.push_str(&format!("{x:02x}")); }
    s
}
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

pub fn probe() {
    use std::time::Instant;
    macro_rules! pass { ($name:expr, $cond:expr) => {{ println!("  [{}] {}", if $cond { "ok " } else { "FAIL" }, $name); }}; }

    println!("=== wandr-host --probe-crypto — symmetric core ===");
    println!("HW (ARMv8 ext via HWCAP): aes={} sha2={} sha1={}", hw_aes(), hw_sha2(), hw_sha1());

    println!("── correctness (known-answer + roundtrip) ──");
    // SHA-256("abc")
    pass!("SHA-256(\"abc\")", hex(&digest(HashAlgo::Sha256, b"abc"))
        == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    // SHA-512("abc") prefix check
    pass!("SHA-512(\"abc\")", hex(&digest(HashAlgo::Sha512, b"abc"))
        .starts_with("ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a"));
    // HMAC-SHA256 RFC 4231 test case 2 (key="Jefe", data="what do ya want for nothing?")
    pass!("HMAC-SHA256 RFC4231-2", hex(&mac_compute(MacAlgo::HmacSha256, b"Jefe", b"what do ya want for nothing?").unwrap())
        == "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    // HKDF-SHA256 RFC 5869 test case 1
    {
        let ikm = unhex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = unhex("000102030405060708090a0b0c");
        let info = unhex("f0f1f2f3f4f5f6f7f8f9");
        let okm = kdf_derive(KdfAlgo::HkdfSha256, &ikm, &salt, &info, 0, 42).unwrap();
        pass!("HKDF-SHA256 RFC5869-1", hex(&okm)
            == "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865");
    }
    // AEAD roundtrip + tamper-detect, all algos.
    for (algo, klen, name) in [
        (AeadAlgo::Aes128Gcm, 16, "AES-128-GCM"),
        (AeadAlgo::Aes256Gcm, 32, "AES-256-GCM"),
        (AeadAlgo::Aes128Ccm, 16, "AES-128-CCM"),
        (AeadAlgo::Aes256Ccm, 32, "AES-256-CCM"),
        (AeadAlgo::ChaCha20Poly1305, 32, "ChaCha20-Poly1305"),
        (AeadAlgo::XChaCha20Poly1305, 32, "XChaCha20-Poly1305"),
    ] {
        let key = vec![0x42u8; klen];
        let nonce = vec![0x24u8; aead_nonce_len(algo)];
        let aad = b"rtp-header";
        let pt = b"the quick brown fox jumps over the lazy dog";
        let ct = aead_seal(algo, &key, &nonce, aad, pt).unwrap();
        let rt = aead_open(algo, &key, &nonce, aad, &ct).map(|p| p == pt).unwrap_or(false);
        let mut bad = ct.clone();
        *bad.last_mut().unwrap() ^= 1;
        let forged = aead_open(algo, &key, &nonce, aad, &bad);
        pass!(format!("{name} seal/open + tamper-detect"), rt && forged == Err(CryptoError::AuthFailed));
    }
    // CTR + CBC roundtrip.
    for (algo, klen, name) in [
        (CipherAlgo::Aes256Ctr, 32, "AES-256-CTR"),
        (CipherAlgo::Aes256Cbc, 32, "AES-256-CBC"),
    ] {
        let key = vec![0x11u8; klen];
        let iv = vec![0x22u8; 16];
        let pt = b"raw cipher payload, sixteen+ bytes here!!".to_vec();
        let ct = cipher_encrypt(algo, &key, &iv, &pt).unwrap();
        let rt = cipher_decrypt(algo, &key, &iv, &ct).map(|p| p == pt).unwrap_or(false);
        pass!(format!("{name} roundtrip"), rt);
    }
    // MAC verify (constant-time) accept + reject.
    {
        let key = vec![0x33u8; 32];
        let tag = mac_compute(MacAlgo::HmacSha256, &key, b"msg").unwrap();
        let mut bad = tag.clone(); bad[0] ^= 1;
        pass!("HMAC-SHA256 verify accept+reject",
            mac_verify(MacAlgo::HmacSha256, &key, b"msg", &tag) && !mac_verify(MacAlgo::HmacSha256, &key, b"msg", &bad));
    }

    println!("── throughput (build cipher once, MB/s bulk + 1400B SRTP packet rate) ──");
    let buf = vec![0u8; 1 << 20]; // 1 MiB chunk
    let iters = 64u64; // 64 MiB total
    // AEAD bulk + packet-rate
    for (algo, klen, name) in [
        (AeadAlgo::Aes256Gcm, 32, "AES-256-GCM"),
        (AeadAlgo::Aes128Gcm, 16, "AES-128-GCM"),
        (AeadAlgo::ChaCha20Poly1305, 32, "ChaCha20-Poly1305"),
    ] {
        let key = vec![0x42u8; klen];
        let nonce = vec![0x24u8; aead_nonce_len(algo)];
        // bulk
        let t = Instant::now();
        for _ in 0..iters { let _ = aead_seal(algo, &key, &nonce, b"", &buf).unwrap(); }
        let mb = (iters as f64) / t.elapsed().as_secs_f64();
        // SRTP packet rate (1400-byte payload, fresh-cipher-per-packet like SRTP would key once but seal per packet)
        let pkt = vec![0u8; 1400];
        let pkts = 200_000u64;
        let t2 = Instant::now();
        for _ in 0..pkts { let _ = aead_seal(algo, &key, &nonce, b"hdr", &pkt).unwrap(); }
        let pps = (pkts as f64) / t2.elapsed().as_secs_f64();
        println!("  {name:<20} {mb:7.0} MB/s   {:.0}k pkt/s (1400B)", pps / 1000.0);
    }
    // CTR + hashes
    for (algo, name) in [(CipherAlgo::Aes256Ctr, "AES-256-CTR"), (CipherAlgo::Aes128Ctr, "AES-128-CTR")] {
        let key = vec![0x11u8; if matches!(algo, CipherAlgo::Aes256Ctr) { 32 } else { 16 }];
        let iv = vec![0x22u8; 16];
        let t = Instant::now();
        for _ in 0..iters { let _ = cipher_encrypt(algo, &key, &iv, &buf).unwrap(); }
        println!("  {name:<20} {:7.0} MB/s", (iters as f64) / t.elapsed().as_secs_f64());
    }
    for (algo, name) in [(HashAlgo::Sha256, "SHA-256"), (HashAlgo::Sha512, "SHA-512"), (HashAlgo::Sha1, "SHA-1")] {
        let t = Instant::now();
        for _ in 0..iters { let _ = digest(algo, &buf); }
        println!("  {name:<20} {:7.0} MB/s", (iters as f64) / t.elapsed().as_secs_f64());
    }
    println!("=== probe-crypto done ===");
}
