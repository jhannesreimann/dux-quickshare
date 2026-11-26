//! Protocol buffer messages used by Quick Share (Nearby Share).
//!
//! Only the subset needed for file/text transfer is declared. Field numbers
//! follow the public `.proto` definitions of Google's Nearby project
//! (`securegcm`, `securemessage`, `location.nearby.connections` and
//! `sharing.nearby`). Enum fields are declared as `int32`, which is
//! wire-compatible with protobuf enums and keeps unknown values decodable.

#![allow(clippy::derive_partial_eq_without_eq)]

use prost::Message;

// ----------------------------------------------------------------------------
// UKEY2 (securegcm/ukey.proto)
// ----------------------------------------------------------------------------

pub mod ukey2_type {
    pub const ALERT: i32 = 1;
    pub const CLIENT_INIT: i32 = 2;
    pub const SERVER_INIT: i32 = 3;
    pub const CLIENT_FINISH: i32 = 4;
}

pub const HANDSHAKE_CIPHER_P256_SHA512: i32 = 100;
pub const NEXT_PROTOCOL: &str = "AES_256_CBC-HMAC_SHA256";

#[derive(Clone, PartialEq, Message)]
pub struct Ukey2Message {
    #[prost(int32, optional, tag = "1")]
    pub message_type: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub message_data: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ukey2Alert {
    #[prost(int32, optional, tag = "1")]
    pub r#type: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub error_message: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CipherCommitment {
    #[prost(int32, optional, tag = "1")]
    pub handshake_cipher: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub commitment: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ukey2ClientInit {
    #[prost(int32, optional, tag = "1")]
    pub version: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub random: Option<Vec<u8>>,
    #[prost(message, repeated, tag = "3")]
    pub cipher_commitments: Vec<CipherCommitment>,
    #[prost(string, optional, tag = "4")]
    pub next_protocol: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ukey2ServerInit {
    #[prost(int32, optional, tag = "1")]
    pub version: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub random: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "3")]
    pub handshake_cipher: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub public_key: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ukey2ClientFinished {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub public_key: Option<Vec<u8>>,
}

// ----------------------------------------------------------------------------
// SecureMessage (securemessage.proto, securegcm.proto, device_to_device_messages.proto)
// ----------------------------------------------------------------------------

pub const PUBLIC_KEY_TYPE_EC_P256: i32 = 1;
pub const SIG_SCHEME_HMAC_SHA256: i32 = 1;
pub const ENC_SCHEME_AES_256_CBC: i32 = 2;
pub const GCM_METADATA_DEVICE_TO_DEVICE_MESSAGE: i32 = 13;

#[derive(Clone, PartialEq, Message)]
pub struct EcP256PublicKey {
    #[prost(bytes = "vec", required, tag = "1")]
    pub x: Vec<u8>,
    #[prost(bytes = "vec", required, tag = "2")]
    pub y: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GenericPublicKey {
    #[prost(int32, required, tag = "1")]
    pub r#type: i32,
    #[prost(message, optional, tag = "2")]
    pub ec_p256_public_key: Option<EcP256PublicKey>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SecureMessage {
    #[prost(bytes = "vec", required, tag = "1")]
    pub header_and_body: Vec<u8>,
    #[prost(bytes = "vec", required, tag = "2")]
    pub signature: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Header {
    #[prost(int32, required, tag = "1")]
    pub signature_scheme: i32,
    #[prost(int32, required, tag = "2")]
    pub encryption_scheme: i32,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub verification_key_id: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub decryption_key_id: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "5")]
    pub iv: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "6")]
    pub public_metadata: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "7")]
    pub associated_data_length: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct HeaderAndBody {
    #[prost(message, required, tag = "1")]
    pub header: Header,
    #[prost(bytes = "vec", required, tag = "2")]
    pub body: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GcmMetadata {
    #[prost(int32, required, tag = "1")]
    pub r#type: i32,
    #[prost(int32, optional, tag = "2")]
    pub version: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeviceToDeviceMessage {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub message: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "2")]
    pub sequence_number: Option<i32>,
}

// ----------------------------------------------------------------------------
// Nearby Connections (offline_wire_formats.proto)
// ----------------------------------------------------------------------------

