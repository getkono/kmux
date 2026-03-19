use thiserror::Error;

use crate::messages::{ClientMessage, ServerMessage};

/// Errors that can occur during message encoding or decoding.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("encode/decode error: {0}")]
    Postcard(#[from] postcard::Error),
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
}
