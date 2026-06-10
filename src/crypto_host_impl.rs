//! `wandr:crypto` host impl (task 93 Phase A) — wraps the `crypto.rs` symmetric
//! backend in the WIT resources. Keyed contexts are host resources stored in
//! `HostState.table`; per-op the method looks up the context + calls the backend
//! (RustCrypto, HW AES/GHASH). Asymmetric (`signatures`/`key-exchange`) is stubbed
//! `unsupported` for Phase A.

use wasmtime::component::Resource;

use crate::crypto;
use crate::crypto_host_bindings::wandr::crypto as wit;
use crate::HostState;
use digest::Digest;

// ── resource backing structs (mapped via bindgen `with`) ─────────────────────

pub struct HasherState(pub Box<dyn digest::DynDigest + Send>);
pub struct MacKeyState {
    pub algo: crypto::MacAlgo,
    pub key: Vec<u8>,
}
pub struct AeadKeyState {
    pub algo: crypto::AeadAlgo,
    pub key: Vec<u8>,
}
pub struct CipherKeyState {
    pub algo: crypto::CipherAlgo,
    pub key: Vec<u8>,
}

// ── enum conversions (WIT bindgen ↔ crypto.rs) ───────────────────────────────

fn hash2c(a: wit::types::HashAlgo) -> crypto::HashAlgo {
    use wit::types::HashAlgo as W;
    match a {
        W::Sha1 => crypto::HashAlgo::Sha1,
        W::Sha256 => crypto::HashAlgo::Sha256,
        W::Sha384 => crypto::HashAlgo::Sha384,
        W::Sha512 => crypto::HashAlgo::Sha512,
        W::Sha512256 => crypto::HashAlgo::Sha512_256,
    }
}
fn mac2c(a: wit::types::MacAlgo) -> crypto::MacAlgo {
    use wit::types::MacAlgo as W;
    match a {
        W::HmacSha1 => crypto::MacAlgo::HmacSha1,
        W::HmacSha256 => crypto::MacAlgo::HmacSha256,
        W::HmacSha384 => crypto::MacAlgo::HmacSha384,
        W::HmacSha512 => crypto::MacAlgo::HmacSha512,
        W::AesCmac => crypto::MacAlgo::AesCmac,
        W::Poly1305 => crypto::MacAlgo::Poly1305,
    }
}
fn aead2c(a: wit::types::AeadAlgo) -> crypto::AeadAlgo {
    use wit::types::AeadAlgo as W;
    match a {
        W::Aes128Gcm => crypto::AeadAlgo::Aes128Gcm,
        W::Aes256Gcm => crypto::AeadAlgo::Aes256Gcm,
        W::Aes128Ccm => crypto::AeadAlgo::Aes128Ccm,
        W::Aes256Ccm => crypto::AeadAlgo::Aes256Ccm,
        W::Chacha20Poly1305 => crypto::AeadAlgo::ChaCha20Poly1305,
        W::Xchacha20Poly1305 => crypto::AeadAlgo::XChaCha20Poly1305,
    }
}
fn cipher2c(a: wit::types::CipherAlgo) -> crypto::CipherAlgo {
    use wit::types::CipherAlgo as W;
    match a {
        W::Aes128Ctr => crypto::CipherAlgo::Aes128Ctr,
        W::Aes256Ctr => crypto::CipherAlgo::Aes256Ctr,
        W::Aes128Cbc => crypto::CipherAlgo::Aes128Cbc,
        W::Aes256Cbc => crypto::CipherAlgo::Aes256Cbc,
    }
}
fn kdf2c(a: wit::types::KdfAlgo) -> crypto::KdfAlgo {
    use wit::types::KdfAlgo as W;
    match a {
        W::HkdfSha256 => crypto::KdfAlgo::HkdfSha256,
        W::HkdfSha512 => crypto::KdfAlgo::HkdfSha512,
        W::Pbkdf2HmacSha256 => crypto::KdfAlgo::Pbkdf2HmacSha256,
    }
}
fn err2w(e: crypto::CryptoError) -> wit::types::CryptoError {
    use wit::types::CryptoError as W;
    match e {
        crypto::CryptoError::UnsupportedAlgorithm => W::UnsupportedAlgorithm,
        crypto::CryptoError::InvalidKeyLength => W::InvalidKeyLength,
        crypto::CryptoError::InvalidNonceLength => W::InvalidNonceLength,
        crypto::CryptoError::InvalidLength => W::InvalidLength,
        crypto::CryptoError::AuthFailed => W::AuthFailed,
    }
}

/// Expected key length for an AEAD (so `create` rejects a bad key up front).
fn aead_key_len(a: crypto::AeadAlgo) -> usize {
    match a {
        crypto::AeadAlgo::Aes128Gcm | crypto::AeadAlgo::Aes128Ccm => 16,
        _ => 32,
    }
}

