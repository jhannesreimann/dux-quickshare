//! Encoding of the identity a Quick Share device advertises: the endpoint
//! info blob, the mDNS instance name and the Bluetooth LE advertisement.

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE_NO_PAD, URL_SAFE_NO_PAD_INDIFFERENT};
use serde::Serialize;

use crate::crypto::random_bytes;

/// First 3 bytes of SHA-256("NearbySharing").
pub const SERVICE_ID_HASH: [u8; 3] = [0xFC, 0x9F, 0x5E];
/// mDNS service type: first 6 bytes of SHA-256("NearbySharing").
pub const MDNS_SERVICE_TYPE: &str = "_FC9F5ED42C8A._tcp.local.";
/// RFCOMM service UUID (Java `UUID.nameUUIDFromBytes("NearbySharing")`).
pub const RFCOMM_UUID: &str = "a82efa21-ae5c-3dde-9bbc-f16da7b16c5a";
/// BLE service UUID carrying Nearby Connections advertisements.
pub const BLE_COPRESENCE_UUID: &str = "0000fef3-0000-1000-8000-00805f9b34fb";
/// BLE service UUID of the Quick Share "someone nearby is sharing" signal.
pub const BLE_SHARING_UUID: &str = "0000fe2c-0000-1000-8000-00805f9b34fb";
/// Service data of the "fast initiation" advertisement a sharing device sends.
pub const FAST_INIT_DATA: [u8; 24] = [
    0xFC, 0x12, 0x8E, 0x01, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xBF, 0x2D,
    0x5B, 0xA0, 0xE1, 0xD8, 0x75, 0x24, 0xCA, 0x00,
];

/// Nearby Connections PCP byte: version 1, P2P_POINT_TO_POINT.
const PCP: u8 = 0x23;
/// Longest device name we put on the air (bytes).
const MAX_NAME_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Unknown,
    Phone,
    Tablet,
    Laptop,
}

impl DeviceType {
    pub fn from_bits(v: u8) -> Self {
        match v {
            1 => Self::Phone,
            2 => Self::Tablet,
            3 => Self::Laptop,
            _ => Self::Unknown,
        }
    }
    fn bits(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Phone => 1,
            Self::Tablet => 2,
            Self::Laptop => 3,
        }
    }
}

/// Decoded remote endpoint info.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub name: Option<String>,
    pub device_type: DeviceType,
    pub visible: bool,
}

pub fn parse_endpoint_info(info: &[u8]) -> Option<RemoteInfo> {
    let first = *info.first()?;
    let visible = (first >> 4) & 1 == 0;
    let device_type = DeviceType::from_bits((first >> 1) & 0x07);
    let name = if info.len() > 17 {
        let len = info[17] as usize;
        info.get(18..18 + len)
            .map(|b| String::from_utf8_lossy(b).into_owned())
    } else {
        None
    };
    Some(RemoteInfo {
        name: name.filter(|n| !n.is_empty()),
        device_type,
        visible,
    })
}

pub fn truncate_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if out.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// Our own identity on the network.
#[derive(Debug, Clone)]
pub struct LocalEndpoint {
    pub endpoint_id: String,
    pub name: String,
    pub device_type: DeviceType,
    salt: [u8; 16],
    token: [u8; 2],
    /// Random "dummy service id" mixed into the advertisement header.
    dummy_service_id: Vec<u8>,
}

impl LocalEndpoint {
    pub fn new(name: &str) -> Self {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let raw: [u8; 4] = random_bytes();
        let endpoint_id = raw
            .iter()
            .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
            .collect();
        Self {
            endpoint_id,
            name: truncate_name(name),
            device_type: DeviceType::Laptop,
            salt: random_bytes(),
            token: random_bytes(),
            dummy_service_id: crate::crypto::random_vec(128),
        }
    }