pub mod frame_type {
    pub const CONNECTION_REQUEST: i32 = 1;
    pub const CONNECTION_RESPONSE: i32 = 2;
    pub const PAYLOAD_TRANSFER: i32 = 3;
    pub const BANDWIDTH_UPGRADE_NEGOTIATION: i32 = 4;
    pub const KEEP_ALIVE: i32 = 5;
    pub const DISCONNECTION: i32 = 6;
    pub const BANDWIDTH_UPGRADE_RETRY: i32 = 12;
}

pub mod medium {
    pub const BLUETOOTH: i32 = 2;
    pub const WIFI_HOTSPOT: i32 = 3;
    pub const BLE: i32 = 4;
    pub const WIFI_LAN: i32 = 5;
    pub const WIFI_AWARE: i32 = 6;
    pub const WIFI_DIRECT: i32 = 8;
    pub const WEB_RTC: i32 = 9;
    pub const BLE_L2CAP: i32 = 10;

    pub fn name(m: i32) -> &'static str {
        match m {
            BLUETOOTH => "bluetooth",
            WIFI_HOTSPOT => "hotspot",
            BLE | BLE_L2CAP => "ble",
            WIFI_LAN => "wifi",
            WIFI_AWARE => "wifi-aware",
            WIFI_DIRECT => "wifi-direct",
            WEB_RTC => "webrtc",
            _ => "unknown",
        }
    }
}

pub const CONNECTION_RESPONSE_ACCEPT: i32 = 1;
pub const OS_TYPE_LINUX: i32 = 100;

pub mod payload_type {
    pub const BYTES: i32 = 1;
    pub const FILE: i32 = 2;
}

pub const PACKET_TYPE_DATA: i32 = 1;
pub const PACKET_TYPE_CONTROL: i32 = 2;
pub const CHUNK_FLAG_LAST: i32 = 1;

pub mod bwu_event {
    pub const UPGRADE_PATH_AVAILABLE: i32 = 1;
    pub const LAST_WRITE_TO_PRIOR_CHANNEL: i32 = 2;
    pub const SAFE_TO_CLOSE_PRIOR_CHANNEL: i32 = 3;
    pub const CLIENT_INTRODUCTION: i32 = 4;
    pub const UPGRADE_FAILURE: i32 = 5;
    pub const CLIENT_INTRODUCTION_ACK: i32 = 6;
    pub const UPGRADE_PATH_REQUEST: i32 = 7;
}

#[derive(Clone, PartialEq, Message)]
pub struct OfflineFrame {
    #[prost(int32, optional, tag = "1")]
    pub version: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub v1: Option<V1Frame>,
}

#[derive(Clone, PartialEq, Message)]
pub struct V1Frame {
    #[prost(int32, optional, tag = "1")]
    pub r#type: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub connection_request: Option<ConnectionRequestFrame>,
    #[prost(message, optional, tag = "3")]
    pub connection_response: Option<ConnectionResponseFrame>,
    #[prost(message, optional, tag = "4")]
    pub payload_transfer: Option<PayloadTransferFrame>,
    #[prost(message, optional, tag = "5")]
    pub bandwidth_upgrade_negotiation: Option<BandwidthUpgradeNegotiationFrame>,
    #[prost(message, optional, tag = "6")]
    pub keep_alive: Option<KeepAliveFrame>,
    #[prost(message, optional, tag = "7")]
    pub disconnection: Option<DisconnectionFrame>,
    #[prost(message, optional, tag = "13")]
    pub bandwidth_upgrade_retry: Option<BandwidthUpgradeRetryFrame>,
}

