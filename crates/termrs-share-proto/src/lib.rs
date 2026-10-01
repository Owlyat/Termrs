//! Wire protocol for termrs terminal sharing.
//!
//! One iroh bi-directional QUIC stream carries length-prefixed frames. Each
//! frame is a 4-byte big-endian body length (the tag byte plus its payload),
//! followed by a 1-byte tag and the payload. Data frames (`Snapshot`, `Output`,
//! `Input`) carry raw terminal bytes; the handshake and resize frames use a
//! fixed little layout.
//!
//! This crate is dependency-free so both the native host and the browser
//! (wasm) client use exactly the same encoding.

use std::fmt;

/// ALPN identifying the termrs share protocol on an iroh connection.
pub const ALPN: &[u8] = b"termrs/share/1";

/// Protocol version carried in the handshake.
pub const PROTOCOL_VERSION: u8 = 1;

/// Bytes of the length prefix preceding every frame body.
pub const LEN_PREFIX: usize = 4;

/// Largest accepted frame body (tag + payload). Guards against a hostile peer
/// asking for an unbounded allocation.
pub const MAX_BODY: usize = 16 * 1024 * 1024;

/// Frame tags.
pub mod tag {
    pub const HELLO: u8 = 1;
    pub const SNAPSHOT: u8 = 2;
    pub const OUTPUT: u8 = 3;
    pub const INPUT: u8 = 4;
    pub const RESIZE: u8 = 5;
    pub const ERROR: u8 = 6;
}

/// Whether a client may drive the terminal or only watch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The first client to claim this may type; sent by the viewer.
    Control,
    /// Watch only: input frames are ignored by the host.
    ReadOnly,
}

impl Mode {
    pub fn as_u8(self) -> u8 {
        match self {
            Mode::Control => 0,
            Mode::ReadOnly => 1,
        }
    }

    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            0 => Ok(Mode::Control),
            1 => Ok(Mode::ReadOnly),
            other => Err(ProtoError::BadMode(other)),
        }
    }
}

/// The first frame a client sends after connecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub mode: Mode,
    /// Client's terminal size, so the host can send a matching snapshot.
    pub cols: u16,
    pub rows: u16,
    /// Short session code shown by the host; empty when not required.
    pub code: String,
}

impl Hello {
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(PROTOCOL_VERSION);
        out.push(self.mode.as_u8());
        out.extend_from_slice(&self.cols.to_be_bytes());
        out.extend_from_slice(&self.rows.to_be_bytes());
        let code = self.code.as_bytes();
        let len = code.len().min(u16::MAX as usize) as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&code[..len as usize]);
    }

    fn decode(payload: &[u8]) -> Result<Self, ProtoError> {
        if payload.len() < 7 {
            return Err(ProtoError::Short);
        }
        let version = payload[0];
        if version != PROTOCOL_VERSION {
            return Err(ProtoError::BadVersion(version));
        }
        let mode = Mode::from_u8(payload[1])?;
        let cols = u16::from_be_bytes([payload[2], payload[3]]);
        let rows = u16::from_be_bytes([payload[4], payload[5]]);
        let code_len = u16::from_be_bytes([payload[6], payload[7]]) as usize;
        let rest = &payload[8..];
        if rest.len() < code_len {
            return Err(ProtoError::Short);
        }
        let code = std::str::from_utf8(&rest[..code_len])
            .map_err(|_| ProtoError::Utf8)?
            .to_string();
        Ok(Self {
            mode,
            cols,
            rows,
            code,
        })
    }
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Hello(Hello),
    /// Full-screen redraw to (re)initialize a viewer.
    Snapshot(Vec<u8>),
    /// Incremental raw PTY output.
    Output(Vec<u8>),
    /// Keystrokes / paste from a controlling viewer.
    Input(Vec<u8>),
    /// Viewer's terminal size changed.
    Resize { cols: u16, rows: u16 },
    /// Human-readable error; the connection is then closed.
    Error(String),
}

impl Frame {
    /// Frame tag byte.
    pub fn tag(&self) -> u8 {
        match self {
            Frame::Hello(_) => tag::HELLO,
            Frame::Snapshot(_) => tag::SNAPSHOT,
            Frame::Output(_) => tag::OUTPUT,
            Frame::Input(_) => tag::INPUT,
            Frame::Resize { .. } => tag::RESIZE,
            Frame::Error(_) => tag::ERROR,
        }
    }

    /// Payload bytes (everything after the tag).
    pub fn payload(&self) -> Vec<u8> {
        match self {
            Frame::Hello(h) => {
                let mut out = Vec::with_capacity(9 + h.code.len());
                h.encode_into(&mut out);
                out
            }
            Frame::Snapshot(b) | Frame::Output(b) | Frame::Input(b) => b.clone(),
            Frame::Resize { cols, rows } => {
                let mut out = Vec::with_capacity(4);
                out.extend_from_slice(&cols.to_be_bytes());
                out.extend_from_slice(&rows.to_be_bytes());
                out
            }
            Frame::Error(msg) => msg.as_bytes().to_vec(),
        }
    }

    /// Encode to `len_prefix || tag || payload`.
    pub fn encode(&self) -> Vec<u8> {
        encode(self.tag(), &self.payload())
    }

    /// Decode a tag + payload into a frame.
    pub fn decode(tag: u8, payload: &[u8]) -> Result<Self, ProtoError> {
        match tag {
            tag::HELLO => Ok(Frame::Hello(Hello::decode(payload)?)),
            tag::SNAPSHOT => Ok(Frame::Snapshot(payload.to_vec())),
            tag::OUTPUT => Ok(Frame::Output(payload.to_vec())),
            tag::INPUT => Ok(Frame::Input(payload.to_vec())),
            tag::RESIZE => {
                if payload.len() < 4 {
                    return Err(ProtoError::Short);
                }
                Ok(Frame::Resize {
                    cols: u16::from_be_bytes([payload[0], payload[1]]),
                    rows: u16::from_be_bytes([payload[2], payload[3]]),
                })
            }
            tag::ERROR => Ok(Frame::Error(
                String::from_utf8_lossy(payload).into_owned(),
            )),
            other => Err(ProtoError::UnknownTag(other)),
        }
    }
}

