//! TNS (Transparent Network Substrate) Protocol Implementation
//!
//! TNS is Oracle's proprietary network protocol for database connections.
//! This module implements basic TNS packet parsing and handling.
//!
//! Reference: Oracle Database Net Services Reference

use bytes::{Buf, BufMut, BytesMut};
use std::io::{self, Cursor};

/// TNS packet types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TnsPacketType {
    /// TNS Connect packet
    Connect = 1,
    /// TNS Accept packet
    Accept = 2,
    /// TNS Acknowledge packet (ACK)
    Ack = 3,
    /// TNS Refuse packet
    Refuse = 4,
    /// TNS Redirect packet
    Redirect = 5,
    /// TNS Data packet
    Data = 6,
    /// TNS Null packet
    Null = 7,
    /// TNS Abort packet
    Abort = 9,
    /// TNS Resend packet
    Resend = 11,
    /// TNS Marker packet
    Marker = 12,
    /// TNS Attention packet
    Attention = 13,
    /// TNS Control packet
    Control = 14,
}

impl TnsPacketType {
    /// Convert u8 to TnsPacketType
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Connect),
            2 => Some(Self::Accept),
            3 => Some(Self::Ack),
            4 => Some(Self::Refuse),
            5 => Some(Self::Redirect),
            6 => Some(Self::Data),
            7 => Some(Self::Null),
            9 => Some(Self::Abort),
            11 => Some(Self::Resend),
            12 => Some(Self::Marker),
            13 => Some(Self::Attention),
            14 => Some(Self::Control),
            _ => None,
        }
    }
}

/// TNS packet header
#[derive(Debug, Clone)]
pub struct TnsHeader {
    /// Total packet length (including header)
    pub length: u16,
    /// Packet checksum
    pub checksum: u16,
    /// Packet type
    pub packet_type: TnsPacketType,
    /// Flags
    pub flags: u8,
    /// Header checksum
    pub header_checksum: u16,
}

impl TnsHeader {
    /// TNS header size in bytes
    pub const SIZE: usize = 8;

    /// Parse TNS header from bytes
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Insufficient data for TNS header",
            ));
        }

        let mut cursor = Cursor::new(data);

        // TNS uses big-endian byte order
        let length = cursor.get_u16();
        let checksum = cursor.get_u16();
        let packet_type_byte = cursor.get_u8();
        let flags = cursor.get_u8();
        let header_checksum = cursor.get_u16();

        let packet_type = TnsPacketType::from_u8(packet_type_byte)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid TNS packet type: {}", packet_type_byte),
                )
            })?;

        Ok(Self {
            length,
            checksum,
            packet_type,
            flags,
            header_checksum,
        })
    }

    /// Encode TNS header to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(Self::SIZE);
        buf.put_u16(self.length);
        buf.put_u16(self.checksum);
        buf.put_u8(self.packet_type as u8);
        buf.put_u8(self.flags);
        buf.put_u16(self.header_checksum);
        buf.to_vec()
    }
}

/// TNS Connect packet data
#[derive(Debug, Clone)]
pub struct TnsConnect {
    /// Protocol version
    pub version: u16,
    /// Minimum compatible version
    pub version_compatible: u16,
    /// Service options
    pub service_options: u16,
    /// Session data unit (SDU) size
    pub sdu_size: u16,
    /// Maximum transmission unit (TDU) size
    pub tdu_size: u16,
    /// Protocol characteristics
    pub nt_protocol_characteristics: u16,
    /// Line turnaround value
    pub line_turnaround: u16,
    /// Value of 1 in hardware
    pub value_of_1: u16,
    /// Connect data length
    pub connect_data_length: u16,
    /// Connect data offset
    pub connect_data_offset: u16,
    /// Maximum receivable connect data
    pub max_receivable_connect_data: u32,
    /// Connect flags 0
    pub connect_flags_0: u8,
    /// Connect flags 1
    pub connect_flags_1: u8,
    /// Connect data (connection string)
    pub connect_data: String,
}

