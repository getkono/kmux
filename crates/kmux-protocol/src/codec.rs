use thiserror::Error;

use crate::messages::{ClientMessage, ServerMessage};

/// Errors that can occur during message encoding or decoding.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("encode/decode error: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large: {size} bytes (max {max})")]
    FrameTooLarge { size: u32, max: u32 },
    /// A frame's length prefix was too small to even hold the codec tag byte.
    #[error("malformed frame: length prefix too small")]
    MalformedFrame,
    /// The frame's codec tag did not correspond to any known codec. Most likely
    /// a version skew or a corrupted stream.
    #[error("unknown frame codec tag: {0}")]
    UnknownCodec(u8),
    #[error("compression error: {0}")]
    Compress(String),
    #[error("decompression error: {0}")]
    Decompress(String),
    /// A compressed frame inflated past [`MAX_DECOMPRESSED_SIZE`] — refused to
    /// guard against decompression bombs.
    #[error("decompressed frame too large: {size} bytes (max {max})")]
    DecompressedTooLarge { size: usize, max: usize },
}

/// Encode a `ClientMessage` into a postcard byte vector.
pub fn encode_client(msg: &ClientMessage) -> Result<Vec<u8>, ProtocolError> {
    postcard::to_allocvec(msg).map_err(ProtocolError::Postcard)
}

/// Decode a `ClientMessage` from a postcard byte slice.
pub fn decode_client(data: &[u8]) -> Result<ClientMessage, ProtocolError> {
    postcard::from_bytes(data).map_err(ProtocolError::Postcard)
}

/// Encode a `ServerMessage` into a postcard byte vector.
pub fn encode_server(msg: &ServerMessage) -> Result<Vec<u8>, ProtocolError> {
    postcard::to_allocvec(msg).map_err(ProtocolError::Postcard)
}

/// Decode a `ServerMessage` from a postcard byte slice.
pub fn decode_server(data: &[u8]) -> Result<ServerMessage, ProtocolError> {
    postcard::from_bytes(data).map_err(ProtocolError::Postcard)
}

//  Length-prefixed framing for async byte streams
//
//  Wire frame: `[u32 big-endian length][u8 codec tag][payload…]`. The length
//  prefix counts the 1-byte codec tag plus the (possibly compressed) payload.
//  The tag makes each frame self-describing, so a reader decompresses purely
//  from the tag with no per-connection state, and the compression switch-over
//  after the handshake needs no coordination between the read and write tasks.
//
//  See `docs/compression.md` for the negotiation model.

/// Maximum on-wire frame size (16 MiB) -- prevents unbounded allocations from
/// malformed data. Bounds the length prefix (tag byte + payload).
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Maximum size a single frame may occupy *after* decompression (64 MiB).
/// Guards against decompression bombs: a small compressed frame must never be
/// allowed to inflate without bound.
pub const MAX_DECOMPRESSED_SIZE: usize = 64 * 1024 * 1024;

/// Per-frame codec tag: the payload is stored verbatim (no compression).
const FRAME_RAW: u8 = 0;
/// Per-frame codec tag: the payload is zstd-compressed.
const FRAME_ZSTD: u8 = 1;

/// Outbound per-connection compression policy applied by [`write_frame_compressed`].
///
/// The negotiated algorithm and level are connection constants chosen by the
/// daemon at auth time; per-frame the writer still only compresses payloads at
/// or above `min_size` and only keeps the result when it actually shrinks.
#[derive(Debug, Clone, Copy)]
pub enum Compressor {
    /// Never compress; every frame is written with the identity tag.
    Off,
    /// Compress payloads `>= min_size` with zstd at `level`.
    Zstd { level: i32, min_size: usize },
}

/// Write a length-prefixed identity (uncompressed) frame. Used for the
/// handshake, the `ls` front door, and any path that does not negotiate
/// compression.
#[cfg(feature = "framing")]
pub async fn write_frame<W: tokio::io::AsyncWriteExt + Unpin>(
    w: &mut W,
    data: &[u8],
) -> Result<(), ProtocolError> {
    write_tagged(w, FRAME_RAW, data).await.map(|_| ())
}

