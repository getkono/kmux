//! Decoding for nested wire enums that a newer peer may extend.
//!
//! A nested enum is decoded by an older build too. With serde's derive alone, a
//! variant that build has never heard of fails the decode — and with it the
//! whole frame, taking down an unrelated `Event` or reply. `#[serde(other)]`
//! only helps for a *unit* variant: a newer variant carrying data arrives as a
//! one-entry map, and the derived code then fails to read its payload as a
//! unit.
//!
//! [`wire_enum!`] closes that gap. The enum keeps its derived encoding
//! (`#[serde(remote = "Self")]` turns the derive into inherent functions) and
//! gains a `Deserialize` that reads the variant name first: a name the derived
//! code knows is decoded by it, unchanged; any other name has its payload
//! skipped and becomes the enum's `Unknown` variant. The frame around it still
//! decodes, and the receiver decides what an `Unknown` means (see
//! `docs/architecture-protocol-versioning.md`, "Unknown variants").
//!
//! This relies on a self-describing codec (`deserialize_any`), which the
//! data plane's named MessagePack and the JSON control plane both are. An enum
//! that also crosses a Postcard boundary (the VT-worker contract, persisted
//! daemon state) cannot use it and stays strict.

use std::fmt;
use std::marker::PhantomData;

use serde::de::value::{EnumAccessDeserializer, StrDeserializer, StringDeserializer};
use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, IgnoredAny, IntoDeserializer, MapAccess,
    VariantAccess, Visitor,
};

/// A wire enum whose unknown variants decode to an `Unknown` fallback.
///
/// Implemented by [`wire_enum!`]; not meant to be implemented by hand.
pub trait WireEnum: Sized {
    /// Decode with the derived (strict) implementation.
    fn decode_known<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error>;
    /// The fallback for a variant this build does not know.
    fn unknown() -> Self;
    /// Whether this value is the fallback.
    fn is_unknown(&self) -> bool;
}

/// Implement `Serialize`, `Deserialize` and [`WireEnum`] for an enum declared
/// with `#[derive(Serialize, Deserialize)]`, `#[serde(remote = "Self")]` and a
/// `#[serde(other)] Unknown` unit variant.
macro_rules! wire_enum {
    ($ty:ty) => {
        impl ::serde::Serialize for $ty {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                <$ty>::serialize(self, s)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $ty {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                $crate::messages::wire_enum::deserialize(d)
            }
        }

        impl $crate::messages::wire_enum::WireEnum for $ty {
            fn decode_known<'de, D: ::serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                <$ty>::deserialize(d)
            }

            fn unknown() -> Self {
                Self::Unknown
            }

            fn is_unknown(&self) -> bool {
                matches!(self, Self::Unknown)
            }
        }
    };
}
pub(crate) use wire_enum;

/// Decode a [`WireEnum`], mapping a variant this build does not know to its
/// `Unknown` fallback.
pub fn deserialize<'de, D: Deserializer<'de>, T: WireEnum>(d: D) -> Result<T, D::Error> {
    d.deserialize_any(LenientVisitor(PhantomData))
}

/// Whether `tag` names no variant this build knows. The derived code answers:
/// decoding the bare name gives `Unknown` (via `#[serde(other)]`) exactly when
/// it is not one of the enum's own names.
fn is_unknown_tag<T: WireEnum, E: de::Error>(tag: &str) -> bool {
    matches!(T::decode_known(StrDeserializer::<E>::new(tag)), Ok(value) if value.is_unknown())
}

struct LenientVisitor<T>(PhantomData<T>);

impl<'de, T: WireEnum> Visitor<'de> for LenientVisitor<T> {
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an enum variant name, or a one-entry map from a variant name to its payload")
    }

    /// A unit variant travels as its bare name.
    fn visit_str<E: de::Error>(self, tag: &str) -> Result<T, E> {
        T::decode_known(StrDeserializer::<E>::new(tag))
    }

    /// Any other variant is a one-entry map from its name to its payload.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<T, A::Error> {
        let Some(tag) = map.next_key::<String>()? else {
            return Err(de::Error::invalid_length(0, &self));
        };
        let value = if is_unknown_tag::<T, A::Error>(&tag) {
            map.next_value::<IgnoredAny>()?;
            T::unknown()
        } else {
            T::decode_known(EnumAccessDeserializer::new(Tagged { tag, map: &mut map }))?
        };
        if map.next_key::<IgnoredAny>()?.is_some() {
            return Err(de::Error::invalid_length(2, &self));
        }
        Ok(value)
    }
}