    /// The 17-byte advertisement header for the legacy (GATT) advertisement:
    /// [version 2|extended flag|slot count][bloom filter 10][hash 4][PSM 2].
    /// Scanners that cannot use the extended advertisement read the full
    /// advertisement from GATT slot 0 instead.
    pub fn ble_advertisement_header(&self, slot0: &[u8], extended: bool, psm: u16) -> Vec<u8> {
        let mut bloom = [0u8; 10];
        bloom_add(&mut bloom, &self.dummy_service_id);
        bloom_add(&mut bloom, SERVICE_ID.as_bytes());
        let seed = sha256_4(&[&self.dummy_service_id]);
        let hash = sha256_4(&[&seed, slot0]);
        let mut out = Vec::with_capacity(17);
        out.push((2 << 5) | (u8::from(extended) << 4) | 1);
        out.extend_from_slice(&bloom);
        out.extend_from_slice(&hash);
        out.extend_from_slice(&psm.to_be_bytes());
        out
    }

    /// The header as it goes on air: URL-safe Base64 without padding (23
    /// bytes), which is what Android devices advertise and expect.
    pub fn ble_advertisement_header_b64(&self, slot0: &[u8], extended: bool, psm: u16) -> Vec<u8> {
        URL_SAFE_NO_PAD
            .encode(self.ble_advertisement_header(slot0, extended, psm))
            .into_bytes()
    }

    pub fn set_name(&mut self, name: &str) {
        self.name = truncate_name(name);
    }

    /// Endpoint info: [version|visibility|type] + 16 random bytes + name.
    pub fn endpoint_info(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let mut v = Vec::with_capacity(18 + name.len());
        v.push((1 << 5) | (self.device_type.bits() << 1));
        v.extend_from_slice(&self.salt);
        v.push(name.len() as u8);
        v.extend_from_slice(name);
        v
    }

    pub fn endpoint_info_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.endpoint_info())
    }

    /// mDNS instance name: base64url([PCP][endpoint id][service hash][0,0]).
    pub fn mdns_instance_name(&self) -> String {
        let mut v = Vec::with_capacity(10);
        v.push(PCP);
        v.extend_from_slice(self.endpoint_id.as_bytes());
        v.extend_from_slice(&SERVICE_ID_HASH);
        v.extend_from_slice(&[0, 0]);
        URL_SAFE_NO_PAD.encode(v)
    }

    /// BLE (v2, extended) advertisement service data under UUID 0xFEF3,
    /// in the same layout Android uses for its receive advertisement.
    /// With a PSM, phones can connect over a BLE L2CAP channel (no pairing).
    pub fn ble_advertisement(&self, bt_mac: [u8; 6], psm: Option<u16>) -> Vec<u8> {
        let info = self.endpoint_info();
        let mut data = Vec::with_capacity(16 + info.len());
        data.push(PCP);
        data.extend_from_slice(&SERVICE_ID_HASH);
        data.extend_from_slice(self.endpoint_id.as_bytes());
        data.push(info.len() as u8);
        data.extend_from_slice(&info);
        data.extend_from_slice(&bt_mac);
        data.push(0); // UWB address length
        data.push(0); // extra field

        let mut adv = Vec::with_capacity(10 + data.len());
        adv.push((2 << 5) | (2 << 2)); // version 2, socket version 2, regular advertisement
        adv.extend_from_slice(&SERVICE_ID_HASH);
        adv.extend_from_slice(&(data.len() as u32).to_be_bytes());
        adv.extend_from_slice(&data);
        adv.extend_from_slice(&self.token);
        if let Some(psm) = psm {
            adv.push(0x01); // extra fields: PSM
            adv.extend_from_slice(&psm.to_be_bytes());
        }
        adv
    }
}

/// Nearby Connections service id of Quick Share.
pub const SERVICE_ID: &str = "NearbySharing";
/// GATT characteristic holding advertisement slot 0 (under service 0xFEF3).
pub const GATT_SLOT0_UUID: &str = "00000000-0000-3000-8000-000000000000";