impl TnsConnect {
    /// Parse TNS Connect packet
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        if data.len() < 26 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Insufficient data for TNS Connect packet",
            ));
        }

        let mut cursor = Cursor::new(data);

        let version = cursor.get_u16();
        let version_compatible = cursor.get_u16();
        let service_options = cursor.get_u16();
        let sdu_size = cursor.get_u16();
        let tdu_size = cursor.get_u16();
        let nt_protocol_characteristics = cursor.get_u16();
        let line_turnaround = cursor.get_u16();
        let value_of_1 = cursor.get_u16();
        let connect_data_length = cursor.get_u16();
        let connect_data_offset = cursor.get_u16();
        let max_receivable_connect_data = cursor.get_u32();
        let connect_flags_0 = cursor.get_u8();
        let connect_flags_1 = cursor.get_u8();

        // Read connect data string
        let connect_data = if connect_data_length > 0 && cursor.remaining() >= connect_data_length as usize {
            let mut connect_bytes = vec![0u8; connect_data_length as usize];
            cursor.copy_to_slice(&mut connect_bytes);
            String::from_utf8_lossy(&connect_bytes).to_string()
        } else {
            String::new()
        };

        Ok(Self {
            version,
            version_compatible,
            service_options,
            sdu_size,
            tdu_size,
            nt_protocol_characteristics,
            line_turnaround,
            value_of_1,
            connect_data_length,
            connect_data_offset,
            max_receivable_connect_data,
            connect_flags_0,
            connect_flags_1,
            connect_data,
        })
    }

    /// Extract service name from connect data
    pub fn service_name(&self) -> Option<String> {
        // Connect data format: (DESCRIPTION=(ADDRESS=...)...(CONNECT_DATA=(SERVICE_NAME=...)...))
        let data = &self.connect_data;

        // Simple parser for SERVICE_NAME
        if let Some(start) = data.find("SERVICE_NAME=") {
            let start = start + 13; // Length of "SERVICE_NAME="
            if let Some(end) = data[start..].find([')', ' ']) {
                return Some(data[start..start + end].to_string());
            }
        }

        None
    }
}

/// TNS Accept packet data
#[derive(Debug, Clone)]
pub struct TnsAccept {
    /// Protocol version
    pub version: u16,
    /// Service options
    pub service_options: u16,
    /// Session data unit (SDU) size
    pub sdu_size: u16,
    /// Maximum transmission unit (TDU) size
    pub tdu_size: u16,
    /// Value of 1 in hardware
    pub value_of_1: u16,
    /// Data length
    pub data_length: u16,
    /// Data offset
    pub data_offset: u16,
    /// Connect flags 0
    pub connect_flags_0: u8,
    /// Connect flags 1
    pub connect_flags_1: u8,
    /// Accept data
    pub accept_data: Vec<u8>,
}

impl TnsAccept {
    /// Create a new TNS Accept packet
    pub fn new(version: u16, sdu_size: u16, tdu_size: u16) -> Self {
        Self {
            version,
            service_options: 0x0C41, // Default service options
            sdu_size,
            tdu_size,
            value_of_1: 0x0001,
            data_length: 0,
            data_offset: 0,
            connect_flags_0: 0x00,
            connect_flags_1: 0x00,
            accept_data: Vec::new(),
        }
    }

    /// Encode TNS Accept packet to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.put_u16(self.version);
        buf.put_u16(self.service_options);
        buf.put_u16(self.sdu_size);
        buf.put_u16(self.tdu_size);
        buf.put_u16(self.value_of_1);
        buf.put_u16(self.data_length);
        buf.put_u16(self.data_offset);
        buf.put_u8(self.connect_flags_0);
        buf.put_u8(self.connect_flags_1);
        buf.extend_from_slice(&self.accept_data);
        buf.to_vec()
    }
}

/// TNS Refuse packet data
#[derive(Debug, Clone)]
pub struct TnsRefuse {
    /// Reason for refusal
    pub reason: u16,
    /// Data length
    pub data_length: u16,
    /// Refuse data (error message)
    pub refuse_data: String,
}

