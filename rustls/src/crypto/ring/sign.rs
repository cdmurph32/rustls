use crate::enums::{SignatureAlgorithm, SignatureScheme};
use crate::error::Error;
use crate::sign::{Signer, SigningKey};
use crate::x509::{wrap_in_asn1_len, wrap_in_sequence};

use pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use ring::io::der;
use ring::signature::{self, EcdsaKeyPair, Ed25519KeyPair, RsaKeyPair};

use alloc::sync::Arc;
use core::fmt;
use std::error::Error as StdError;

/// Parse `der` as any supported key encoding/type, returning
/// the first which works.
pub fn any_supported_type(der: &PrivateKeyDer<'_>) -> Result<Arc<dyn SigningKey>, SignError> {
    if let Ok(rsa) = RsaSigningKey::new(der) {
        Ok(Arc::new(rsa))
    } else if let Ok(ecdsa) = any_ecdsa_type(der) {
        Ok(ecdsa)
    } else if let PrivateKeyDer::Pkcs8(pkcs8) = der {
        any_eddsa_type(pkcs8)
    } else {
        Err(SignError(()))
    }
}

/// Parse `der` as any ECDSA key type, returning the first which works.
///
/// Both SEC1 (PEM section starting with 'BEGIN EC PRIVATE KEY') and PKCS8
/// (PEM section starting with 'BEGIN PRIVATE KEY') encodings are supported.
pub fn any_ecdsa_type(der: &PrivateKeyDer<'_>) -> Result<Arc<dyn SigningKey>, SignError> {
    if let Ok(ecdsa_p256) = EcdsaSigningKey::new(
        der,
        SignatureScheme::ECDSA_NISTP256_SHA256,
        &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
    ) {
        return Ok(Arc::new(ecdsa_p256));
    }

    if let Ok(ecdsa_p384) = EcdsaSigningKey::new(
        der,
        SignatureScheme::ECDSA_NISTP384_SHA384,
        &signature::ECDSA_P384_SHA384_ASN1_SIGNING,
    ) {
        return Ok(Arc::new(ecdsa_p384));
    }

    Err(SignError(()))
}

/// Parse `der` as any EdDSA key type, returning the first which works.
pub fn any_eddsa_type(der: &PrivatePkcs8KeyDer<'_>) -> Result<Arc<dyn SigningKey>, SignError> {
    if let Ok(ed25519) = Ed25519SigningKey::new(der, SignatureScheme::ED25519) {
        return Ok(Arc::new(ed25519));
    }

    // TODO: Add support for Ed448

    Err(SignError(()))
}

/// A `SigningKey` for RSA-PKCS1 or RSA-PSS.
///
/// This is used by the test suite, so it must be `pub`, but it isn't part of
/// the public, stable, API.
#[doc(hidden)]
pub struct RsaSigningKey {
    key: Arc<RsaKeyPair>,
}

static ALL_RSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::RSA_PSS_SHA512,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PKCS1_SHA512,
    SignatureScheme::RSA_PKCS1_SHA384,
    SignatureScheme::RSA_PKCS1_SHA256,
];

impl RsaSigningKey {
    /// Make a new `RsaSigningKey` from a DER encoding, in either
    /// PKCS#1 or PKCS#8 format.
    pub fn new(der: &PrivateKeyDer<'_>) -> Result<Self, SignError> {
        let key_pair = match der {
            PrivateKeyDer::Pkcs1(pkcs1) => RsaKeyPair::from_der(pkcs1.secret_pkcs1_der()),
            PrivateKeyDer::Pkcs8(pkcs8) => RsaKeyPair::from_pkcs8(pkcs8.secret_pkcs8_der()),
            _ => return Err(SignError(())),
        }
        .map_err(|_| SignError(()))?;

        Ok(Self {
            key: Arc::new(key_pair),
        })
    }
}

impl SigningKey for RsaSigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        ALL_RSA_SCHEMES
            .iter()
            .find(|scheme| offered.contains(scheme))
            .map(|scheme| RsaSigner::new(Arc::clone(&self.key), *scheme))
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::RSA
    }
}

struct RsaSigner {
    key: Arc<RsaKeyPair>,
    scheme: SignatureScheme,
    encoding: &'static dyn signature::RsaEncoding,
}

impl RsaSigner {
    fn new(key: Arc<RsaKeyPair>, scheme: SignatureScheme) -> Box<dyn Signer> {
        let encoding: &dyn signature::RsaEncoding = match scheme {
            SignatureScheme::RSA_PKCS1_SHA256 => &signature::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384 => &signature::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512 => &signature::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256 => &signature::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384 => &signature::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512 => &signature::RSA_PSS_SHA512,
            _ => unreachable!(),
        };

        Box::new(Self {
            key,
            scheme,
            encoding,
        })
    }
}

