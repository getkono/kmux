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

//  Length-prefixed framing for QUIC byte streams 

/// Maximum frame size (16 MiB) -- prevents unbounded allocations from malformed data.
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Write a length-prefixed frame: `[u32 big-endian length][payload]`.
#[cfg(feature = "framing")]
pub async fn write_frame<W: tokio::io::AsyncWriteExt + Unpin>(
    w: &mut W,
    data: &[u8],
) -> Result<(), ProtocolError> {
    let len = data.len() as u32;
    if len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            size: len,
            max: MAX_FRAME_SIZE,
        });
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(data).await?;
    Ok(())
}

/// Read a length-prefixed frame. Returns `None` on clean stream close (EOF on length prefix).
#[cfg(feature = "framing")]
pub async fn read_frame<R: tokio::io::AsyncReadExt + Unpin>(
    r: &mut R,
) -> Result<Option<Vec<u8>>, ProtocolError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ProtocolError::Io(e)),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            size: len,
            max: MAX_FRAME_SIZE,
        });
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
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
        };
        let bytes = encode_client(&msg).expect("encode");
        let decoded = decode_client(&bytes).expect("decode");
        assert!(matches!(decoded, ClientMessage::Auth { token, .. } if token == "secret"));
    }

    #[test]
    #[allow(deprecated)]
    fn roundtrip_server_pty_output() {
        let msg = ServerMessage::PtyOutput {
            session: "alpha".to_string(),
            data: b"hello\r\n".to_vec(),
            seqno: SequenceNo(42),
        };
        let bytes = encode_server(&msg).expect("encode");
        let decoded = decode_server(&bytes).expect("decode");
        assert!(
            matches!(&decoded, ServerMessage::PtyOutput { session, data, seqno }
                if session == "alpha" && data == b"hello\r\n" && seqno.0 == 42)
        );
    }

    #[test]
    fn roundtrip_client_session_create() {
        let msg = ClientMessage::SessionCreate {
            request_id: 42,
            name: "my-session".to_string(),
            program: Some("/bin/bash".to_string()),
            args: vec![],
            size: TermSize {
                rows: 40,
                cols: 120,
            },
        };
        let bytes = encode_client(&msg).expect("encode");
        let decoded = decode_client(&bytes).expect("decode");
        assert!(
            matches!(&decoded, ClientMessage::SessionCreate { request_id: 42, name, .. } if name == "my-session")
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
            session: "s1".to_string(),
            last_seqno: Some(SequenceNo(100)),
        };
        let bytes = encode_client(&msg).expect("encode");
        let decoded = decode_client(&bytes).expect("decode");
        assert!(
            matches!(&decoded, ClientMessage::Attach { session, last_seqno: Some(SequenceNo(100)) }
                if session == "s1")
        );
    }

    #[test]
    fn roundtrip_auth_result_with_client_id() {
        let msg = ServerMessage::AuthResult {
            success: true,
            reason: None,
            client_id: Some(ClientId(7)),
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
        };
        let msg = ServerMessage::TerminalUpdate {
            session: "test".to_string(),
            diff: std::sync::Arc::new(diff),
            seqno: SequenceNo(1),
        };
        let bytes = encode_server(&msg).expect("encode");
        let decoded = decode_server(&bytes).expect("decode");
        assert!(
            matches!(&decoded, ServerMessage::TerminalUpdate { session, .. } if session == "test")
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
            },
            modes: TermModes(TermModes::APP_CURSOR),
        };
        let msg = ServerMessage::TerminalSnapshot {
            session: "snap".to_string(),
            snapshot,
            seqno: SequenceNo(99),
        };
        let bytes = encode_server(&msg).expect("encode");
        let decoded = decode_server(&bytes).expect("decode");
        match &decoded {
            ServerMessage::TerminalSnapshot {
                session, snapshot, ..
            } => {
                assert_eq!(session, "snap");
                assert_eq!(snapshot.rows, 2);
                assert_eq!(snapshot.cols, 3);
                assert_eq!(snapshot.cells.len(), 6);
                assert!(snapshot.modes.app_cursor());
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
        };
        let msg = ServerMessage::TerminalUpdate {
            session: "s".to_string(),
            diff: std::sync::Arc::new(diff),
            seqno: SequenceNo(1),
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
        let data = b"hello, QUIC!";
        let mut buf = Vec::new();
        write_frame(&mut buf, data).await.expect("write");
        assert_eq!(buf.len(), 4 + data.len());

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
        let len = (MAX_FRAME_SIZE + 1).to_be_bytes();
        let mut cursor = std::io::Cursor::new(len.to_vec());
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
}