/// MurmurHash3 x64 128-bit (reference algorithm), returns (h1, h2).
pub fn murmur3_x64_128(data: &[u8], seed: u32) -> (u64, u64) {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    fn fmix(mut k: u64) -> u64 {
        k ^= k >> 33;
        k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
        k ^= k >> 33;
        k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        k ^ (k >> 33)
    }
    let (mut h1, mut h2) = (u64::from(seed), u64::from(seed));
    let blocks = data.chunks_exact(16);
    let tail = blocks.remainder();
    for block in blocks {
        let k1 = u64::from_le_bytes(block[..8].try_into().unwrap());
        let k2 = u64::from_le_bytes(block[8..].try_into().unwrap());
        h1 ^= k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
        h1 = h1
            .rotate_left(27)
            .wrapping_add(h2)
            .wrapping_mul(5)
            .wrapping_add(0x52dc_e729);
        h2 ^= k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
        h2 = h2
            .rotate_left(31)
            .wrapping_add(h1)
            .wrapping_mul(5)
            .wrapping_add(0x3849_5ab5);
    }
    let (mut k1, mut k2) = (0u64, 0u64);
    for (i, b) in tail.iter().enumerate() {
        if i < 8 {
            k1 ^= u64::from(*b) << (i * 8);
        } else {
            k2 ^= u64::from(*b) << ((i - 8) * 8);
        }
    }
    if tail.len() > 8 {
        h2 ^= k2.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
    }
    if !tail.is_empty() {
        h1 ^= k1.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
    }
    let len = data.len() as u64;
    h1 ^= len;
    h2 ^= len;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    (h1, h2)
}

/// Add an item to the 80-bit service id bloom filter of a BLE advertisement
/// header (5 hashes derived from MurmurHash3, like Nearby Connections).
pub fn bloom_add(filter: &mut [u8; 10], item: &[u8]) {
    let (h, _) = murmur3_x64_128(item, 0);
    let hash1 = h as u32 as i32;
    let hash2 = (h >> 32) as u32 as i32;
    for i in 1..=5i32 {
        let mut combined = hash1.wrapping_add(i.wrapping_mul(hash2));
        if combined < 0 {
            combined = !combined;
        }
        let pos = combined as usize % 80;
        filter[pos / 8] |= 1 << (pos % 8);
    }
}

#[cfg(test)]
pub fn bloom_contains(filter: &[u8; 10], item: &[u8]) -> bool {
    let mut probe = [0u8; 10];
    bloom_add(&mut probe, item);
    probe.iter().zip(filter).all(|(p, f)| p & f == *p)
}

fn sha256_4(parts: &[&[u8]]) -> [u8; 4] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize()[..4].try_into().expect("4 bytes")
}

/// A Quick Share receiver found through BLE.
#[derive(Debug, Clone)]
pub struct BleEndpoint {
    pub endpoint_id: String,
    pub info: RemoteInfo,
    pub bt_mac: [u8; 6],
}

/// Parse a 0xFEF3 service data blob. Returns `None` for anything that is not
/// a visible Quick Share receiver (fast advertisements, other services, ...).
pub fn parse_ble_advertisement(adv: &[u8]) -> Option<BleEndpoint> {
    let first = *adv.first()?;
    let fast = (first >> 1) & 1 == 1;
    if fast || adv.get(1..4)? != SERVICE_ID_HASH {
        return None;
    }
    let len = u32::from_be_bytes(adv.get(4..8)?.try_into().ok()?) as usize;
    let data = adv.get(8..8 + len)?;
    if data.first()? & 0x1F != PCP & 0x1F || data.get(1..4)? != SERVICE_ID_HASH {
        return None;
    }
    let endpoint_id = String::from_utf8(data.get(4..8)?.to_vec()).ok()?;
    let info_len = *data.get(8)? as usize;
    let info = parse_endpoint_info(data.get(9..9 + info_len)?)?;
    let mac: [u8; 6] = data.get(9 + info_len..15 + info_len)?.try_into().ok()?;
    if mac == [0; 6] {
        return None;
    }
    Some(BleEndpoint {
        endpoint_id,
        info,
        bt_mac: mac,
    })
}

/// Parse an mDNS instance name into the endpoint id it carries.
pub fn parse_mdns_instance_name(name: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD_INDIFFERENT.decode(name).ok()?;
    if bytes.len() < 10 || bytes[0] & 0x1F != PCP & 0x1F || bytes[5..8] != SERVICE_ID_HASH {
        return None;
    }
    String::from_utf8(bytes[1..5].to_vec()).ok()
}

pub fn decode_b64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD_INDIFFERENT.decode(s.trim()).ok()
}