impl Signer for RsaSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let mut sig = vec![0; self.key.public_modulus_len()];

        let rng = ring::rand::SystemRandom::new();
        self.key
            .sign(self.encoding, &rng, message, &mut sig)
            .map(|_| sig)
            .map_err(|_| Error::General("signing failed".to_string()))
    }

    fn scheme(&self) -> SignatureScheme {
        self.scheme
    }
}

/// A SigningKey that uses exactly one TLS-level SignatureScheme
/// and one ring-level signature::SigningAlgorithm.
///
/// Compare this to RsaSigningKey, which for a particular key is
/// willing to sign with several algorithms.  This is quite poor
/// cryptography practice, but is necessary because a given RSA key
/// is expected to work in TLS1.2 (PKCS#1 signatures) and TLS1.3
/// (PSS signatures) -- nobody is willing to obtain certificates for
/// different protocol versions.
///
/// Currently this is only implemented for ECDSA keys.
struct EcdsaSigningKey {
    key: Arc<EcdsaKeyPair>,
    scheme: SignatureScheme,
}

impl EcdsaSigningKey {
    /// Make a new `ECDSASigningKey` from a DER encoding in PKCS#8 or SEC1
    /// format, expecting a key usable with precisely the given signature
    /// scheme.
    fn new(
        der: &PrivateKeyDer<'_>,
        scheme: SignatureScheme,
        sigalg: &'static signature::EcdsaSigningAlgorithm,
    ) -> Result<Self, ()> {
        let key_pair = match der {
            PrivateKeyDer::Sec1(sec1) => {
                Self::convert_sec1_to_pkcs8(scheme, sigalg, sec1.secret_sec1_der())?
            }
            PrivateKeyDer::Pkcs8(pkcs8) => {
                EcdsaKeyPair::from_pkcs8(sigalg, pkcs8.secret_pkcs8_der()).map_err(|_| ())?
            }
            _ => return Err(()),
        };

        Ok(Self {
            key: Arc::new(key_pair),
            scheme,
        })
    }

    /// Convert a SEC1 encoding to PKCS8, and ask ring to parse it.  This
    /// can be removed once <https://github.com/briansmith/ring/pull/1456>
    /// (or equivalent) is landed.
    fn convert_sec1_to_pkcs8(
        scheme: SignatureScheme,
        sigalg: &'static signature::EcdsaSigningAlgorithm,
        maybe_sec1_der: &[u8],
    ) -> Result<EcdsaKeyPair, ()> {
        let pkcs8_prefix = match scheme {
            SignatureScheme::ECDSA_NISTP256_SHA256 => &PKCS8_PREFIX_ECDSA_NISTP256,
            SignatureScheme::ECDSA_NISTP384_SHA384 => &PKCS8_PREFIX_ECDSA_NISTP384,
            _ => unreachable!(), // all callers are in this file
        };

        // wrap sec1 encoding in an OCTET STRING
        let mut sec1_wrap = Vec::with_capacity(maybe_sec1_der.len() + 8);
        sec1_wrap.extend_from_slice(maybe_sec1_der);
        wrap_in_asn1_len(&mut sec1_wrap);
        sec1_wrap.insert(0, der::Tag::OctetString as u8);

        let mut pkcs8 = Vec::with_capacity(pkcs8_prefix.len() + sec1_wrap.len() + 4);
        pkcs8.extend_from_slice(pkcs8_prefix);
        pkcs8.extend_from_slice(&sec1_wrap);
        wrap_in_sequence(&mut pkcs8);

        EcdsaKeyPair::from_pkcs8(sigalg, &pkcs8).map_err(|_| ())
    }
}

// This is (line-by-line):
// - INTEGER Version = 0
// - SEQUENCE (privateKeyAlgorithm)
//   - id-ecPublicKey OID
//   - prime256v1 OID
const PKCS8_PREFIX_ECDSA_NISTP256: &[u8] = b"\x02\x01\x00\
      \x30\x13\
      \x06\x07\x2a\x86\x48\xce\x3d\x02\x01\
      \x06\x08\x2a\x86\x48\xce\x3d\x03\x01\x07";

// This is (line-by-line):
// - INTEGER Version = 0
// - SEQUENCE (privateKeyAlgorithm)
//   - id-ecPublicKey OID
//   - secp384r1 OID
const PKCS8_PREFIX_ECDSA_NISTP384: &[u8] = b"\x02\x01\x00\
     \x30\x10\
     \x06\x07\x2a\x86\x48\xce\x3d\x02\x01\
     \x06\x05\x2b\x81\x04\x00\x22";

impl SigningKey for EcdsaSigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        if offered.contains(&self.scheme) {
            Some(Box::new(EcdsaSigner {
                key: Arc::clone(&self.key),
                scheme: self.scheme,
            }))
        } else {
            None
        }
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        self.scheme.sign()
    }
}

struct EcdsaSigner {
    key: Arc<EcdsaKeyPair>,
    scheme: SignatureScheme,
}

impl Signer for EcdsaSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        let rng = ring::rand::SystemRandom::new();
        self.key
            .sign(&rng, message)
            .map_err(|_| Error::General("signing failed".into()))
            .map(|sig| sig.as_ref().into())
    }

    fn scheme(&self) -> SignatureScheme {
        self.scheme
    }
}