impl TnsRefuse {
    /// Create a new TNS Refuse packet
    pub fn new(reason: u16, message: String) -> Self {
        Self {
            reason,
            data_length: message.len() as u16,
            refuse_data: message,
        }
    }

    /// Encode TNS Refuse packet to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.put_u16(self.reason);
        buf.put_u16(self.data_length);
        buf.extend_from_slice(self.refuse_data.as_bytes());
        buf.to_vec()
    }
}

/// TNS Data packet
#[derive(Debug, Clone)]
pub struct TnsData {
    /// Data flags
    pub flags: u16,
    /// Payload data
    pub data: Vec<u8>,
}

impl TnsData {
    /// Create a new TNS Data packet
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            flags: 0x0000,
            data,
        }
    }

    /// Parse TNS Data packet
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        if data.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Insufficient data for TNS Data packet",
            ));
        }

        let mut cursor = Cursor::new(data);
        let flags = cursor.get_u16();
        let payload = data.get(2..).unwrap_or(&[]).to_vec();

        Ok(Self {
            flags,
            data: payload,
        })
    }

    /// Encode TNS Data packet to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.put_u16(self.flags);
        buf.extend_from_slice(&self.data);
        buf.to_vec()
    }
}

/// TNS packet (header + payload)
#[derive(Debug, Clone)]
pub struct TnsPacket {
    /// TNS header
    pub header: TnsHeader,
    /// Packet payload
    pub payload: Vec<u8>,
}

impl TnsPacket {
    /// Create a new TNS packet
    pub fn new(packet_type: TnsPacketType, payload: Vec<u8>) -> Self {
        let length = (TnsHeader::SIZE + payload.len()) as u16;
        let header = TnsHeader {
            length,
            checksum: 0,
            packet_type,
            flags: 0,
            header_checksum: 0,
        };

        Self { header, payload }
    }

    /// Parse TNS packet from bytes
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let header = TnsHeader::parse(data)?;

        if data.len() < header.length as usize {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "Incomplete TNS packet: expected {} bytes, got {}",
                    header.length,
                    data.len()
                ),
            ));
        }

        let payload = data.get(TnsHeader::SIZE..header.length as usize)
            .ok_or_else(|| io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "TNS packet payload out of bounds",
            ))?
            .to_vec();

        Ok(Self { header, payload })
    }

    /// Encode TNS packet to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&self.header.encode());
        buf.extend_from_slice(&self.payload);
        buf.to_vec()
    }

    /// Create an Accept packet
    pub fn accept(version: u16, sdu_size: u16, tdu_size: u16) -> Self {
        let accept = TnsAccept::new(version, sdu_size, tdu_size);
        Self::new(TnsPacketType::Accept, accept.encode())
    }

    /// Create a Refuse packet
    pub fn refuse(reason: u16, message: String) -> Self {
        let refuse = TnsRefuse::new(reason, message);
        Self::new(TnsPacketType::Refuse, refuse.encode())
    }

    /// Create a Data packet
    pub fn data(data: Vec<u8>) -> Self {
        let tns_data = TnsData::new(data);
        Self::new(TnsPacketType::Data, tns_data.encode())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_tns_header_parse() {
        let data = vec![
            0x00, 0x20, // length = 32
            0x00, 0x00, // checksum = 0
            0x01,       // packet type = Connect
            0x00,       // flags = 0
            0x00, 0x00, // header checksum = 0
        ];

        let header = TnsHeader::parse(&data).unwrap();
        assert_eq!(header.length, 32);
        assert_eq!(header.packet_type, TnsPacketType::Connect);
    }

    #[test]
    fn test_tns_packet_encode_decode() {
        let payload = vec![1, 2, 3, 4];
        let packet = TnsPacket::new(TnsPacketType::Data, payload.clone());

        let encoded = packet.encode();
        let decoded = TnsPacket::parse(&encoded).unwrap();

        assert_eq!(decoded.header.packet_type, TnsPacketType::Data);
        assert_eq!(decoded.payload, payload);
    }
}