/// The variant name already read from the map, and the map positioned at its
/// payload: an [`EnumAccess`] the derived code consumes as if it had read the
/// name itself.
struct Tagged<'a, A> {
    tag: String,
    map: &'a mut A,
}

impl<'de, 'a, A: MapAccess<'de>> EnumAccess<'de> for Tagged<'a, A> {
    type Error = A::Error;
    type Variant = Payload<'a, A>;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), A::Error> {
        let tag: StringDeserializer<A::Error> = self.tag.into_deserializer();
        Ok((seed.deserialize(tag)?, Payload(self.map)))
    }
}

/// The payload half of a one-entry variant map.
struct Payload<'a, A>(&'a mut A);

impl<'de, A: MapAccess<'de>> VariantAccess<'de> for Payload<'_, A> {
    type Error = A::Error;

    fn unit_variant(self) -> Result<(), A::Error> {
        self.0.next_value::<IgnoredAny>().map(|_| ())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, A::Error> {
        self.0.next_value_seed(seed)
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, A::Error> {
        self.0.next_value_seed(TupleSeed { len, visitor })
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, A::Error> {
        self.0.next_value_seed(StructSeed { fields, visitor })
    }
}

struct TupleSeed<V> {
    len: usize,
    visitor: V,
}

impl<'de, V: Visitor<'de>> DeserializeSeed<'de> for TupleSeed<V> {
    type Value = V::Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<V::Value, D::Error> {
        d.deserialize_tuple(self.len, self.visitor)
    }
}

struct StructSeed<V> {
    fields: &'static [&'static str],
    visitor: V,
}

impl<'de, V: Visitor<'de>> DeserializeSeed<'de> for StructSeed<V> {
    type Value = V::Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<V::Value, D::Error> {
        d.deserialize_struct("", self.fields, self.visitor)
    }
}