/// A SigningKey that uses exactly one TLS-level SignatureScheme
/// and one ring-level signature::SigningAlgorithm.
///
/// Compare this to RsaSigningKey, which for a particular key is
/// willing to sign with several algorithms.  This is quite poor
/// cryptography practice, but is necessary because a given RSA key
/// is expected to work in TLS1.2 (PKCS#1 signatures) and TLS1.3
/// (PSS signatures) -- nobody is willing to obtain certificates for
/// different protocol versions.
///
/// Currently this is only implemented for Ed25519 keys.
struct Ed25519SigningKey {
    key: Arc<Ed25519KeyPair>,
    scheme: SignatureScheme,
}

impl Ed25519SigningKey {
    /// Make a new `Ed25519SigningKey` from a DER encoding in PKCS#8 format,
    /// expecting a key usable with precisely the given signature scheme.
    fn new(der: &PrivatePkcs8KeyDer<'_>, scheme: SignatureScheme) -> Result<Self, SignError> {
        match Ed25519KeyPair::from_pkcs8_maybe_unchecked(der.secret_pkcs8_der()) {
            Ok(key_pair) => Ok(Self {
                key: Arc::new(key_pair),
                scheme,
            }),
            Err(_) => Err(SignError(())),
        }
    }
}

impl SigningKey for Ed25519SigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        if offered.contains(&self.scheme) {
            Some(Box::new(Ed25519Signer {
                key: Arc::clone(&self.key),
                scheme: self.scheme,
            }))
        } else {
            None
        }
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        self.scheme.sign()
    }
}

struct Ed25519Signer {
    key: Arc<Ed25519KeyPair>,
    scheme: SignatureScheme,
}

impl Signer for Ed25519Signer {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.key.sign(message).as_ref().into())
    }

    fn scheme(&self) -> SignatureScheme {
        self.scheme
    }
}

/// Errors while signing
#[derive(Debug)]
pub struct SignError(());

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("sign error")
    }
}

impl StdError for SignError {}

#[cfg(test)]
mod tests {
    use super::*;
    use pki_types::{PrivatePkcs1KeyDer, PrivateSec1KeyDer};

    #[test]
    fn can_load_ecdsa_nistp256_pkcs8() {
        let key =
            PrivatePkcs8KeyDer::from(&include_bytes!("../../testdata/nistp256key.pkcs8.der")[..]);
        assert!(any_eddsa_type(&key).is_err());
        let key = PrivateKeyDer::Pkcs8(key);
        assert!(any_supported_type(&key).is_ok());
        assert!(any_ecdsa_type(&key).is_ok());
    }

    #[test]
    fn can_load_ecdsa_nistp256_sec1() {
        let key = PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(
            &include_bytes!("../../testdata/nistp256key.der")[..],
        ));
        assert!(any_supported_type(&key).is_ok());
        assert!(any_ecdsa_type(&key).is_ok());
    }

    #[test]
    fn can_load_ecdsa_nistp384_pkcs8() {
        let key =
            PrivatePkcs8KeyDer::from(&include_bytes!("../../testdata/nistp384key.pkcs8.der")[..]);
        assert!(any_eddsa_type(&key).is_err());
        let key = PrivateKeyDer::Pkcs8(key);
        assert!(any_supported_type(&key).is_ok());
        assert!(any_ecdsa_type(&key).is_ok());
    }

    #[test]
    fn can_load_ecdsa_nistp384_sec1() {
        let key = PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(
            &include_bytes!("../../testdata/nistp384key.der")[..],
        ));
        assert!(any_supported_type(&key).is_ok());
        assert!(any_ecdsa_type(&key).is_ok());
    }

    #[test]
    fn can_load_eddsa_pkcs8() {
        let key = PrivatePkcs8KeyDer::from(&include_bytes!("../../testdata/eddsakey.der")[..]);
        assert!(any_eddsa_type(&key).is_ok());
        let key = PrivateKeyDer::Pkcs8(key);
        assert!(any_supported_type(&key).is_ok());
        assert!(any_ecdsa_type(&key).is_err());
    }

    #[test]
    fn can_load_rsa2048_pkcs8() {
        let key =
            PrivatePkcs8KeyDer::from(&include_bytes!("../../testdata/rsa2048key.pkcs8.der")[..]);
        assert!(any_eddsa_type(&key).is_err());
        let key = PrivateKeyDer::Pkcs8(key);
        assert!(any_supported_type(&key).is_ok());
        assert!(any_ecdsa_type(&key).is_err());
    }

    #[test]
    fn can_load_rsa2048_pkcs1() {
        let key = PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(
            &include_bytes!("../../testdata/rsa2048key.pkcs1.der")[..],
        ));
        assert!(any_supported_type(&key).is_ok());
        assert!(any_ecdsa_type(&key).is_err());
    }
}
