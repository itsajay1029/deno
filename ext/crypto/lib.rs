// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use aes_kw::KekAes128;
use aes_kw::KekAes192;
use aes_kw::KekAes256;

use deno_core::error::custom_error;
use deno_core::error::not_supported;
use deno_core::error::type_error;
use deno_core::error::AnyError;
use deno_core::op;
use deno_core::ToJsBuffer;

use deno_core::task::spawn_blocking;
use deno_core::JsBuffer;
use deno_core::OpState;
use serde::Deserialize;
use shared::operation_error;

use p256::elliptic_curve::sec1::FromEncodedPoint;
use p256::pkcs8::DecodePrivateKey;
use rand::rngs::OsRng;
use rand::rngs::StdRng;
use rand::thread_rng;
use rand::Rng;
use rand::SeedableRng;
use ring::digest;
use ring::hkdf;
use ring::hmac::Algorithm as HmacAlgorithm;
use ring::hmac::Key as HmacKey;
use ring::pbkdf2;
use ring::rand as RingRand;
use ring::signature::EcdsaKeyPair;
use ring::signature::EcdsaSigningAlgorithm;
use ring::signature::EcdsaVerificationAlgorithm;
use ring::signature::KeyPair;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use sha1::Sha1;
use sha2::Sha256;
use sha2::Sha384;
use sha2::Sha512;
use signature::RandomizedSigner;
use signature::Signer;
use signature::Verifier;
use std::convert::TryFrom;
use std::num::NonZeroU32;
use std::path::PathBuf;

pub use rand; // Re-export rand

mod decrypt;
mod ed25519;
mod encrypt;
mod export_key;
mod generate_key;
mod import_key;
mod key;
mod shared;
mod x25519;

pub use crate::decrypt::op_crypto_decrypt;
pub use crate::encrypt::op_crypto_encrypt;
pub use crate::export_key::op_crypto_export_key;
pub use crate::generate_key::op_crypto_generate_key;
pub use crate::import_key::op_crypto_import_key;
use crate::key::Algorithm;
use crate::key::CryptoHash;
use crate::key::CryptoNamedCurve;
use crate::key::HkdfOutput;
use crate::shared::V8RawKeyData;

deno_core::extension!(deno_crypto,
  deps = [ deno_webidl, deno_web ],
  ops = [
    op_crypto_get_random_values,
    op_crypto_generate_key,
    op_crypto_sign_key,
    op_crypto_verify_key,
    op_crypto_derive_bits,
    op_crypto_import_key,
    op_crypto_export_key,
    op_crypto_encrypt,
    op_crypto_decrypt,
    op_crypto_subtle_digest,
    op_crypto_random_uuid,
    op_crypto_wrap_key,
    op_crypto_unwrap_key,
    op_crypto_base64url_decode,
    op_crypto_base64url_encode,
    x25519::op_crypto_generate_x25519_keypair,
    x25519::op_crypto_derive_bits_x25519,
    x25519::op_crypto_import_spki_x25519,
    x25519::op_crypto_import_pkcs8_x25519,
    ed25519::op_crypto_generate_ed25519_keypair,
    ed25519::op_crypto_import_spki_ed25519,
    ed25519::op_crypto_import_pkcs8_ed25519,
    ed25519::op_crypto_sign_ed25519,
    ed25519::op_crypto_verify_ed25519,
    ed25519::op_crypto_export_spki_ed25519,
    ed25519::op_crypto_export_pkcs8_ed25519,
    ed25519::op_crypto_jwk_x_ed25519,
    x25519::op_crypto_export_spki_x25519,
    x25519::op_crypto_export_pkcs8_x25519,
  ],
  esm = [ "00_crypto.js" ],
  options = {
    maybe_seed: Option<u64>,
  },
  state = |state, options| {
    if let Some(seed) = options.maybe_seed {
      state.put(StdRng::seed_from_u64(seed));
    }
  },
);

#[op]
pub fn op_crypto_base64url_decode(
  data: String,
) -> Result<ToJsBuffer, AnyError> {
  let data: Vec<u8> = base64::decode_config(data, base64::URL_SAFE_NO_PAD)?;
  Ok(data.into())
}

#[op]
pub fn op_crypto_base64url_encode(data: JsBuffer) -> String {
  let data: String = base64::encode_config(data, base64::URL_SAFE_NO_PAD);
  data
}

