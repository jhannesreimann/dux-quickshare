//! UKEY2 key agreement and the D2D SecureMessage channel used by Nearby
//! Connections (AES-256-CBC + HMAC-SHA256).

use anyhow::{Context, Result, anyhow, bail, ensure};
use cbc::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use p256::PublicKey;
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::Generate;
use p256::elliptic_curve::sec1::ToSec1Point;
use prost::Message;
use sha2::{Digest, Sha256, Sha512};

use crate::proto::{self, ukey2_type};

type HmacSha256 = Hmac<Sha256>;
type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// SHA-256("D2D")
const D2D_SALT: [u8; 32] = [
    0x82, 0xAA, 0x55, 0xA0, 0xD3, 0x97, 0xF8, 0x83, 0x46, 0xCA, 0x1C, 0xEE, 0x8D, 0x39, 0x09, 0xB9,
    0x5F, 0x13, 0xFA, 0x7D, 0xEB, 0x1D, 0x4A, 0xB3, 0x83, 0x76, 0xB8, 0x25, 0x6D, 0xA8, 0x55, 0x10,
];

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::fill(&mut buf[..]);
    buf
}

pub fn random_vec(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    rand::fill(&mut buf[..]);
    buf
}

fn hkdf32(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    okm
}

/// Encode an EC coordinate like Java's `BigInteger.toByteArray()`: big endian
/// two's complement, so a leading zero byte is required when the top bit is set.
fn encode_coordinate(c: &[u8]) -> Vec<u8> {
    let mut v = c.to_vec();
    if v.first().is_some_and(|b| b & 0x80 != 0) {
        v.insert(0, 0);
    }
    v
}

/// Normalize a (possibly sign-extended or zero-stripped) coordinate to 32 bytes.
fn decode_coordinate(c: &[u8]) -> Result<[u8; 32]> {
    let start = c.iter().position(|b| *b != 0).unwrap_or(c.len());
    let digits = &c[start..];
    ensure!(
        digits.len() <= 32,
        "EC coordinate too long ({} bytes)",
        c.len()
    );
    let mut out = [0u8; 32];
    out[32 - digits.len()..].copy_from_slice(digits);
    Ok(out)
}

fn encode_public_key(pk: &PublicKey) -> Vec<u8> {
    let point = pk.to_uncompressed_point();
    let bytes = point.as_slice();
    proto::GenericPublicKey {
        r#type: proto::PUBLIC_KEY_TYPE_EC_P256,
        ec_p256_public_key: Some(proto::EcP256PublicKey {
            x: encode_coordinate(&bytes[1..33]),
            y: encode_coordinate(&bytes[33..65]),
        }),
    }
    .encode_to_vec()
}

fn decode_public_key(bytes: &[u8]) -> Result<PublicKey> {
    let key = proto::GenericPublicKey::decode(bytes).context("invalid GenericPublicKey")?;
    ensure!(
        key.r#type == proto::PUBLIC_KEY_TYPE_EC_P256,
        "unsupported public key type {}",
        key.r#type
    );
    let ec = key
        .ec_p256_public_key
        .ok_or_else(|| anyhow!("missing EC P-256 key"))?;
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04);
    sec1.extend_from_slice(&decode_coordinate(&ec.x)?);
    sec1.extend_from_slice(&decode_coordinate(&ec.y)?);
    PublicKey::from_sec1_bytes(&sec1).map_err(|_| anyhow!("peer public key is not on P-256"))
}

/// Derive the 4 digit confirmation PIN shown on both devices.
fn pin_from_auth_string(auth: &[u8]) -> String {
    let mut hash: i64 = 0;
    let mut multiplier: i64 = 1;
    for &b in auth {
        // Same arithmetic as Android (signed bytes, truncating remainder).
        hash = (hash + i64::from(b as i8) * multiplier) % 9973;
        multiplier = (multiplier * 31) % 9973;
    }
    format!("{:04}", hash.abs())
}

pub struct Keys {
    encrypt: [u8; 32],
    send_hmac: [u8; 32],
    decrypt: [u8; 32],
    recv_hmac: [u8; 32],
}

pub struct Handshake {
    pub keys: Keys,
    pub pin: String,
}