// ── interface Host markers (no free funcs) + the real ones ───────────────────

impl wit::types::Host for HostState {}

impl wit::caps::Host for HostState {
    fn hashes(&mut self) -> Vec<(wit::types::HashAlgo, bool)> {
        use wit::types::HashAlgo::*;
        let hw = crypto::hw_sha2();
        vec![(Sha1, crypto::hw_sha1()), (Sha256, hw), (Sha384, hw), (Sha512, hw), (Sha512256, hw)]
    }
    fn macs(&mut self) -> Vec<(wit::types::MacAlgo, bool)> {
        use wit::types::MacAlgo::*;
        let s = crypto::hw_sha2();
        vec![(HmacSha1, crypto::hw_sha1()), (HmacSha256, s), (HmacSha384, s), (HmacSha512, s),
             (AesCmac, crypto::hw_aes()), (Poly1305, false)]
    }
    fn aeads(&mut self) -> Vec<(wit::types::AeadAlgo, bool)> {
        use wit::types::AeadAlgo::*;
        let a = crypto::hw_aes();
        vec![(Aes128Gcm, a), (Aes256Gcm, a), (Aes128Ccm, a), (Aes256Ccm, a),
             (Chacha20Poly1305, false), (Xchacha20Poly1305, false)]
    }
    fn ciphers(&mut self) -> Vec<(wit::types::CipherAlgo, bool)> {
        use wit::types::CipherAlgo::*;
        let a = crypto::hw_aes();
        vec![(Aes128Ctr, a), (Aes256Ctr, a), (Aes128Cbc, a), (Aes256Cbc, a)]
    }
    fn kdfs(&mut self) -> Vec<(wit::types::KdfAlgo, bool)> {
        use wit::types::KdfAlgo::*;
        let s = crypto::hw_sha2();
        vec![(HkdfSha256, s), (HkdfSha512, s), (Pbkdf2HmacSha256, s)]
    }
    fn sigs(&mut self) -> Vec<wit::types::SigAlgo> {
        Vec::new() // Phase B
    }
    fn kxs(&mut self) -> Vec<wit::types::KxAlgo> {
        Vec::new() // Phase B
    }
}

impl wit::hash::Host for HostState {
    fn digest(&mut self, algo: wit::types::HashAlgo, data: Vec<u8>) -> Vec<u8> {
        crypto::digest(hash2c(algo), &data)
    }
}