/// Write a length-prefixed frame, compressing the payload when `comp` allows it
/// and the result is actually smaller. Returns the number of bytes written to
/// the wire (length prefix + tag + payload) so callers can account real
/// bandwidth.
#[cfg(feature = "framing")]
pub async fn write_frame_compressed<W: tokio::io::AsyncWriteExt + Unpin>(
    w: &mut W,
    data: &[u8],
    comp: Compressor,
) -> Result<usize, ProtocolError> {
    if let Compressor::Zstd { level, min_size } = comp
        && data.len() >= min_size
    {
        let compressed = zstd::bulk::compress(data, level)
            .map_err(|e| ProtocolError::Compress(e.to_string()))?;
        // Only keep the compressed form when it genuinely shrinks the frame;
        // otherwise the identity tag avoids ever growing past `data.len() + 1`.
        if compressed.len() < data.len() {
            return write_tagged(w, FRAME_ZSTD, &compressed).await;
        }
    }
    write_tagged(w, FRAME_RAW, data).await
}

/// Write `[u32 len][u8 tag][payload]` where `len = payload.len() + 1`.
/// Returns the total bytes written to the wire.
#[cfg(feature = "framing")]
async fn write_tagged<W: tokio::io::AsyncWriteExt + Unpin>(
    w: &mut W,
    tag: u8,
    payload: &[u8],
) -> Result<usize, ProtocolError> {
    let len = payload.len() as u64 + 1; // + 1 for the codec tag byte
    if len > MAX_FRAME_SIZE as u64 {
        return Err(ProtocolError::FrameTooLarge {
            size: len.min(u32::MAX as u64) as u32,
            max: MAX_FRAME_SIZE,
        });
    }
    // One write for the 5-byte header (length prefix + tag) keeps the syscall
    // count identical to the old `[len][payload]` format.
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&(len as u32).to_be_bytes());
    header[4] = tag;
    w.write_all(&header).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(4 + len as usize)
}

/// Read a length-prefixed frame, transparently decompressing it. Returns `None`
/// on clean stream close (EOF on the header). The returned bytes are always the
/// decoded (decompressed) payload, so all callers are codec-agnostic.
#[cfg(feature = "framing")]
pub async fn read_frame<R: tokio::io::AsyncReadExt + Unpin>(
    r: &mut R,
) -> Result<Option<Vec<u8>>, ProtocolError> {
    // 5-byte header: 4-byte big-endian length prefix + 1-byte codec tag.
    let mut header = [0u8; 5];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ProtocolError::Io(e)),
    }
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            size: len,
            max: MAX_FRAME_SIZE,
        });
    }
    if len == 0 {
        // The length must account for at least the codec tag byte.
        return Err(ProtocolError::MalformedFrame);
    }
    let tag = header[4];
    let mut payload = vec![0u8; (len - 1) as usize];
    r.read_exact(&mut payload).await?;
    match tag {
        FRAME_RAW => Ok(Some(payload)),
        FRAME_ZSTD => Ok(Some(decompress_frame(&payload)?)),
        other => Err(ProtocolError::UnknownCodec(other)),
    }
}