fn derive(
    shared_x: &[u8],
    client_init: &[u8],
    server_init: &[u8],
    we_are_client: bool,
) -> Handshake {
    let dhs = Sha256::digest(shared_x);
    let mut info = Vec::with_capacity(client_init.len() + server_init.len());
    info.extend_from_slice(client_init);
    info.extend_from_slice(server_init);
    let auth = hkdf32(b"UKEY2 v1 auth", &dhs, &info);
    let next = hkdf32(b"UKEY2 v1 next", &dhs, &info);

    let d2d_client = hkdf32(&D2D_SALT, &next, b"client");
    let d2d_server = hkdf32(&D2D_SALT, &next, b"server");
    let smsg_salt = Sha256::digest(b"SecureMessage");
    let client_enc = hkdf32(&smsg_salt, &d2d_client, b"ENC:2");
    let client_sig = hkdf32(&smsg_salt, &d2d_client, b"SIG:1");
    let server_enc = hkdf32(&smsg_salt, &d2d_server, b"ENC:2");
    let server_sig = hkdf32(&smsg_salt, &d2d_server, b"SIG:1");

    let keys = if we_are_client {
        Keys {
            encrypt: client_enc,
            send_hmac: client_sig,
            decrypt: server_enc,
            recv_hmac: server_sig,
        }
    } else {
        Keys {
            encrypt: server_enc,
            send_hmac: server_sig,
            decrypt: client_enc,
            recv_hmac: client_sig,
        }
    };
    Handshake {
        keys,
        pin: pin_from_auth_string(&auth),
    }
}

fn wrap_ukey2(message_type: i32, data: Vec<u8>) -> Vec<u8> {
    proto::Ukey2Message {
        message_type: Some(message_type),
        message_data: Some(data),
    }
    .encode_to_vec()
}

fn unwrap_ukey2(bytes: &[u8], expected: i32) -> Result<Vec<u8>> {
    let msg = proto::Ukey2Message::decode(bytes).context("invalid UKEY2 message")?;
    let ty = msg.message_type.unwrap_or_default();
    if ty == ukey2_type::ALERT {
        let alert = proto::Ukey2Alert::decode(msg.message_data.as_deref().unwrap_or_default())
            .unwrap_or_default();
        bail!(
            "peer aborted the handshake: {}",
            alert.error_message.unwrap_or_default()
        );
    }
    ensure!(
        ty == expected,
        "unexpected UKEY2 message type {ty} (wanted {expected})"
    );
    msg.message_data
        .ok_or_else(|| anyhow!("UKEY2 message without data"))
}

/// Initiator side of the UKEY2 handshake.
pub struct Ukey2Client {
    secret: EphemeralSecret,
    client_init: Vec<u8>,
    client_finish: Vec<u8>,
}

impl Ukey2Client {
    pub fn new() -> Self {
        let secret = EphemeralSecret::generate();
        let finished = proto::Ukey2ClientFinished {
            public_key: Some(encode_public_key(&secret.public_key())),
        };
        let client_finish = wrap_ukey2(ukey2_type::CLIENT_FINISH, finished.encode_to_vec());
        let commitment = Sha512::digest(&client_finish).to_vec();
        let init = proto::Ukey2ClientInit {
            version: Some(1),
            random: Some(random_vec(32)),
            cipher_commitments: vec![proto::CipherCommitment {
                handshake_cipher: Some(proto::HANDSHAKE_CIPHER_P256_SHA512),
                commitment: Some(commitment),
            }],
            next_protocol: Some(proto::NEXT_PROTOCOL.to_owned()),
        };
        let client_init = wrap_ukey2(ukey2_type::CLIENT_INIT, init.encode_to_vec());
        Self {
            secret,
            client_init,
            client_finish,
        }
    }

    pub fn client_init(&self) -> &[u8] {
        &self.client_init
    }

