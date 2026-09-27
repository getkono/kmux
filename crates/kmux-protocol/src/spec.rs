//! `docs/protocol.md` is the normative specification of this crate's wire
//! protocol. These tests hold the document to the code, in both directions,
//! so neither can drift from the other (docs/testing.md R9: one invariant
//! test per claim, not spot checks):
//!
//! - the message catalogue lists every `ClientMessage` and `ServerMessage`
//!   variant exactly once, and nothing else;
//! - the timing table lists every constant in [`crate::timing`], with its
//!   value;
//! - every test a state table says pins a transition exists.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::messages::{ClientMessage, ServerMessage};
use crate::timing;

/// The specification, as the build that tests it sees it.
const SPEC: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/protocol.md"
));

/// The rows of the table that follows `<!-- spec:{name} -->` in the spec,
/// each split into its trimmed cells (header and separator rows skipped).
fn table(name: &str) -> Vec<Vec<String>> {
    let marker = format!("<!-- spec:{name} -->");
    let (_, after) = SPEC
        .split_once(&marker)
        .unwrap_or_else(|| panic!("docs/protocol.md has no {marker}"));
    after
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .skip(2)
        .map(|line| {
            line.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect()
}

/// Every name set in backticks in `cell`.
fn code_spans(cell: &str) -> Vec<&str> {
    cell.split('`').skip(1).step_by(2).collect()
}

/// The first backticked name in each row's first cell.
fn first_column(rows: &[Vec<String>]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            code_spans(&row[0])
                .first()
                .unwrap_or_else(|| panic!("row without a `name`: {row:?}"))
                .to_string()
        })
        .collect()
}

/// The variant names of a top-level message enum, from the enum itself:
/// serde's error for an unknown `type` lists every variant it knows.
fn variants<T: DeserializeOwned + std::fmt::Debug>() -> BTreeSet<String> {
    let err = serde_json::from_str::<T>(r#"{"type":"\u0000","data":null}"#)
        .expect_err("no variant is named NUL");
    let text = err.to_string();
    let expected = text
        .split_once("expected one of ")
        .unwrap_or_else(|| panic!("unexpected serde error: {text}"))
        .1;
    code_spans(expected)
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Assert `listed` names exactly `actual`, each once.
fn assert_lists_exactly(what: &str, listed: &[String], actual: &BTreeSet<String>) {
    let mut seen = BTreeSet::new();
    for name in listed {
        assert!(
            seen.insert(name.clone()),
            "{what}: `{name}` is listed twice"
        );
    }
    let missing: Vec<_> = actual.difference(&seen).collect();
    let extra: Vec<_> = seen.difference(actual).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{what}: the catalogue does not match the enum — missing {missing:?}, not a variant {extra:?}"
    );
}

#[test]
fn the_catalogue_lists_every_client_message_and_no_other() {
    assert_lists_exactly(
        "ClientMessage",
        &first_column(&table("client-messages")),
        &variants::<ClientMessage>(),
    );
}

#[test]
fn the_catalogue_lists_every_server_message_and_no_other() {
    assert_lists_exactly(
        "ServerMessage",
        &first_column(&table("server-messages")),
        &variants::<ServerMessage>(),
    );
}

/// Every constant in [`crate::timing`], by name.
fn timing_constants() -> Vec<(&'static str, String)> {
    fn secs(d: Duration) -> String {
        if d.subsec_millis() == 0 {
            format!("{} s", d.as_secs())
        } else {
            format!("{} ms", d.as_millis())
        }
    }
    vec![
        ("PING_INTERVAL", secs(timing::PING_INTERVAL)),
        ("SILENCE_TIMEOUT", secs(timing::SILENCE_TIMEOUT)),
        ("PONG_DEADLINE", secs(timing::PONG_DEADLINE)),
        ("AUTH_DEADLINE", secs(timing::AUTH_DEADLINE)),
        ("AUTH_REPLY_TIMEOUT", secs(timing::AUTH_REPLY_TIMEOUT)),
        ("AUTH_REFUSAL_FLUSH", secs(timing::AUTH_REFUSAL_FLUSH)),
        (
            "TRANSPORT_HANDSHAKE_TIMEOUT",
            secs(timing::TRANSPORT_HANDSHAKE_TIMEOUT),
        ),
        ("FRAME_WRITE_TIMEOUT", secs(timing::FRAME_WRITE_TIMEOUT)),
        ("QUIC_IDLE_TIMEOUT", secs(timing::QUIC_IDLE_TIMEOUT)),
        ("QUIC_KEEP_ALIVE", secs(timing::QUIC_KEEP_ALIVE)),
        (
            "PANE_STREAM_STALL_TIMEOUT",
            secs(timing::PANE_STREAM_STALL_TIMEOUT),
        ),
        ("PEER_CONNECT_TIMEOUT", secs(timing::PEER_CONNECT_TIMEOUT)),
        ("PEER_LIST_TIMEOUT", secs(timing::PEER_LIST_TIMEOUT)),
        ("PEER_CREATE_TIMEOUT", secs(timing::PEER_CREATE_TIMEOUT)),
        ("PEER_OVERVIEW_TIMEOUT", secs(timing::PEER_OVERVIEW_TIMEOUT)),
        ("BACKOFF_MIN", secs(timing::BACKOFF_MIN)),
        ("BACKOFF_MAX", secs(timing::BACKOFF_MAX)),
        (
            "BACKOFF_JITTER_CAP_PERMILLE",
            format!("{} ‰", timing::BACKOFF_JITTER_CAP_PERMILLE),
        ),
    ]
}

#[test]
fn the_timing_table_is_the_timing_module() {
    let rows = table("timing");
    let listed: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            (
                first_column(std::slice::from_ref(row))[0].clone(),
                row[1].clone(),
            )
        })
        .collect();
    let actual: Vec<(String, String)> = timing_constants()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    let mut listed_sorted = listed;
    listed_sorted.sort();
    let mut actual_sorted = actual;
    actual_sorted.sort();
    assert_eq!(
        listed_sorted, actual_sorted,
        "docs/protocol.md timing table"
    );
}

/// Every `fn name` defined anywhere under `crates/`.
fn functions_in_the_workspace() -> BTreeSet<String> {
    fn walk(dir: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).expect("readable source tree") {
            let path: PathBuf = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = std::fs::read_to_string(&path).expect("readable source");
                for rest in source.split("fn ").skip(1) {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        out.insert(name);
                    }
                }
            }
        }
    }
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut out = BTreeSet::new();
    walk(&crates, &mut out);
    out
}

#[test]
fn every_test_a_state_table_names_exists() {
    let defined = functions_in_the_workspace();
    let tables: Vec<&str> = SPEC
        .split("<!-- spec:")
        .skip(1)
        .filter_map(|rest| rest.split_once(" -->").map(|(name, _)| name))
        .filter(|name| name.starts_with("states-"))
        .collect();
    assert!(
        tables.len() >= 6,
        "connection, auth, attach, pause, peer and session each have a state table: {tables:?}"
    );
    for name in tables {
        for row in table(name) {
            let pinned = row.last().expect("a row has cells");
            let tests = code_spans(pinned);
            assert!(!tests.is_empty(), "{name}: `{row:?}` names no test");
            for test in tests {
                assert!(
                    defined.contains(test),
                    "{name}: `{test}` is not a function in crates/"
                );
            }
        }
    }
}