/// Decompress a zstd frame, bounding the output at [`MAX_DECOMPRESSED_SIZE`].
///
/// Decoding streams into a growing buffer (rather than pre-allocating the cap)
/// so a small frame stays cheap, while the `take` limit keeps a malicious frame
/// from inflating without bound.
#[cfg(feature = "framing")]
fn decompress_frame(payload: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    use std::io::Read;
    let decoder = zstd::stream::read::Decoder::new(payload)
        .map_err(|e| ProtocolError::Decompress(e.to_string()))?;
    let mut out = Vec::new();
    decoder
        .take(MAX_DECOMPRESSED_SIZE as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| ProtocolError::Decompress(e.to_string()))?;
    if out.len() > MAX_DECOMPRESSED_SIZE {
        return Err(ProtocolError::DecompressedTooLarge {
            size: out.len(),
            max: MAX_DECOMPRESSED_SIZE,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::*;

    #[test]
    fn roundtrip_client_auth() {
        let msg = ClientMessage::Auth {
            token: "secret".to_string(),
            protocol_version: PROTOCOL_VERSION,
            capabilities: ClientCapabilities::default(),
            connection_id: None,
        };
        let bytes = encode_client(&msg).expect("encode");
        let decoded = decode_client(&bytes).expect("decode");
        assert!(matches!(decoded, ClientMessage::Auth { token, .. } if token == "secret"));
    }

    #[test]
    fn roundtrip_client_session_create() {
        let msg = ClientMessage::SessionCreate {
            request_id: 42,
            name: Some("my-session".to_string()),
            cwd: None,
            program: Some("/bin/bash".to_string()),
            args: vec![],
            size: TermSize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            },
            peer: None,
        };
        let bytes = encode_client(&msg).expect("encode");
        let decoded = decode_client(&bytes).expect("decode");
        assert!(
            matches!(&decoded, ClientMessage::SessionCreate { request_id: 42, name, .. }
                if name.as_deref() == Some("my-session"))
        );
    }

    #[test]
    fn roundtrip_server_error() {
        let msg = ServerMessage::Error {
            request_id: Some(1),
            code: ErrorCode::SessionNotFound,
            message: "no such session".to_string(),
        };
        let bytes = encode_server(&msg).expect("encode");
        let decoded = decode_server(&bytes).expect("decode");
        assert!(matches!(
            &decoded,
            ServerMessage::Error {
                code: ErrorCode::SessionNotFound,
                ..
            }
        ));
    }

    #[test]
    fn roundtrip_client_attach_with_seqno() {
        let msg = ClientMessage::Attach {
            pane_id: "eagle/0".to_string(),
            last_seqno: Some(SequenceNo(100)),
            size: TermSize::default(),
        };
        let bytes = encode_client(&msg).expect("encode");
        let decoded = decode_client(&bytes).expect("decode");
        assert!(
            matches!(&decoded, ClientMessage::Attach { pane_id, last_seqno: Some(SequenceNo(100)), .. }
                if pane_id == "eagle/0")
        );
    }

    #[test]
    fn attach_roundtrip_with_size() {
        let size = TermSize {
            rows: 40,
            cols: 132,
            pixel_width: 1056,
            pixel_height: 640,
        };
        let msg = ClientMessage::Attach {
            pane_id: "eagle/0".to_string(),
            last_seqno: None,
            size,
        };
        let bytes = encode_client(&msg).expect("encode");
        let decoded = decode_client(&bytes).expect("decode");
        match decoded {
            ClientMessage::Attach {
                pane_id,
                last_seqno,
                size: decoded_size,
            } => {
                assert_eq!(pane_id, "eagle/0");
                assert!(last_seqno.is_none());
                assert_eq!(decoded_size.rows, 40);
                assert_eq!(decoded_size.cols, 132);
                assert_eq!(decoded_size.pixel_width, 1056);
                assert_eq!(decoded_size.pixel_height, 640);
            }
            _ => panic!("expected Attach"),
        }
    }

    #[test]
    fn roundtrip_auth_result_with_client_id() {
        let msg = ServerMessage::AuthResult {
            success: true,
            reason: None,
            client_id: Some(ClientId(7)),
            server_version: Some("0.1.0".to_string()),
            connection_id: None,
            compression: None,
        };
        let bytes = encode_server(&msg).expect("encode");
        let decoded = decode_server(&bytes).expect("decode");
        assert!(matches!(
            &decoded,
            ServerMessage::AuthResult {
                success: true,
                client_id: Some(ClientId(7)),
                ..
            }
        ));
    }

    #[test]
    fn roundtrip_terminal_diff() {
        let diff = TerminalDiff {
            ops: vec![
                DiffOp::Cell {
                    row: 0,
                    col: 5,
                    cell: CellState::default(),
                },
                DiffOp::Row {
                    row: 1,
                    start_col: 0,
                    cells: vec![CellState::default(); 3],
                },
                DiffOp::Clear,
            ],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_reset: None,
        };
        let msg = ServerMessage::TerminalUpdate {
            pane_id: "eagle/0".to_string(),
            diff: std::sync::Arc::new(diff),
            seqno: SequenceNo(1),
            sent_at_ms: 0,
        };
        let bytes = encode_server(&msg).expect("encode");
        let decoded = decode_server(&bytes).expect("decode");
        assert!(
            matches!(&decoded, ServerMessage::TerminalUpdate { pane_id, .. } if pane_id == "eagle/0")
        );
    }

    #[test]
    fn roundtrip_grid_snapshot() {
        let snapshot = GridSnapshot {
            rows: 2,
            cols: 3,
            cells: vec![CellState::default(); 6],
            cursor: CursorState {
                row: 1,
                col: 2,
                shape: CursorShape::Bar,
                visible: true,
                blink: true,
            },
            modes: TermModes(TermModes::APP_CURSOR),
            history_total: 0,
            scrollback_base: 0,
            scrollback_tail: Vec::new(),
        };
        let msg = ServerMessage::TerminalSnapshot {
            pane_id: "eagle/0".to_string(),
            snapshot,
            seqno: SequenceNo(99),
            sent_at_ms: 0,
        };
        let bytes = encode_server(&msg).expect("encode");
        let decoded = decode_server(&bytes).expect("decode");
        match &decoded {
            ServerMessage::TerminalSnapshot {
                pane_id, snapshot, ..
            } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(snapshot.rows, 2);
                assert_eq!(snapshot.cols, 3);
                assert_eq!(snapshot.cells.len(), 6);
                assert!(snapshot.modes.app_cursor());
                // Cursor shape, position, and blink survive the wire round-trip.
                assert_eq!(snapshot.cursor.shape, CursorShape::Bar);
                assert_eq!((snapshot.cursor.row, snapshot.cursor.col), (1, 2));
                assert!(snapshot.cursor.blink);
            }
            _ => panic!("expected TerminalSnapshot"),
        }
    }

    #[test]
    fn roundtrip_cell_state_with_attrs() {
        let cell = CellState {
            c: 'X',
            fg: CellColor::new(255, 0, 0),
            bg: CellColor::new(0, 0, 0),
            attrs: CellAttrs(CellAttrs::BOLD | CellAttrs::UNDERLINE),
        };
        let diff = TerminalDiff {
            ops: vec![DiffOp::Cell {
                row: 0,
                col: 0,
                cell,
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_reset: None,
        };
        let msg = ServerMessage::TerminalUpdate {
            pane_id: "eagle/0".to_string(),
            diff: std::sync::Arc::new(diff),
            seqno: SequenceNo(1),
            sent_at_ms: 0,
        };
        let bytes = encode_server(&msg).expect("encode");
        let decoded = decode_server(&bytes).expect("decode");
        match decoded {
            ServerMessage::TerminalUpdate { diff, .. } => {
                if let DiffOp::Cell { cell, .. } = &diff.ops[0] {
                    assert_eq!(cell.c, 'X');
                    assert!(cell.attrs.contains(CellAttrs::BOLD));
                    assert!(cell.attrs.contains(CellAttrs::UNDERLINE));
                    assert!(!cell.attrs.contains(CellAttrs::ITALIC));
                } else {
                    panic!("expected Cell op");
                }
            }
            _ => panic!("expected TerminalUpdate"),
        }
    }
}

#[cfg(all(test, feature = "framing"))]
mod framing_tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let data = b"hello, framing!";
        let mut buf = Vec::new();
        write_frame(&mut buf, data).await.expect("write");
        // 4-byte length prefix + 1-byte codec tag + payload.
        assert_eq!(buf.len(), 5 + data.len());
        assert_eq!(buf[4], FRAME_RAW);

        let mut cursor = std::io::Cursor::new(buf);
        let result = read_frame(&mut cursor).await.expect("read");
        assert_eq!(result.unwrap(), data);
    }

    #[tokio::test]
    async fn frame_eof_returns_none() {
        let buf: Vec<u8> = vec![];
        let mut cursor = std::io::Cursor::new(buf);
        let result = read_frame(&mut cursor).await.expect("read");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn frame_too_large_rejected() {
        // 5-byte header: oversized length prefix + a codec tag byte.
        let mut bytes = (MAX_FRAME_SIZE + 1).to_be_bytes().to_vec();
        bytes.push(FRAME_RAW);
        let mut cursor = std::io::Cursor::new(bytes);
        let result = read_frame(&mut cursor).await;
        assert!(matches!(result, Err(ProtocolError::FrameTooLarge { .. })));
    }

    #[tokio::test]
    async fn frame_server_message_roundtrip() {
        let msg = ServerMessage::Pong { seq: 42 };
        let payload = encode_server(&msg).expect("encode");

        let mut buf = Vec::new();
        write_frame(&mut buf, &payload).await.expect("write");

        let mut cursor = std::io::Cursor::new(buf);
        let frame = read_frame(&mut cursor).await.expect("read").unwrap();
        let decoded = decode_server(&frame).expect("decode");
        assert!(matches!(decoded, ServerMessage::Pong { seq: 42 }));
    }

    /// A compressible payload above the threshold is stored zstd-tagged and the
    /// wire frame is smaller than the raw payload, yet round-trips exactly.
    #[tokio::test]
    async fn compressed_frame_shrinks_and_roundtrips() {
        let data = vec![b'a'; 4096]; // trivially compressible
        let comp = Compressor::Zstd {
            level: 3,
            min_size: 256,
        };
        let mut buf = Vec::new();
        let wire = write_frame_compressed(&mut buf, &data, comp)
            .await
            .expect("write");
        assert_eq!(wire, buf.len());
        assert_eq!(buf[4], FRAME_ZSTD, "should have used the zstd codec");
        assert!(buf.len() < data.len(), "compressed frame must be smaller");

        let mut cursor = std::io::Cursor::new(buf);
        let out = read_frame(&mut cursor).await.expect("read").unwrap();
        assert_eq!(out, data);
    }

    /// Payloads below `min_size` are never compressed (the zstd overhead would
    /// dominate), so they stay identity-tagged.
    #[tokio::test]
    async fn small_payload_stays_raw() {
        let data = b"tiny";
        let comp = Compressor::Zstd {
            level: 3,
            min_size: 256,
        };
        let mut buf = Vec::new();
        write_frame_compressed(&mut buf, data, comp)
            .await
            .expect("write");
        assert_eq!(buf[4], FRAME_RAW);

        let mut cursor = std::io::Cursor::new(buf);
        let out = read_frame(&mut cursor).await.expect("read").unwrap();
        assert_eq!(out, data);
    }

    /// Incompressible data above the threshold falls back to identity rather
    /// than growing the frame.
    #[tokio::test]
    async fn incompressible_payload_falls_back_to_raw() {
        // High-entropy xorshift64 bytes that zstd cannot shrink.
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let data: Vec<u8> = (0..2048)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s & 0xff) as u8
            })
            .collect();
        let comp = Compressor::Zstd {
            level: 3,
            min_size: 256,
        };
        let mut buf = Vec::new();
        write_frame_compressed(&mut buf, &data, comp)
            .await
            .expect("write");
        assert_eq!(buf[4], FRAME_RAW, "non-shrinking data must stay raw");

        let mut cursor = std::io::Cursor::new(buf);
        let out = read_frame(&mut cursor).await.expect("read").unwrap();
        assert_eq!(out, data);
    }

    /// `Compressor::Off` writes identity frames even for large compressible data.
    #[tokio::test]
    async fn compressor_off_never_compresses() {
        let data = vec![b'z'; 4096];
        let mut buf = Vec::new();
        write_frame_compressed(&mut buf, &data, Compressor::Off)
            .await
            .expect("write");
        assert_eq!(buf[4], FRAME_RAW);
        assert_eq!(buf.len(), 5 + data.len());
    }

    /// An unknown codec tag is rejected rather than silently mis-decoded.
    #[tokio::test]
    async fn unknown_codec_tag_rejected() {
        // [len = 2][tag = 0xFF][one payload byte]
        let bytes = vec![0x00, 0x00, 0x00, 0x02, 0xFF, 0x41];
        let mut cursor = std::io::Cursor::new(bytes);
        let result = read_frame(&mut cursor).await;
        assert!(matches!(result, Err(ProtocolError::UnknownCodec(0xFF))));
    }

    /// End-to-end over a real duplex pipe: a writer task compresses a realistic
    /// mix of large (compressible) and tiny `ServerMessage`s exactly as the
    /// daemon's writer does, and a concurrent reader decodes them transparently.
    /// Mirrors the server-writer → client-reader wire contract.
    #[tokio::test]
    async fn compressed_duplex_roundtrip() {
        use crate::messages::ErrorCode;

        let (mut server_side, mut client_side) = tokio::io::duplex(64 * 1024);
        let comp = Compressor::Zstd {
            level: 3,
            min_size: 256,
        };

        let msgs: Vec<ServerMessage> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    ServerMessage::Error {
                        request_id: Some(i),
                        code: ErrorCode::InternalError,
                        // ~1.3 KiB of repetitive text → comfortably compressible.
                        message: "the quick brown fox jumps ".repeat(50),
                    }
                } else {
                    ServerMessage::Pong { seq: i }
                }
            })
            .collect();

        let sent = msgs.clone();
        let writer = tokio::spawn(async move {
            for m in &sent {
                let bytes = encode_server(m).expect("encode");
                write_frame_compressed(&mut server_side, &bytes, comp)
                    .await
                    .expect("write");
            }
            // Dropping `server_side` closes the pipe so the reader sees EOF.
        });

        let mut got = Vec::new();
        while let Some(frame) = read_frame(&mut client_side).await.expect("read") {
            got.push(decode_server(&frame).expect("decode"));
            if got.len() == msgs.len() {
                break;
            }
        }
        writer.await.unwrap();

        assert_eq!(got.len(), msgs.len());
        // `ServerMessage` is not `PartialEq`; compare by re-encoding.
        for (sent, recv) in msgs.iter().zip(&got) {
            assert_eq!(encode_server(sent).unwrap(), encode_server(recv).unwrap());
        }
    }
}