/// Sent when a device gains new upgrade options mid-connection (e.g. it
/// just (re)connected to Wi-Fi), asking the other side to try again.
#[derive(Clone, PartialEq, Message)]
pub struct BandwidthUpgradeRetryFrame {
    #[prost(int32, repeated, packed = "false", tag = "1")]
    pub supported_medium: Vec<i32>,
    #[prost(bool, optional, tag = "2")]
    pub is_request: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectionRequestFrame {
    #[prost(string, optional, tag = "1")]
    pub endpoint_id: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub endpoint_name: Option<String>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub handshake_data: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "4")]
    pub nonce: Option<i32>,
    #[prost(int32, repeated, packed = "false", tag = "5")]
    pub mediums: Vec<i32>,
    #[prost(bytes = "vec", optional, tag = "6")]
    pub endpoint_info: Option<Vec<u8>>,
    #[prost(message, optional, tag = "7")]
    pub medium_metadata: Option<MediumMetadata>,
    #[prost(int32, optional, tag = "8")]
    pub keep_alive_interval_millis: Option<i32>,
    #[prost(int32, optional, tag = "9")]
    pub keep_alive_timeout_millis: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OsInfo {
    #[prost(int32, optional, tag = "1")]
    pub r#type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectionResponseFrame {
    #[prost(int32, optional, tag = "1")]
    pub status: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub handshake_data: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "3")]
    pub response: Option<i32>,
    #[prost(message, optional, tag = "4")]
    pub os_info: Option<OsInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PayloadHeader {
    #[prost(int64, optional, tag = "1")]
    pub id: Option<i64>,
    #[prost(int32, optional, tag = "2")]
    pub r#type: Option<i32>,
    #[prost(int64, optional, tag = "3")]
    pub total_size: Option<i64>,
    #[prost(bool, optional, tag = "4")]
    pub is_sensitive: Option<bool>,
    #[prost(string, optional, tag = "5")]
    pub file_name: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub parent_folder: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PayloadChunk {
    #[prost(int32, optional, tag = "1")]
    pub flags: Option<i32>,
    #[prost(int64, optional, tag = "2")]
    pub offset: Option<i64>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub body: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ControlMessage {
    #[prost(int32, optional, tag = "1")]
    pub event: Option<i32>,
    #[prost(int64, optional, tag = "2")]
    pub offset: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PayloadTransferFrame {
    #[prost(int32, optional, tag = "1")]
    pub packet_type: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub payload_header: Option<PayloadHeader>,
    #[prost(message, optional, tag = "3")]
    pub payload_chunk: Option<PayloadChunk>,
    #[prost(message, optional, tag = "4")]
    pub control_message: Option<ControlMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MediumMetadata {
    #[prost(bool, optional, tag = "1")]
    pub supports_5_ghz: Option<bool>,
    #[prost(string, optional, tag = "2")]
    pub bssid: Option<String>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub ip_address: Option<Vec<u8>>,
    #[prost(bool, optional, tag = "4")]
    pub supports_6_ghz: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    pub mobile_radio: Option<bool>,
    #[prost(int32, optional, tag = "6")]
    pub ap_frequency: Option<i32>,
    #[prost(message, optional, tag = "12")]
    pub medium_role: Option<MediumRole>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MediumRole {
    #[prost(bool, optional, tag = "1")]
    pub support_wifi_direct_group_owner: Option<bool>,
    #[prost(bool, optional, tag = "2")]
    pub support_wifi_direct_group_client: Option<bool>,
    #[prost(bool, optional, tag = "3")]
    pub support_wifi_hotspot_host: Option<bool>,
    #[prost(bool, optional, tag = "4")]
    pub support_wifi_hotspot_client: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpgradePathRequest {
    #[prost(int32, repeated, packed = "true", tag = "1")]
    pub mediums: Vec<i32>,
    #[prost(message, optional, tag = "2")]
    pub medium_meta_data: Option<MediumMetadata>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServiceAddress {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub ip_address: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "2")]
    pub port: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WifiHotspotCredentials {
    #[prost(string, optional, tag = "1")]
    pub ssid: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub password: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub port: Option<i32>,
    #[prost(string, optional, tag = "4")]
    pub gateway: Option<String>,
    #[prost(int32, optional, tag = "5")]
    pub frequency: Option<i32>,
    #[prost(message, repeated, tag = "6")]
    pub address_candidates: Vec<ServiceAddress>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WifiLanSocket {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub ip_address: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "2")]
    pub wifi_port: Option<i32>,
    #[prost(message, repeated, tag = "3")]
    pub address_candidates: Vec<ServiceAddress>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WifiDirectCredentials {
    #[prost(string, optional, tag = "1")]
    pub ssid: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub password: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub port: Option<i32>,
    #[prost(int32, optional, tag = "4")]
    pub frequency: Option<i32>,
    #[prost(string, optional, tag = "5")]
    pub gateway: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpgradePathInfo {
    #[prost(int32, optional, tag = "1")]
    pub medium: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub wifi_hotspot_credentials: Option<WifiHotspotCredentials>,
    #[prost(message, optional, tag = "3")]
    pub wifi_lan_socket: Option<WifiLanSocket>,
    #[prost(message, optional, tag = "6")]
    pub wifi_direct_credentials: Option<WifiDirectCredentials>,
    #[prost(bool, optional, tag = "7")]
    pub supports_disabling_encryption: Option<bool>,
    #[prost(bool, optional, tag = "9")]
    pub supports_client_introduction_ack: Option<bool>,
    #[prost(message, optional, tag = "10")]
    pub upgrade_path_request: Option<UpgradePathRequest>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientIntroduction {
    #[prost(string, optional, tag = "1")]
    pub endpoint_id: Option<String>,
    #[prost(bool, optional, tag = "2")]
    pub supports_disabling_encryption: Option<bool>,
    #[prost(string, optional, tag = "3")]
    pub last_endpoint_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientIntroductionAck {}

#[derive(Clone, PartialEq, Message)]
pub struct BandwidthUpgradeNegotiationFrame {
    #[prost(int32, optional, tag = "1")]
    pub event_type: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub upgrade_path_info: Option<UpgradePathInfo>,
    #[prost(message, optional, tag = "3")]
    pub client_introduction: Option<ClientIntroduction>,
    #[prost(message, optional, tag = "4")]
    pub client_introduction_ack: Option<ClientIntroductionAck>,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeepAliveFrame {
    #[prost(bool, optional, tag = "1")]
    pub ack: Option<bool>,
    #[prost(uint32, optional, tag = "2")]
    pub seq_num: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DisconnectionFrame {
    #[prost(bool, optional, tag = "1")]
    pub request_safe_to_disconnect: Option<bool>,
    #[prost(bool, optional, tag = "2")]
    pub ack_safe_to_disconnect: Option<bool>,
}

// ----------------------------------------------------------------------------
// Quick Share sharing frames (wire_format.proto, package sharing.nearby)
// ----------------------------------------------------------------------------

pub mod sharing_type {
    pub const INTRODUCTION: i32 = 1;
    pub const RESPONSE: i32 = 2;
    pub const PAIRED_KEY_ENCRYPTION: i32 = 3;
    pub const PAIRED_KEY_RESULT: i32 = 4;
    pub const CANCEL: i32 = 6;
}

pub mod response_status {
    pub const ACCEPT: i32 = 1;
    pub const REJECT: i32 = 2;
    pub const NOT_ENOUGH_SPACE: i32 = 3;
    pub const UNSUPPORTED_ATTACHMENT_TYPE: i32 = 4;
    pub const TIMED_OUT: i32 = 5;
}

pub const PAIRED_KEY_RESULT_UNABLE: i32 = 3;

pub mod file_type {
    pub const UNKNOWN: i32 = 0;
    pub const IMAGE: i32 = 1;
    pub const VIDEO: i32 = 2;
    pub const AUDIO: i32 = 4;
    pub const DOCUMENT: i32 = 5;
}

pub mod text_type {
    pub const TEXT: i32 = 1;
    pub const URL: i32 = 2;
    pub const ADDRESS: i32 = 3;
    pub const PHONE_NUMBER: i32 = 4;
}

#[derive(Clone, PartialEq, Message)]
pub struct SharingFrame {
    #[prost(int32, optional, tag = "1")]
    pub version: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub v1: Option<SharingV1Frame>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SharingV1Frame {
    #[prost(int32, optional, tag = "1")]
    pub r#type: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub introduction: Option<IntroductionFrame>,
    #[prost(message, optional, tag = "3")]
    pub connection_response: Option<SharingResponseFrame>,
    #[prost(message, optional, tag = "4")]
    pub paired_key_encryption: Option<PairedKeyEncryptionFrame>,
    #[prost(message, optional, tag = "5")]
    pub paired_key_result: Option<PairedKeyResultFrame>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileMetadata {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(int32, optional, tag = "2")]
    pub r#type: Option<i32>,
    #[prost(int64, optional, tag = "3")]
    pub payload_id: Option<i64>,
    #[prost(int64, optional, tag = "4")]
    pub size: Option<i64>,
    #[prost(string, optional, tag = "5")]
    pub mime_type: Option<String>,
    #[prost(int64, optional, tag = "6")]
    pub id: Option<i64>,
    #[prost(string, optional, tag = "7")]
    pub parent_folder: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TextMetadata {
    #[prost(string, optional, tag = "2")]
    pub text_title: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub r#type: Option<i32>,
    #[prost(int64, optional, tag = "4")]
    pub payload_id: Option<i64>,
    #[prost(int64, optional, tag = "5")]
    pub size: Option<i64>,
    #[prost(int64, optional, tag = "6")]
    pub id: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WifiCredentialsMetadata {
    #[prost(string, optional, tag = "2")]
    pub ssid: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub security_type: Option<i32>,
    #[prost(int64, optional, tag = "4")]
    pub payload_id: Option<i64>,
    #[prost(int64, optional, tag = "5")]
    pub id: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WifiCredentials {
    #[prost(string, optional, tag = "1")]
    pub password: Option<String>,
    #[prost(bool, optional, tag = "2")]
    pub hidden_ssid: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct IntroductionFrame {
    #[prost(message, repeated, tag = "1")]
    pub file_metadata: Vec<FileMetadata>,
    #[prost(message, repeated, tag = "2")]
    pub text_metadata: Vec<TextMetadata>,
    #[prost(string, optional, tag = "3")]
    pub required_package: Option<String>,
    #[prost(message, repeated, tag = "4")]
    pub wifi_credentials_metadata: Vec<WifiCredentialsMetadata>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SharingResponseFrame {
    #[prost(int32, optional, tag = "1")]
    pub status: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PairedKeyEncryptionFrame {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub signed_data: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub secret_id_hash: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PairedKeyResultFrame {
    #[prost(int32, optional, tag = "1")]
    pub status: Option<i32>,
}

// ----------------------------------------------------------------------------
// Helpers to build common frames
// ----------------------------------------------------------------------------

pub fn offline(v1: V1Frame) -> OfflineFrame {
    OfflineFrame {
        version: Some(1),
        v1: Some(v1),
    }
}

pub fn sharing(v1: SharingV1Frame) -> SharingFrame {
    SharingFrame {
        version: Some(1),
        v1: Some(v1),
    }
}

pub fn keep_alive(ack: bool) -> OfflineFrame {
    offline(V1Frame {
        r#type: Some(frame_type::KEEP_ALIVE),
        keep_alive: Some(KeepAliveFrame {
            ack: Some(ack),
            seq_num: None,
        }),
        ..Default::default()
    })
}

pub fn disconnection() -> OfflineFrame {
    offline(V1Frame {
        r#type: Some(frame_type::DISCONNECTION),
        disconnection: Some(DisconnectionFrame::default()),
        ..Default::default()
    })
}

pub fn connection_response_accept() -> OfflineFrame {
    offline(V1Frame {
        r#type: Some(frame_type::CONNECTION_RESPONSE),
        connection_response: Some(ConnectionResponseFrame {
            status: Some(0),
            handshake_data: None,
            response: Some(CONNECTION_RESPONSE_ACCEPT),
            os_info: Some(OsInfo {
                r#type: Some(OS_TYPE_LINUX),
            }),
        }),
        ..Default::default()
    })
}

/// A bandwidth upgrade negotiation frame.
pub fn bwu(event: i32, f: impl FnOnce(&mut BandwidthUpgradeNegotiationFrame)) -> OfflineFrame {
    let mut b = BandwidthUpgradeNegotiationFrame {
        event_type: Some(event),
        ..Default::default()
    };
    f(&mut b);
    offline(V1Frame {
        r#type: Some(frame_type::BANDWIDTH_UPGRADE_NEGOTIATION),
        bandwidth_upgrade_negotiation: Some(b),
        ..Default::default()
    })
}

pub fn bwu_failure(info: UpgradePathInfo) -> OfflineFrame {
    bwu(bwu_event::UPGRADE_FAILURE, |b| {
        b.upgrade_path_info = Some(info)
    })
}

/// A payload chunk frame for payload `id`.
pub fn payload_chunk(
    id: i64,
    kind: i32,
    total_size: i64,
    file_name: Option<&str>,
    offset: i64,
    body: Vec<u8>,
    last: bool,
) -> OfflineFrame {
    offline(V1Frame {
        r#type: Some(frame_type::PAYLOAD_TRANSFER),
        payload_transfer: Some(PayloadTransferFrame {
            packet_type: Some(PACKET_TYPE_DATA),
            payload_header: Some(PayloadHeader {
                id: Some(id),
                r#type: Some(kind),
                total_size: Some(total_size),
                is_sensitive: Some(false),
                file_name: file_name.map(str::to_owned),
                parent_folder: None,
            }),
            payload_chunk: Some(PayloadChunk {
                flags: Some(if last { CHUNK_FLAG_LAST } else { 0 }),
                offset: Some(offset),
                body: if last { None } else { Some(body) },
            }),
            control_message: None,
        }),
        ..Default::default()
    })
}
