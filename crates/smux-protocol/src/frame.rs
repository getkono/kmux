use thiserror::Error;

use crate::messages::{ClientMessage, ServerMessage};

/// Errors that can occur during message encoding or decoding.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),

    #[error("decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

/// Encode a `ClientMessage` into a MessagePack byte vector.
pub fn encode_client(msg: &ClientMessage) -> Result<Vec<u8>, ProtocolError> {
    rmp_serde::to_vec_named(msg).map_err(ProtocolError::Encode)
}

/// Decode a `ClientMessage` from a MessagePack byte slice.
pub fn decode_client(data: &[u8]) -> Result<ClientMessage, ProtocolError> {
    rmp_serde::from_slice(data).map_err(ProtocolError::Decode)
}

/// Encode a `ServerMessage` into a MessagePack byte vector.
pub fn encode_server(msg: &ServerMessage) -> Result<Vec<u8>, ProtocolError> {
    rmp_serde::to_vec_named(msg).map_err(ProtocolError::Encode)
}

/// Decode a `ServerMessage` from a MessagePack byte slice.
pub fn decode_server(data: &[u8]) -> Result<ServerMessage, ProtocolError> {
    rmp_serde::from_slice(data).map_err(ProtocolError::Decode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::*;

    #[test]
    fn roundtrip_client_auth() {
        let msg = ClientMessage::Auth {
            token: "secret".to_string(),
        };
        let bytes = encode_client(&msg).expect("encode");
        let decoded = decode_client(&bytes).expect("decode");
        assert!(matches!(decoded, ClientMessage::Auth { token } if token == "secret"));
    }

    #[test]
    fn roundtrip_server_pty_output() {
        let msg = ServerMessage::PtyOutput {
            session: "alpha".to_string(),
            data: b"hello\r\n".to_vec(),
        };
        let bytes = encode_server(&msg).expect("encode");
        let decoded = decode_server(&bytes).expect("decode");
        assert!(
            matches!(&decoded, ServerMessage::PtyOutput { session, data } if session == "alpha" && data == b"hello\r\n")
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
}