#[op(fast)]
pub fn op_crypto_get_random_values(
  state: &mut OpState,
  out: &mut [u8],
) -> Result<(), AnyError> {
  if out.len() > 65536 {
    return Err(
      deno_web::DomExceptionQuotaExceededError::new(&format!("The ArrayBufferView's byte length ({}) exceeds the number of bytes of entropy available via this API (65536)", out.len()))
        .into(),
    );
  }

  let maybe_seeded_rng = state.try_borrow_mut::<StdRng>();
  if let Some(seeded_rng) = maybe_seeded_rng {
    seeded_rng.fill(out);
  } else {
    let mut rng = thread_rng();
    rng.fill(out);
  }

  Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyFormat {
  Raw,
  Pkcs8,
  Spki,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyType {
  Secret,
  Private,
  Public,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct KeyData {
  r#type: KeyType,
  data: JsBuffer,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignArg {
  key: KeyData,
  algorithm: Algorithm,
  salt_length: Option<u32>,
  hash: Option<CryptoHash>,
  named_curve: Option<CryptoNamedCurve>,
}

#[op]
pub async fn op_crypto_sign_key(
  args: SignArg,
  zero_copy: JsBuffer,
) -> Result<ToJsBuffer, AnyError> {
  let data = &*zero_copy;
  let algorithm = args.algorithm;

  let signature = match algorithm {
    Algorithm::RsassaPkcs1v15 => {
      use rsa::pkcs1v15::SigningKey;
      let private_key = RsaPrivateKey::from_pkcs1_der(&args.key.data)?;
      match args
        .hash
        .ok_or_else(|| type_error("Missing argument hash".to_string()))?
      {
        CryptoHash::Sha1 => {
          let signing_key = SigningKey::<Sha1>::new_with_prefix(private_key);
          signing_key.sign(data)
        }
        CryptoHash::Sha256 => {
          let signing_key = SigningKey::<Sha256>::new_with_prefix(private_key);
          signing_key.sign(data)
        }
        CryptoHash::Sha384 => {
          let signing_key = SigningKey::<Sha384>::new_with_prefix(private_key);
          signing_key.sign(data)
        }
        CryptoHash::Sha512 => {
          let signing_key = SigningKey::<Sha512>::new_with_prefix(private_key);
          signing_key.sign(data)
        }
      }
      .to_vec()
    }
    Algorithm::RsaPss => {
      use rsa::pss::SigningKey;
      let private_key = RsaPrivateKey::from_pkcs1_der(&args.key.data)?;

      let salt_len = args
        .salt_length
        .ok_or_else(|| type_error("Missing argument saltLength".to_string()))?
        as usize;

      let rng = OsRng;
      match args
        .hash
        .ok_or_else(|| type_error("Missing argument hash".to_string()))?
      {
        CryptoHash::Sha1 => {
          let signing_key =
            SigningKey::<Sha1>::new_with_salt_len(private_key, salt_len);
          signing_key.sign_with_rng(rng, data)
        }
        CryptoHash::Sha256 => {
          let signing_key =
            SigningKey::<Sha256>::new_with_salt_len(private_key, salt_len);
          signing_key.sign_with_rng(rng, data)
        }
        CryptoHash::Sha384 => {
          let signing_key =
            SigningKey::<Sha384>::new_with_salt_len(private_key, salt_len);
          signing_key.sign_with_rng(rng, data)
        }
        CryptoHash::Sha512 => {
          let signing_key =
            SigningKey::<Sha512>::new_with_salt_len(private_key, salt_len);
          signing_key.sign_with_rng(rng, data)
        }
      }
      .to_vec()
    }
    Algorithm::Ecdsa => {
      let curve: &EcdsaSigningAlgorithm =
        args.named_curve.ok_or_else(not_supported)?.try_into()?;

      let key_pair = EcdsaKeyPair::from_pkcs8(curve, &args.key.data)?;
      // We only support P256-SHA256 & P384-SHA384. These are recommended signature pairs.
      // https://briansmith.org/rustdoc/ring/signature/index.html#statics
      if let Some(hash) = args.hash {
        match hash {
          CryptoHash::Sha256 | CryptoHash::Sha384 => (),
          _ => return Err(type_error("Unsupported algorithm")),
        }
      };

      let rng = RingRand::SystemRandom::new();
      let signature = key_pair.sign(&rng, data)?;

      // Signature data as buffer.
      signature.as_ref().to_vec()
    }
    Algorithm::Hmac => {
      let hash: HmacAlgorithm = args.hash.ok_or_else(not_supported)?.into();

      let key = HmacKey::new(hash, &args.key.data);

      let signature = ring::hmac::sign(&key, data);
      signature.as_ref().to_vec()
    }
    _ => return Err(type_error("Unsupported algorithm".to_string())),
  };

  Ok(signature.into())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyArg {
  key: KeyData,
  algorithm: Algorithm,
  hash: Option<CryptoHash>,
  signature: JsBuffer,
  named_curve: Option<CryptoNamedCurve>,
}

#[op]
pub async fn op_crypto_verify_key(
  args: VerifyArg,
  zero_copy: JsBuffer,
) -> Result<bool, AnyError> {
  let data = &*zero_copy;
  let algorithm = args.algorithm;

  let verification = match algorithm {
    Algorithm::RsassaPkcs1v15 => {
      use rsa::pkcs1v15::Signature;
      use rsa::pkcs1v15::VerifyingKey;
      let public_key = read_rsa_public_key(args.key)?;
      let signature: Signature = args.signature.to_vec().into();
      match args
        .hash
        .ok_or_else(|| type_error("Missing argument hash".to_string()))?
      {
        CryptoHash::Sha1 => {
          let verifying_key = VerifyingKey::<Sha1>::new_with_prefix(public_key);
          verifying_key.verify(data, &signature).is_ok()
        }
        CryptoHash::Sha256 => {
          let verifying_key =
            VerifyingKey::<Sha256>::new_with_prefix(public_key);
          verifying_key.verify(data, &signature).is_ok()
        }
        CryptoHash::Sha384 => {
          let verifying_key =
            VerifyingKey::<Sha384>::new_with_prefix(public_key);
          verifying_key.verify(data, &signature).is_ok()
        }
        CryptoHash::Sha512 => {
          let verifying_key =
            VerifyingKey::<Sha512>::new_with_prefix(public_key);
          verifying_key.verify(data, &signature).is_ok()
        }
      }
    }
    Algorithm::RsaPss => {
      use rsa::pss::Signature;
      use rsa::pss::VerifyingKey;
      let public_key = read_rsa_public_key(args.key)?;
      let signature: Signature = args.signature.to_vec().into();

      match args
        .hash
        .ok_or_else(|| type_error("Missing argument hash".to_string()))?
      {
        CryptoHash::Sha1 => {
          let verifying_key: VerifyingKey<Sha1> = public_key.into();
          verifying_key.verify(data, &signature).is_ok()
        }
        CryptoHash::Sha256 => {
          let verifying_key: VerifyingKey<Sha256> = public_key.into();
          verifying_key.verify(data, &signature).is_ok()
        }
        CryptoHash::Sha384 => {
          let verifying_key: VerifyingKey<Sha384> = public_key.into();
          verifying_key.verify(data, &signature).is_ok()
        }
        CryptoHash::Sha512 => {
          let verifying_key: VerifyingKey<Sha512> = public_key.into();
          verifying_key.verify(data, &signature).is_ok()
        }
      }
    }
    Algorithm::Hmac => {
      let hash: HmacAlgorithm = args.hash.ok_or_else(not_supported)?.into();
      let key = HmacKey::new(hash, &args.key.data);
      ring::hmac::verify(&key, data, &args.signature).is_ok()
    }
    Algorithm::Ecdsa => {
      let signing_alg: &EcdsaSigningAlgorithm =
        args.named_curve.ok_or_else(not_supported)?.try_into()?;
      let verify_alg: &EcdsaVerificationAlgorithm =
        args.named_curve.ok_or_else(not_supported)?.try_into()?;

      let private_key;

      let public_key_bytes = match args.key.r#type {
        KeyType::Private => {
          private_key = EcdsaKeyPair::from_pkcs8(signing_alg, &args.key.data)?;

          private_key.public_key().as_ref()
        }
        KeyType::Public => &*args.key.data,
        _ => return Err(type_error("Invalid Key format".to_string())),
      };

      let public_key =
        ring::signature::UnparsedPublicKey::new(verify_alg, public_key_bytes);

      public_key.verify(data, &args.signature).is_ok()
    }
    _ => return Err(type_error("Unsupported algorithm".to_string())),
  };

  Ok(verification)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeriveKeyArg {
  key: KeyData,
  algorithm: Algorithm,
  hash: Option<CryptoHash>,
  length: usize,
  iterations: Option<u32>,
  // ECDH
  public_key: Option<KeyData>,
  named_curve: Option<CryptoNamedCurve>,
  // HKDF
  info: Option<JsBuffer>,
}

#[op]
pub async fn op_crypto_derive_bits(
  args: DeriveKeyArg,
  zero_copy: Option<JsBuffer>,
) -> Result<ToJsBuffer, AnyError> {
  let algorithm = args.algorithm;
  match algorithm {
    Algorithm::Pbkdf2 => {
      let zero_copy = zero_copy.ok_or_else(not_supported)?;
      let salt = &*zero_copy;
      // The caller must validate these cases.
      assert!(args.length > 0);
      assert!(args.length % 8 == 0);

      let algorithm = match args.hash.ok_or_else(not_supported)? {
        CryptoHash::Sha1 => pbkdf2::PBKDF2_HMAC_SHA1,
        CryptoHash::Sha256 => pbkdf2::PBKDF2_HMAC_SHA256,
        CryptoHash::Sha384 => pbkdf2::PBKDF2_HMAC_SHA384,
        CryptoHash::Sha512 => pbkdf2::PBKDF2_HMAC_SHA512,
      };

      // This will never panic. We have already checked length earlier.
      let iterations =
        NonZeroU32::new(args.iterations.ok_or_else(not_supported)?).unwrap();
      let secret = args.key.data;
      let mut out = vec![0; args.length / 8];
      pbkdf2::derive(algorithm, iterations, salt, &secret, &mut out);
      Ok(out.into())
    }
    Algorithm::Ecdh => {
      let named_curve = args
        .named_curve
        .ok_or_else(|| type_error("Missing argument namedCurve".to_string()))?;

      let public_key = args
        .public_key
        .ok_or_else(|| type_error("Missing argument publicKey"))?;

      match named_curve {
        CryptoNamedCurve::P256 => {
          let secret_key = p256::SecretKey::from_pkcs8_der(&args.key.data)
            .map_err(|_| type_error("Unexpected error decoding private key"))?;

          let public_key = match public_key.r#type {
            KeyType::Private => {
              p256::SecretKey::from_pkcs8_der(&public_key.data)
                .map_err(|_| {
                  type_error("Unexpected error decoding private key")
                })?
                .public_key()
            }
            KeyType::Public => {
              let point = p256::EncodedPoint::from_bytes(public_key.data)
                .map_err(|_| {
                  type_error("Unexpected error decoding private key")
                })?;

              let pk = p256::PublicKey::from_encoded_point(&point);
              // pk is a constant time Option.
              if pk.is_some().into() {
                pk.unwrap()
              } else {
                return Err(type_error(
                  "Unexpected error decoding private key",
                ));
              }
            }
            _ => unreachable!(),
          };

          let shared_secret = p256::elliptic_curve::ecdh::diffie_hellman(
            secret_key.to_nonzero_scalar(),
            public_key.as_affine(),
          );

          // raw serialized x-coordinate of the computed point
          Ok(shared_secret.raw_secret_bytes().to_vec().into())
        }
        CryptoNamedCurve::P384 => {
          let secret_key = p384::SecretKey::from_pkcs8_der(&args.key.data)
            .map_err(|_| type_error("Unexpected error decoding private key"))?;

          let public_key = match public_key.r#type {
            KeyType::Private => {
              p384::SecretKey::from_pkcs8_der(&public_key.data)
                .map_err(|_| {
                  type_error("Unexpected error decoding private key")
                })?
                .public_key()
            }
            KeyType::Public => {
              let point = p384::EncodedPoint::from_bytes(public_key.data)
                .map_err(|_| {
                  type_error("Unexpected error decoding private key")
                })?;

              let pk = p384::PublicKey::from_encoded_point(&point);
              // pk is a constant time Option.
              if pk.is_some().into() {
                pk.unwrap()
              } else {
                return Err(type_error(
                  "Unexpected error decoding private key",
                ));
              }
            }
            _ => unreachable!(),
          };

          let shared_secret = p384::elliptic_curve::ecdh::diffie_hellman(
            secret_key.to_nonzero_scalar(),
            public_key.as_affine(),
          );

          // raw serialized x-coordinate of the computed point
          Ok(shared_secret.raw_secret_bytes().to_vec().into())
        }
      }
    }
    Algorithm::Hkdf => {
      let zero_copy = zero_copy.ok_or_else(not_supported)?;
      let salt = &*zero_copy;
      let algorithm = match args.hash.ok_or_else(not_supported)? {
        CryptoHash::Sha1 => hkdf::HKDF_SHA1_FOR_LEGACY_USE_ONLY,
        CryptoHash::Sha256 => hkdf::HKDF_SHA256,
        CryptoHash::Sha384 => hkdf::HKDF_SHA384,
        CryptoHash::Sha512 => hkdf::HKDF_SHA512,
      };

      let info = args
        .info
        .ok_or_else(|| type_error("Missing argument info".to_string()))?;
      // IKM
      let secret = args.key.data;
      // L
      let length = args.length / 8;

      let salt = hkdf::Salt::new(algorithm, salt);
      let prk = salt.extract(&secret);
      let info = &[&*info];
      let okm = prk.expand(info, HkdfOutput(length)).map_err(|_e| {
        custom_error(
          "DOMExceptionOperationError",
          "The length provided for HKDF is too large",
        )
      })?;
      let mut r = vec![0u8; length];
      okm.fill(&mut r)?;
      Ok(r.into())
    }
    _ => Err(type_error("Unsupported algorithm".to_string())),
  }
}

fn read_rsa_public_key(key_data: KeyData) -> Result<RsaPublicKey, AnyError> {
  let public_key = match key_data.r#type {
    KeyType::Private => {
      RsaPrivateKey::from_pkcs1_der(&key_data.data)?.to_public_key()
    }
    KeyType::Public => RsaPublicKey::from_pkcs1_der(&key_data.data)?,
    KeyType::Secret => unreachable!("unexpected KeyType::Secret"),
  };
  Ok(public_key)
}

#[op]
pub fn op_crypto_random_uuid(state: &mut OpState) -> Result<String, AnyError> {
  let maybe_seeded_rng = state.try_borrow_mut::<StdRng>();
  let uuid = if let Some(seeded_rng) = maybe_seeded_rng {
    let mut bytes = [0u8; 16];
    seeded_rng.fill(&mut bytes);
    uuid::Builder::from_bytes(bytes)
      .with_version(uuid::Version::Random)
      .into_uuid()
  } else {
    uuid::Uuid::new_v4()
  };

  Ok(uuid.to_string())
}

#[op]
pub async fn op_crypto_subtle_digest(
  algorithm: CryptoHash,
  data: JsBuffer,
) -> Result<ToJsBuffer, AnyError> {
  let output = spawn_blocking(move || {
    digest::digest(algorithm.into(), &data)
      .as_ref()
      .to_vec()
      .into()
  })
  .await?;

  Ok(output)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WrapUnwrapKeyArg {
  key: V8RawKeyData,
  algorithm: Algorithm,
}

#[op]
pub fn op_crypto_wrap_key(
  args: WrapUnwrapKeyArg,
  data: JsBuffer,
) -> Result<ToJsBuffer, AnyError> {
  let algorithm = args.algorithm;

  match algorithm {
    Algorithm::AesKw => {
      let key = args.key.as_secret_key()?;

      if data.len() % 8 != 0 {
        return Err(type_error("Data must be multiple of 8 bytes"));
      }

      let wrapped_key = match key.len() {
        16 => KekAes128::new(key.into()).wrap_vec(&data),
        24 => KekAes192::new(key.into()).wrap_vec(&data),
        32 => KekAes256::new(key.into()).wrap_vec(&data),
        _ => return Err(type_error("Invalid key length")),
      }
      .map_err(|_| operation_error("encryption error"))?;

      Ok(wrapped_key.into())
    }
    _ => Err(type_error("Unsupported algorithm")),
  }
}

#[op]
pub fn op_crypto_unwrap_key(
  args: WrapUnwrapKeyArg,
  data: JsBuffer,
) -> Result<ToJsBuffer, AnyError> {
  let algorithm = args.algorithm;
  match algorithm {
    Algorithm::AesKw => {
      let key = args.key.as_secret_key()?;

      if data.len() % 8 != 0 {
        return Err(type_error("Data must be multiple of 8 bytes"));
      }

      let unwrapped_key = match key.len() {
        16 => KekAes128::new(key.into()).unwrap_vec(&data),
        24 => KekAes192::new(key.into()).unwrap_vec(&data),
        32 => KekAes256::new(key.into()).unwrap_vec(&data),
        _ => return Err(type_error("Invalid key length")),
      }
      .map_err(|_| {
        operation_error("decryption error - integrity check failed")
      })?;

      Ok(unwrapped_key.into())
    }
    _ => Err(type_error("Unsupported algorithm")),
  }
}

pub fn get_declaration() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lib.deno_crypto.d.ts")
}