/// Encode a raw tag + payload with the length prefix.
pub fn encode(tag: u8, payload: &[u8]) -> Vec<u8> {
    let body_len = 1 + payload.len();
    let mut out = Vec::with_capacity(LEN_PREFIX + body_len);
    out.extend_from_slice(&(body_len as u32).to_be_bytes());
    out.push(tag);
    out.extend_from_slice(payload);
    out
}

/// Build a [`Frame::Snapshot`] payload: `cols u16 BE | rows u16 BE | ansi`.
///
/// The size lets a viewer resize its terminal grid to match the host pane
/// before the redraw is applied.
pub fn encode_snapshot(cols: u16, rows: u16, ansi: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + ansi.len());
    out.extend_from_slice(&cols.to_be_bytes());
    out.extend_from_slice(&rows.to_be_bytes());
    out.extend_from_slice(ansi);
    out
}

/// Split a [`Frame::Snapshot`] payload into `(cols, rows, ansi)`.
pub fn decode_snapshot(payload: &[u8]) -> Result<(u16, u16, &[u8]), ProtoError> {
    if payload.len() < 4 {
        return Err(ProtoError::Short);
    }
    Ok((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
        &payload[4..],
    ))
}

/// Parse a 4-byte length prefix. Errors when the body is over [`MAX_BODY`].
pub fn body_len(prefix: [u8; LEN_PREFIX]) -> Result<usize, ProtoError> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len < 1 {
        return Err(ProtoError::Short);
    }
    if len > MAX_BODY {
        return Err(ProtoError::TooLarge(len));
    }
    Ok(len)
}

/// Protocol errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    Short,
    TooLarge(usize),
    UnknownTag(u8),
    BadVersion(u8),
    BadMode(u8),
    Utf8,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Short => write!(f, "frame payload too short"),
            ProtoError::TooLarge(n) => write!(f, "frame body {n} bytes exceeds limit"),
            ProtoError::UnknownTag(t) => write!(f, "unknown frame tag {t}"),
            ProtoError::BadVersion(v) => write!(f, "unsupported protocol version {v}"),
            ProtoError::BadMode(m) => write!(f, "unknown client mode {m}"),
            ProtoError::Utf8 => write!(f, "invalid UTF-8 in frame"),
        }
    }
}

impl std::error::Error for ProtoError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: Frame) {
        let bytes = frame.encode();
        let body = body_len(bytes[..LEN_PREFIX].try_into().unwrap()).unwrap();
        assert_eq!(body, bytes.len() - LEN_PREFIX);
        let tag = bytes[LEN_PREFIX];
        let payload = &bytes[LEN_PREFIX + 1..];
        assert_eq!(Frame::decode(tag, payload).unwrap(), frame);
    }

    #[test]
    fn all_frames_roundtrip() {
        roundtrip(Frame::Hello(Hello {
            mode: Mode::Control,
            cols: 120,
            rows: 40,
            code: "AB12CD".into(),
        }));
        roundtrip(Frame::Hello(Hello {
            mode: Mode::ReadOnly,
            cols: 80,
            rows: 24,
            code: String::new(),
        }));
        roundtrip(Frame::Snapshot(b"\x1b[2Jhello".to_vec()));
        roundtrip(Frame::Output(b"world\r\n".to_vec()));
        roundtrip(Frame::Input(vec![0x03]));
        roundtrip(Frame::Input(Vec::new()));
        roundtrip(Frame::Resize {
            cols: 100,
            rows: 30,
        });
        roundtrip(Frame::Error("nope".into()));
    }

    #[test]
    fn length_prefix_guards() {
        assert_eq!(body_len([0, 0, 0, 0]), Err(ProtoError::Short));
        assert_eq!(
            body_len(((MAX_BODY + 1) as u32).to_be_bytes()),
            Err(ProtoError::TooLarge(MAX_BODY + 1))
        );
        assert_eq!(
            body_len(5u32.to_be_bytes()).unwrap(),
            5
        );
    }

    #[test]
    fn snapshot_size_roundtrips() {
        let payload = encode_snapshot(120, 40, b"\x1b[2Jhi");
        assert_eq!(
            decode_snapshot(&payload).unwrap(),
            (120, 40, &b"\x1b[2Jhi"[..])
        );
        assert_eq!(decode_snapshot(b"\x00\x01"), Err(ProtoError::Short));
        assert_eq!(decode_snapshot(b""), Err(ProtoError::Short));
    }

    #[test]
    fn bad_frames_rejected() {
        assert_eq!(Frame::decode(99, b"x"), Err(ProtoError::UnknownTag(99)));
        assert_eq!(Frame::decode(tag::RESIZE, b"\x00"), Err(ProtoError::Short));
        let mut hello = Vec::new();
        Hello {
            mode: Mode::Control,
            cols: 1,
            rows: 1,
            code: "x".into(),
        }
        .encode_into(&mut hello);
        hello[0] = 9; // bad version
        assert_eq!(
            Frame::decode(tag::HELLO, &hello),
            Err(ProtoError::BadVersion(9))
        );
        hello[0] = PROTOCOL_VERSION;
        hello[1] = 7; // bad mode
        assert_eq!(Frame::decode(tag::HELLO, &hello), Err(ProtoError::BadMode(7)));
    }
}