pub fn mac_to_string(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<u8> = s
        .split(':')
        .filter_map(|p| u8::from_str_radix(p, 16).ok())
        .collect();
    parts.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    #[test]
    fn parses_real_android_advertisement() {
        // Captured from an OPPO Find X9 Pro in receive mode.
        let adv = hex(
            "48 fc 9f 5e 00 00 00 2a 23 fc 9f 5e 30 35 32 36 19 22 ae f5 2e 87 c4 0c f2 3b 13 ee 68 e0 \
             8a 9b a9 06 07 4a 68 61 6e 6e 65 73 ec 2f 90 89 95 ae 00 00 92 2f 01 00 90",
        );
        let ep = parse_ble_advertisement(&adv).unwrap();
        assert_eq!(ep.endpoint_id, "0526");
        assert_eq!(ep.info.name.as_deref(), Some("Jhannes"));
        assert_eq!(ep.info.device_type, DeviceType::Phone);
        assert!(ep.info.visible);
        assert_eq!(mac_to_string(&ep.bt_mac), "EC:2F:90:89:95:AE");
    }

    #[test]
    fn rejects_fast_advertisement() {
        let adv =
            hex("4a 17 23 30 35 32 36 11 32 12 94 9f 35 b3 14 16 5d c0 a8 c6 37 f6 82 16 59 ec b7");
        assert!(parse_ble_advertisement(&adv).is_none());
    }

    #[test]
    fn own_advertisement_roundtrips() {
        let ep = LocalEndpoint::new("Dux Laptop");
        let adv = ep.ble_advertisement([1, 2, 3, 4, 5, 6], None);
        let parsed = parse_ble_advertisement(&adv).unwrap();
        assert_eq!(parsed.endpoint_id, ep.endpoint_id);
        assert_eq!(parsed.info.name.as_deref(), Some("Dux Laptop"));
        assert_eq!(parsed.info.device_type, DeviceType::Laptop);
        assert_eq!(parsed.bt_mac, [1, 2, 3, 4, 5, 6]);
        assert_eq!(
            parse_mdns_instance_name(&ep.mdns_instance_name()),
            Some(ep.endpoint_id.clone())
        );
        assert!(adv.len() <= 200);
    }

    #[test]
    fn murmur3_reference_vectors() {
        assert_eq!(murmur3_x64_128(b"", 0), (0, 0));
        assert_eq!(
            murmur3_x64_128(b"hello", 0),
            (0xcbd8a7b341bd9b02, 0x5b1e906a48ae1d19)
        );
    }

    #[test]
    fn advertisement_header_layout() {
        let ep = LocalEndpoint::new("Dux Laptop");
        let adv = ep.ble_advertisement([1, 2, 3, 4, 5, 6], None);
        let header = ep.ble_advertisement_header(&adv, true, 0);
        assert_eq!(header.len(), 17);
        assert_eq!(header[0], 0b0101_0001); // version 2, extended, 1 slot
        let bloom: [u8; 10] = header[1..11].try_into().unwrap();
        assert!(bloom_contains(&bloom, SERVICE_ID.as_bytes()));
        assert!(bloom.iter().map(|b| b.count_ones()).sum::<u32>() <= 10);
        let b64 = ep.ble_advertisement_header_b64(&adv, true, 0);
        assert_eq!(b64.len(), 23);
        assert_eq!(
            decode_b64(std::str::from_utf8(&b64).unwrap())
                .unwrap()
                .len(),
            17
        );
    }

    #[test]
    fn decodes_real_android_header() {
        // Base64 header advertised by an Android phone nearby.
        let raw = decode_b64("VWAAgBACCQADIADIESABAIw").unwrap();
        assert_eq!(raw.len(), 17);
        assert_eq!(raw[0] >> 5, 2); // version 2
        assert_eq!((raw[0] >> 4) & 1, 1); // extended advertisement available
    }

    #[test]
    fn parses_android_mdns_record() {
        assert_eq!(
            parse_mdns_instance_name("IzFBUjb8n14AAA").as_deref(),
            Some("1AR6")
        );
        let info =
            parse_endpoint_info(&decode_b64("Bl6jXgVMMPwjmWGh9zqOu2AJYXJjaGxpbnV4").unwrap())
                .unwrap();
        assert_eq!(info.name.as_deref(), Some("archlinux"));
        assert_eq!(info.device_type, DeviceType::Laptop);
    }
}