    /// Process the responder's ServerInit. Returns the ClientFinish message to
    /// send and the derived session.
    pub fn finish(self, server_init: &[u8]) -> Result<(Vec<u8>, Handshake)> {
        let data = unwrap_ukey2(server_init, ukey2_type::SERVER_INIT)?;
        let init = proto::Ukey2ServerInit::decode(data.as_slice()).context("invalid ServerInit")?;
        ensure!(
            init.version == Some(1),
            "unsupported UKEY2 version {:?}",
            init.version
        );
        ensure!(
            init.handshake_cipher == Some(proto::HANDSHAKE_CIPHER_P256_SHA512),
            "unsupported handshake cipher {:?}",
            init.handshake_cipher
        );
        let peer = decode_public_key(init.public_key.as_deref().unwrap_or_default())?;
        let shared = self.secret.diffie_hellman(&peer);
        let hs = derive(
            shared.raw_secret_bytes().as_slice(),
            &self.client_init,
            server_init,
            true,
        );
        Ok((self.client_finish, hs))
    }
}

/// Responder side of the UKEY2 handshake.
pub struct Ukey2Server {
    secret: EphemeralSecret,
    client_init: Vec<u8>,
    server_init: Vec<u8>,
    commitment: Vec<u8>,
}

impl Ukey2Server {
    pub fn new(client_init: &[u8]) -> Result<Self> {
        let data = unwrap_ukey2(client_init, ukey2_type::CLIENT_INIT)?;
        let init = proto::Ukey2ClientInit::decode(data.as_slice()).context("invalid ClientInit")?;
        ensure!(
            init.version == Some(1),
            "unsupported UKEY2 version {:?}",
            init.version
        );
        ensure!(
            init.random.as_ref().is_some_and(|r| r.len() == 32),
            "invalid ClientInit random"
        );
        let commitment = init
            .cipher_commitments
            .iter()
            .find(|c| c.handshake_cipher == Some(proto::HANDSHAKE_CIPHER_P256_SHA512))
            .and_then(|c| c.commitment.clone())
            .ok_or_else(|| anyhow!("peer does not offer P256_SHA512"))?;
        let secret = EphemeralSecret::generate();
        let server = proto::Ukey2ServerInit {
            version: Some(1),
            random: Some(random_vec(32)),
            handshake_cipher: Some(proto::HANDSHAKE_CIPHER_P256_SHA512),
            public_key: Some(encode_public_key(&secret.public_key())),
        };
        let server_init = wrap_ukey2(ukey2_type::SERVER_INIT, server.encode_to_vec());
        Ok(Self {
            secret,
            client_init: client_init.to_vec(),
            server_init,
            commitment,
        })
    }

    pub fn server_init(&self) -> &[u8] {
        &self.server_init
    }

    pub fn finish(self, client_finish: &[u8]) -> Result<Handshake> {
        let digest = Sha512::digest(client_finish);
        ensure!(
            digest.as_slice() == self.commitment.as_slice(),
            "ClientFinish does not match commitment"
        );
        let data = unwrap_ukey2(client_finish, ukey2_type::CLIENT_FINISH)?;
        let fin = proto::Ukey2ClientFinished::decode(data.as_slice())
            .context("invalid ClientFinished")?;
        let peer = decode_public_key(fin.public_key.as_deref().unwrap_or_default())?;
        let shared = self.secret.diffie_hellman(&peer);
        Ok(derive(
            shared.raw_secret_bytes().as_slice(),
            &self.client_init,
            &self.server_init,
            false,
        ))
    }
}

/// Outgoing half of an established D2D channel.
pub struct Sealer {
    key: [u8; 32],
    hmac_key: [u8; 32],
    seq: i32,
}

/// Incoming half of an established D2D channel.
pub struct Opener {
    key: [u8; 32],
    hmac_key: [u8; 32],
    seq: i32,
}

impl Keys {
    pub fn split(self) -> (Sealer, Opener) {
        (
            Sealer {
                key: self.encrypt,
                hmac_key: self.send_hmac,
                seq: 0,
            },
            Opener {
                key: self.decrypt,
                hmac_key: self.recv_hmac,
                seq: 0,
            },
        )
    }
}