#[cfg(test)]
mod tests {
    //! The decoder itself, against a test-only enum pair: `Current` is what
    //! this build knows, `Newer` is what a newer peer sends. The per-enum
    //! fixtures for the real wire enums live beside each enum.

    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(remote = "Self")]
    enum Current {
        Unit,
        Newtype(u32),
        Tuple(u8, u8),
        Struct {
            a: String,
            #[serde(default)]
            b: bool,
        },
        #[serde(other)]
        Unknown,
    }
    wire_enum!(Current);

    #[derive(Serialize)]
    enum Newer {
        Unit,
        Struct { a: String },
        NewUnit,
        NewStruct { x: Vec<u32>, y: String },
        NewNewtype(String),
        NewTuple(u8, u8, u8),
    }

    /// A frame around the enum, so each case also proves the decoder leaves
    /// the stream positioned at the next field.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Frame<E> {
        e: E,
        after: u32,
    }

    fn decode(newer: Newer) -> Frame<Current> {
        let bytes = rmp_serde::to_vec_named(&Frame { e: newer, after: 7 }).expect("encode");
        rmp_serde::from_slice(&bytes).expect("decode")
    }

    #[test]
    fn unknown_variants_of_every_shape_decode_to_unknown() {
        for newer in [
            Newer::NewUnit,
            Newer::NewStruct {
                x: vec![1, 2],
                y: "y".into(),
            },
            Newer::NewNewtype("n".into()),
            Newer::NewTuple(1, 2, 3),
        ] {
            assert_eq!(
                decode(newer),
                Frame {
                    e: Current::Unknown,
                    after: 7
                }
            );
        }
    }

    #[test]
    fn known_variants_decode_unchanged() {
        assert_eq!(decode(Newer::Unit).e, Current::Unit);
        assert_eq!(
            decode(Newer::Struct { a: "a".into() }).e,
            Current::Struct {
                a: "a".into(),
                b: false
            }
        );
        for current in [
            Current::Unit,
            Current::Newtype(3),
            Current::Tuple(4, 5),
            Current::Struct {
                a: "s".into(),
                b: true,
            },
        ] {
            let bytes = rmp_serde::to_vec_named(&Frame {
                e: &current,
                after: 9,
            })
            .expect("encode");
            let back: Frame<Current> = rmp_serde::from_slice(&bytes).expect("decode");
            assert_eq!(
                back,
                Frame {
                    e: current,
                    after: 9
                }
            );
        }
    }

    #[test]
    fn encoding_is_the_derived_encoding() {
        // The fallback changes decoding only: a known variant is encoded by
        // the derived code, byte for byte.
        #[derive(Serialize)]
        enum Plain {
            Struct { a: String, b: bool },
        }
        assert_eq!(
            rmp_serde::to_vec_named(&Current::Struct {
                a: "a".into(),
                b: true
            })
            .expect("encode"),
            rmp_serde::to_vec_named(&Plain::Struct {
                a: "a".into(),
                b: true
            })
            .expect("encode"),
        );
    }

    #[test]
    fn a_known_variant_with_a_malformed_payload_is_still_an_error() {
        // Leniency is for names this build does not know, never a licence to
        // swallow a corrupt payload of one it does.
        #[derive(Serialize)]
        enum Corrupt {
            Newtype(String),
        }
        let bytes = rmp_serde::to_vec_named(&Corrupt::Newtype("not a u32".into())).expect("encode");
        assert!(rmp_serde::from_slice::<Current>(&bytes).is_err());
    }

    #[test]
    fn a_map_with_more_than_one_variant_is_an_error() {
        let bytes = rmp_serde::to_vec_named(&std::collections::BTreeMap::from([
            ("NewUnit", 1u8),
            ("Unit", 2u8),
        ]))
        .expect("encode");
        assert!(rmp_serde::from_slice::<Current>(&bytes).is_err());
        let empty = rmp_serde::to_vec_named(&std::collections::BTreeMap::<String, u8>::new())
            .expect("encode");
        assert!(rmp_serde::from_slice::<Current>(&empty).is_err());
    }

    #[test]
    fn json_decodes_the_same_way() {
        // The JSON control plane carries some of these enums too.
        let unknown: Current = serde_json::from_str(r#"{"NewStruct":{"x":[1],"y":"y"}}"#).unwrap();
        assert_eq!(unknown, Current::Unknown);
        let known: Current = serde_json::from_str(r#"{"Newtype":4}"#).unwrap();
        assert_eq!(known, Current::Newtype(4));
        assert_eq!(
            serde_json::from_str::<Current>(r#""Unit""#).unwrap(),
            Current::Unit
        );
    }
}

#[cfg(test)]
mod fixtures {
    //! One fixture per wire enum: a newer peer's message carrying a variant
    //! this build has never heard of still decodes, with that one value as
    //! `Unknown`. Each message is encoded from a stand-in of its newer schema,
    //! so the bytes are exactly what that peer would put on the wire.

    use serde::Serialize;

    use crate::messages::*;
    use crate::{decode_client, decode_server};

    #[derive(Serialize)]
    enum NewerEvent {
        SessionWiggled {
            word_id: &'static str,
        },
        PaneProgressChanged {
            pane_id: &'static str,
            state: &'static str,
            progress: Option<u8>,
        },
        PaneAttention {
            pane_id: &'static str,
            kind: &'static str,
            title: &'static str,
            body: &'static str,
            attention_id: u64,
        },
    }

    #[derive(Serialize)]
    enum NewerAuthFailure {
        RateLimited { retry_after_secs: u32 },
    }

    #[derive(Serialize)]
    enum NewerPeerTarget {
        Tailscale { node: &'static str },
    }

    #[derive(Serialize)]
    #[serde(tag = "type", content = "data")]
    enum NewerServer {
        Error {
            request_id: Option<u64>,
            code: &'static str,
            message: &'static str,
        },
        Event {
            event: NewerEvent,
        },
        AuthResult {
            success: bool,
            reason: Option<String>,
            failure: Option<NewerAuthFailure>,
            compression: Option<String>,
        },
    }

    #[derive(Serialize)]
    #[serde(tag = "type", content = "data")]
    enum NewerClient {
        Auth {
            token: &'static str,
            protocol_range: ProtocolRange,
            capabilities: ClientCapabilities,
            client_kind: &'static str,
        },
        Notify {
            request_id: u64,
            pane_id: &'static str,
            kind: &'static str,
            title: &'static str,
            body: &'static str,
        },
        OpenPeer {
            request_id: u64,
            target: NewerPeerTarget,
        },
        ApplyLayoutScheme {
            word_id: &'static str,
            tab_index: u32,
            scheme: &'static str,
        },
    }

    fn server(msg: &NewerServer) -> ServerMessage {
        let bytes = rmp_serde::to_vec_named(msg).expect("encode");
        decode_server(&bytes).expect("the enclosing message still decodes")
    }

    fn client(msg: &NewerClient) -> ClientMessage {
        let bytes = rmp_serde::to_vec_named(msg).expect("encode");
        decode_client(&bytes).expect("the enclosing message still decodes")
    }

    #[test]
    fn error_code() {
        let msg = server(&NewerServer::Error {
            request_id: Some(4),
            code: "TabNotFound",
            message: "no tab 9",
        });
        assert!(matches!(
            msg,
            ServerMessage::Error { request_id: Some(4), code: ErrorCode::Unknown, message }
                if message == "no tab 9"
        ));
    }

    #[test]
    fn auth_failure() {
        let msg = server(&NewerServer::AuthResult {
            success: false,
            reason: Some("slow down".into()),
            failure: Some(NewerAuthFailure::RateLimited {
                retry_after_secs: 3,
            }),
            compression: None,
        });
        assert!(matches!(
            msg,
            ServerMessage::AuthResult {
                success: false,
                failure: Some(AuthFailure::Unknown),
                reason: Some(reason),
                ..
            } if reason == "slow down"
        ));
    }

    #[test]
    fn compression() {
        let msg = server(&NewerServer::AuthResult {
            success: true,
            reason: None,
            failure: None,
            compression: Some("Brotli".into()),
        });
        assert!(matches!(
            msg,
            ServerMessage::AuthResult {
                success: true,
                compression: Some(Compression::Unknown),
                ..
            }
        ));
    }

    #[test]
    fn session_event() {
        let msg = server(&NewerServer::Event {
            event: NewerEvent::SessionWiggled { word_id: "eagle" },
        });
        assert!(matches!(
            msg,
            ServerMessage::Event {
                event: SessionEventMsg::Unknown
            }
        ));
    }

    #[test]
    fn pane_progress_state() {
        let msg = server(&NewerServer::Event {
            event: NewerEvent::PaneProgressChanged {
                pane_id: "eagle/0",
                state: "Glowing",
                progress: Some(40),
            },
        });
        assert!(matches!(
            msg,
            ServerMessage::Event {
                event: SessionEventMsg::PaneProgressChanged {
                    state: PaneProgressState::Unknown,
                    progress: Some(40),
                    ..
                }
            }
        ));
    }

    #[test]
    fn attention_kind() {
        let event = server(&NewerServer::Event {
            event: NewerEvent::PaneAttention {
                pane_id: "eagle/0",
                kind: "Urgent",
                title: "t",
                body: "b",
                attention_id: 7,
            },
        });
        assert!(matches!(
            event,
            ServerMessage::Event {
                event: SessionEventMsg::PaneAttention {
                    kind: AttentionKind::Unknown,
                    attention_id: 7,
                    ..
                }
            }
        ));
        let request = client(&NewerClient::Notify {
            request_id: 2,
            pane_id: "eagle/0",
            kind: "Urgent",
            title: "t",
            body: "b",
        });
        assert!(matches!(
            request,
            ClientMessage::Notify {
                request_id: 2,
                kind: AttentionKind::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn frontend_kind() {
        let msg = client(&NewerClient::Auth {
            token: "tok",
            protocol_range: PROTOCOL_RANGE,
            capabilities: ClientCapabilities::default(),
            client_kind: "Tui",
        });
        assert!(matches!(
            msg,
            ClientMessage::Auth { client_kind: FrontendKind::Unknown, token, .. } if token == "tok"
        ));
    }

    #[test]
    fn peer_target() {
        let msg = client(&NewerClient::OpenPeer {
            request_id: 5,
            target: NewerPeerTarget::Tailscale { node: "box" },
        });
        assert!(matches!(
            msg,
            ClientMessage::OpenPeer {
                request_id: 5,
                target: PeerTarget::Unknown
            }
        ));
    }

    #[test]
    fn layout_scheme() {
        let msg = client(&NewerClient::ApplyLayoutScheme {
            word_id: "eagle",
            tab_index: 1,
            scheme: "Tiled",
        });
        assert!(matches!(
            msg,
            ClientMessage::ApplyLayoutScheme {
                tab_index: 1,
                scheme: LayoutScheme::Unknown,
                ..
            }
        ));
    }
}