impl wit::hash::HostHasher for HostState {
    fn create(&mut self, algo: wit::types::HashAlgo) -> Resource<HasherState> {
        let h: Box<dyn digest::DynDigest + Send> = match hash2c(algo) {
            crypto::HashAlgo::Sha1 => Box::new(sha1::Sha1::new()),
            crypto::HashAlgo::Sha256 => Box::new(sha2::Sha256::new()),
            crypto::HashAlgo::Sha384 => Box::new(sha2::Sha384::new()),
            crypto::HashAlgo::Sha512 => Box::new(sha2::Sha512::new()),
            crypto::HashAlgo::Sha512_256 => Box::new(sha2::Sha512_256::new()),
        };
        self.table.push(HasherState(h)).expect("resource table push")
    }
    fn update(&mut self, self_: Resource<HasherState>, data: Vec<u8>) {
        if let Ok(st) = self.table.get_mut(&self_) {
            st.0.update(&data);
        }
    }
    fn finish(&mut self, self_: Resource<HasherState>) -> Vec<u8> {
        match self.table.get_mut(&self_) {
            Ok(st) => st.0.finalize_reset().to_vec(),
            Err(_) => Vec::new(),
        }
    }
    fn drop(&mut self, rep: Resource<HasherState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wit::mac::Host for HostState {}
impl wit::mac::HostMacKey for HostState {
    fn create(&mut self, algo: wit::types::MacAlgo, key: Vec<u8>) -> Result<Resource<MacKeyState>, wit::types::CryptoError> {
        let a = mac2c(algo);
        // Validate the key by computing once (cheap, catches bad CMAC/Poly key lengths).
        crypto::mac_compute(a, &key, b"").map_err(err2w)?;
        self.table.push(MacKeyState { algo: a, key }).map_err(|_| wit::types::CryptoError::InvalidLength)
    }
    fn compute(&mut self, self_: Resource<MacKeyState>, data: Vec<u8>) -> Vec<u8> {
        // The key was validated at `create`, so this can't fail in practice.
        match self.table.get(&self_) {
            Ok(st) => crypto::mac_compute(st.algo, &st.key, &data).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }
    fn verify(&mut self, self_: Resource<MacKeyState>, data: Vec<u8>, tag: Vec<u8>) -> bool {
        match self.table.get(&self_) {
            Ok(st) => crypto::mac_verify(st.algo, &st.key, &data, &tag),
            Err(_) => false,
        }
    }
    fn drop(&mut self, rep: Resource<MacKeyState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wit::aead::Host for HostState {}
impl wit::aead::HostAeadKey for HostState {
    fn create(&mut self, algo: wit::types::AeadAlgo, key: Vec<u8>) -> Result<Resource<AeadKeyState>, wit::types::CryptoError> {
        let a = aead2c(algo);
        if key.len() != aead_key_len(a) {
            return Err(wit::types::CryptoError::InvalidKeyLength);
        }
        self.table.push(AeadKeyState { algo: a, key }).map_err(|_| wit::types::CryptoError::InvalidLength)
    }
    fn seal(&mut self, self_: Resource<AeadKeyState>, nonce: Vec<u8>, aad: Vec<u8>, plaintext: Vec<u8>) -> Result<Vec<u8>, wit::types::CryptoError> {
        let st = self.table.get(&self_).map_err(|_| wit::types::CryptoError::InvalidLength)?;
        crypto::aead_seal(st.algo, &st.key, &nonce, &aad, &plaintext).map_err(err2w)
    }
    fn open(&mut self, self_: Resource<AeadKeyState>, nonce: Vec<u8>, aad: Vec<u8>, ciphertext: Vec<u8>) -> Result<Vec<u8>, wit::types::CryptoError> {
        let st = self.table.get(&self_).map_err(|_| wit::types::CryptoError::InvalidLength)?;
        crypto::aead_open(st.algo, &st.key, &nonce, &aad, &ciphertext).map_err(err2w)
    }
    fn drop(&mut self, rep: Resource<AeadKeyState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wit::cipher::Host for HostState {}
impl wit::cipher::HostCipherKey for HostState {
    fn create(&mut self, algo: wit::types::CipherAlgo, key: Vec<u8>) -> Result<Resource<CipherKeyState>, wit::types::CryptoError> {
        let a = cipher2c(algo);
        let want = match a { crypto::CipherAlgo::Aes128Ctr | crypto::CipherAlgo::Aes128Cbc => 16, _ => 32 };
        if key.len() != want {
            return Err(wit::types::CryptoError::InvalidKeyLength);
        }
        self.table.push(CipherKeyState { algo: a, key }).map_err(|_| wit::types::CryptoError::InvalidLength)
    }
    fn encrypt(&mut self, self_: Resource<CipherKeyState>, iv: Vec<u8>, input: Vec<u8>) -> Result<Vec<u8>, wit::types::CryptoError> {
        let st = self.table.get(&self_).map_err(|_| wit::types::CryptoError::InvalidLength)?;
        crypto::cipher_encrypt(st.algo, &st.key, &iv, &input).map_err(err2w)
    }
    fn decrypt(&mut self, self_: Resource<CipherKeyState>, iv: Vec<u8>, input: Vec<u8>) -> Result<Vec<u8>, wit::types::CryptoError> {
        let st = self.table.get(&self_).map_err(|_| wit::types::CryptoError::InvalidLength)?;
        crypto::cipher_decrypt(st.algo, &st.key, &iv, &input).map_err(err2w)
    }
    fn drop(&mut self, rep: Resource<CipherKeyState>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl wit::kdf::Host for HostState {
    fn derive(&mut self, algo: wit::types::KdfAlgo, ikm: Vec<u8>, salt: Vec<u8>, info: Vec<u8>, iterations: u32, length: u32) -> Result<Vec<u8>, wit::types::CryptoError> {
        crypto::kdf_derive(kdf2c(algo), &ikm, &salt, &info, iterations, length).map_err(err2w)
    }
}

// ── asymmetric — Phase B stubs (unsupported) ─────────────────────────────────

impl wit::signatures::Host for HostState {
    fn generate(&mut self, _algo: wit::types::SigAlgo) -> Result<(Vec<u8>, Vec<u8>), wit::types::CryptoError> {
        Err(wit::types::CryptoError::UnsupportedAlgorithm)
    }
    fn sign(&mut self, _algo: wit::types::SigAlgo, _private_key: Vec<u8>, _message: Vec<u8>) -> Result<Vec<u8>, wit::types::CryptoError> {
        Err(wit::types::CryptoError::UnsupportedAlgorithm)
    }
    fn verify(&mut self, _algo: wit::types::SigAlgo, _public_key: Vec<u8>, _message: Vec<u8>, _signature: Vec<u8>) -> bool {
        false
    }
}

impl wit::key_exchange::Host for HostState {
    fn generate(&mut self, _algo: wit::types::KxAlgo) -> Result<(Vec<u8>, Vec<u8>), wit::types::CryptoError> {
        Err(wit::types::CryptoError::UnsupportedAlgorithm)
    }
    fn agree(&mut self, _algo: wit::types::KxAlgo, _private_key: Vec<u8>, _peer_public: Vec<u8>) -> Result<Vec<u8>, wit::types::CryptoError> {
        Err(wit::types::CryptoError::UnsupportedAlgorithm)
    }
}