impl Sealer {
    /// Encrypt and authenticate a serialized OfflineFrame.
    pub fn seal(&mut self, offline_frame: &[u8]) -> Vec<u8> {
        self.seq = self.seq.wrapping_add(1);
        let d2d = proto::DeviceToDeviceMessage {
            message: Some(offline_frame.to_vec()),
            sequence_number: Some(self.seq),
        }
        .encode_to_vec();
        let iv: [u8; 16] = random_bytes();
        let body = Aes256CbcEnc::new_from_slices(&self.key, &iv)
            .expect("key and IV have valid lengths")
            .encrypt_padded_vec::<Pkcs7>(&d2d);
        let metadata = proto::GcmMetadata {
            r#type: proto::GCM_METADATA_DEVICE_TO_DEVICE_MESSAGE,
            version: Some(1),
        }
        .encode_to_vec();
        let header_and_body = proto::HeaderAndBody {
            header: proto::Header {
                signature_scheme: proto::SIG_SCHEME_HMAC_SHA256,
                encryption_scheme: proto::ENC_SCHEME_AES_256_CBC,
                iv: Some(iv.to_vec()),
                public_metadata: Some(metadata),
                ..Default::default()
            },
            body,
        }
        .encode_to_vec();
        let mut mac =
            <HmacSha256 as KeyInit>::new_from_slice(&self.hmac_key).expect("any key length works");
        mac.update(&header_and_body);
        let signature = mac.finalize().into_bytes().to_vec();
        proto::SecureMessage {
            header_and_body,
            signature,
        }
        .encode_to_vec()
    }
}

impl Opener {
    /// Verify and decrypt a SecureMessage, returning the inner OfflineFrame bytes.
    pub fn open(&mut self, secure_message: &[u8]) -> Result<Vec<u8>> {
        let msg = proto::SecureMessage::decode(secure_message).context("invalid SecureMessage")?;
        let mut mac =
            <HmacSha256 as KeyInit>::new_from_slice(&self.hmac_key).expect("any key length works");
        mac.update(&msg.header_and_body);
        mac.verify_slice(&msg.signature)
            .map_err(|_| anyhow!("SecureMessage signature mismatch"))?;
        let hb = proto::HeaderAndBody::decode(msg.header_and_body.as_slice())
            .context("invalid HeaderAndBody")?;
        let iv = hb
            .header
            .iv
            .ok_or_else(|| anyhow!("SecureMessage without IV"))?;
        let plain = Aes256CbcDec::new_from_slices(&self.key, &iv)
            .map_err(|_| anyhow!("invalid IV length"))?
            .decrypt_padded_vec::<Pkcs7>(&hb.body)
            .map_err(|_| anyhow!("SecureMessage padding error"))?;
        let d2d = proto::DeviceToDeviceMessage::decode(plain.as_slice())
            .context("invalid DeviceToDeviceMessage")?;
        self.seq = self.seq.wrapping_add(1);
        let seq = d2d.sequence_number.unwrap_or_default();
        ensure!(
            seq == self.seq,
            "unexpected sequence number {seq} (wanted {})",
            self.seq
        );
        d2d.message
            .ok_or_else(|| anyhow!("empty DeviceToDeviceMessage"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_roundtrip() {
        let client = Ukey2Client::new();
        let server = Ukey2Server::new(client.client_init()).unwrap();
        let server_init = server.server_init().to_vec();
        let (finish, client_hs) = client.finish(&server_init).unwrap();
        let server_hs = server.finish(&finish).unwrap();
        assert_eq!(client_hs.pin, server_hs.pin);
        assert_eq!(client_hs.pin.len(), 4);

        let (mut c_seal, mut c_open) = client_hs.keys.split();
        let (mut s_seal, mut s_open) = server_hs.keys.split();
        for i in 0..3u8 {
            let msg = vec![i; 100 + i as usize];
            assert_eq!(s_open.open(&c_seal.seal(&msg)).unwrap(), msg);
            assert_eq!(c_open.open(&s_seal.seal(&msg)).unwrap(), msg);
        }
    }

    #[test]
    fn coordinates() {
        assert_eq!(encode_coordinate(&[0x80, 1]), vec![0, 0x80, 1]);
        assert_eq!(decode_coordinate(&[0, 0xff]).unwrap()[31], 0xff);
        assert!(decode_coordinate(&[1; 33]).is_err());
    }
}
