//! Wire constants and framing for the Viper488 WinUSB protocol, version 2.
//!
//! Mirrors `transport/winusb/winusb_proto.h` in the viper-488 firmware repository;
//! `docs/winusb_protocol.md` there is the specification and wins if they disagree.

pub const PROTO_VERSION: u16 = 2;
pub const HDR_LEN: usize = 12;
pub const ADDR_NONE: u8 = 0xFF;

pub const OP_NOP: u8 = 0x00;
pub const OP_WRITE: u8 = 0x01;
pub const OP_READ: u8 = 0x02;
pub const OP_SPOLL: u8 = 0x03;
pub const OP_CLEAR: u8 = 0x04;
pub const OP_TRIGGER: u8 = 0x05;
pub const OP_REN: u8 = 0x06;
pub const OP_LOCAL: u8 = 0x07;
pub const OP_LLO: u8 = 0x08;
pub const OP_IFC: u8 = 0x09;
pub const OP_LINES: u8 = 0x0B;
pub const OP_PROBE: u8 = 0x0C;
/// The interface level (a VISA GPIB INTFC session; firmware with CAP_RAW): interface messages
/// with ATN, then data moved addressing nobody. Nothing sends UNL/UNT afterwards.
pub const OP_CMD: u8 = 0x0D;
pub const OP_RAW_WRITE: u8 = 0x0E;
pub const OP_RAW_READ: u8 = 0x0F;
pub const OP_IDENTIFY: u8 = 0x10;
pub const OP_SPEED: u8 = 0x1A;

pub const ST_OK: u8 = 0x00;
pub const ST_TIMEOUT: u8 = 0x01;
pub const ST_BUS: u8 = 0x02;
pub const ST_NO_DEVICE: u8 = 0x03;
pub const ST_BAD_REQUEST: u8 = 0x04;
pub const ST_UNSUPPORTED: u8 = 0x05;
pub const ST_STORAGE: u8 = 0x07;

pub const W_EOI_LAST: u8 = 0x01;
pub const W_MORE: u8 = 0x02;

pub const R_END_ON_CHAR: u8 = 0x01;
pub const REASON_REQCNT: u8 = 0x01;
pub const REASON_CHR: u8 = 0x02;
pub const REASON_END: u8 = 0x04;

pub const REN_ENABLE: u8 = 0x01;
pub const PROBE_RESTORE_LOCAL: u8 = 0x01;

/// SPEED levels (the T1 settling time of the write direction; docs/winusb_protocol.md 4.14).
pub const SPEED_QUERY: u8 = 0;
pub const SPEED_LOW: u8 = 1;
pub const SPEED_NORMAL: u8 = 2;
pub const SPEED_HIGH: u8 = 3;

pub fn speed_name(l: u8) -> &'static str {
    match l {
        SPEED_LOW => "low",
        SPEED_NORMAL => "normal",
        SPEED_HIGH => "high",
        _ => "?",
    }
}

pub fn speed_from_name(n: &str) -> Option<u8> {
    [SPEED_LOW, SPEED_NORMAL, SPEED_HIGH].into_iter().find(|&l| speed_name(l) == n)
}

/// Response `bus` bit: SRQ asserted.
pub const BUS_SRQ: u16 = 0x0001;
pub const BUS_ATN: u16 = 0x0002;
pub const BUS_REN: u16 = 0x0004;
pub const BUS_EOI: u16 = 0x0008;
pub const BUS_NRFD: u16 = 0x0010;
pub const BUS_NDAC: u16 = 0x0020;
pub const BUS_DAV: u16 = 0x0040;
pub const BUS_IFC: u16 = 0x0080;

pub const CAP_SERVICE: u32 = 0x1;
pub const CAP_SPEED: u32 = 0x2;
pub const CAP_RAW: u32 = 0x4;

pub fn status_name(s: u8) -> String {
    match s {
        ST_OK => "OK".into(),
        ST_TIMEOUT => "TIMEOUT".into(),
        ST_BUS => "BUS".into(),
        ST_NO_DEVICE => "NO_DEVICE".into(),
        ST_BAD_REQUEST => "BAD_REQUEST".into(),
        ST_UNSUPPORTED => "UNSUPPORTED".into(),
        ST_STORAGE => "STORAGE".into(),
        o => format!("0x{o:02X}"),
    }
}

/// Request: op flags pad sad tag:u16 timeout_ms:u16 len:u32 (little endian).
pub fn request_header(op: u8, flags: u8, pad: u8, tag: u16, timeout_ms: u16, len: u32) -> [u8; HDR_LEN] {
    let mut h = [0u8; HDR_LEN];
    h[0] = op;
    h[1] = flags;
    h[2] = pad;
    h[3] = ADDR_NONE; // sad: there is no secondary addressing
    h[4..6].copy_from_slice(&tag.to_le_bytes());
    h[6..8].copy_from_slice(&timeout_ms.to_le_bytes());
    h[8..12].copy_from_slice(&len.to_le_bytes());
    h
}

#[derive(Debug, Clone, Default)]
pub struct Response {
    pub op: u8,
    pub status: u8,
    pub reason: u8,
    pub flags: u8,
    pub tag: u16,
    /// Bus lines, set = asserted (WINUSB_BUS_*).
    pub bus: u16,
    pub payload: Vec<u8>,
}

impl Response {
    /// Parse the 12-byte header; returns the response (empty payload) and the payload length.
    pub fn parse_header(b: &[u8]) -> Option<(Response, usize)> {
        if b.len() < HDR_LEN {
            return None;
        }
        let len = u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize;
        Some((
            Response {
                op: b[0],
                status: b[1],
                reason: b[2],
                flags: b[3],
                tag: u16::from_le_bytes([b[4], b[5]]),
                bus: u16::from_le_bytes([b[6], b[7]]),
                payload: Vec::new(),
            },
            len,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_header_layout() {
        let h = request_header(OP_READ, 0, 20, 0x0102, 0x0304, 8);
        assert_eq!(h, [0x02, 0, 20, 0xFF, 0x02, 0x01, 0x04, 0x03, 8, 0, 0, 0]);
    }

    #[test]
    fn response_header_parse() {
        let raw = [0x1A, 0, 0, 0, 0x05, 0x00, 0x30, 0x00, 1, 0, 0, 0];
        let (r, len) = Response::parse_header(&raw).unwrap();
        assert_eq!((r.op, r.status, r.tag, r.bus, len), (0x1A, 0, 5, 0x30, 1));
        assert!(Response::parse_header(&raw[..11]).is_none());
    }
}
