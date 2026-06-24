//! `kmux notify` — raise a desktop notification from inside a pane (issue #169).
//!
//! Mirrors the raw-TCP one-shot approach of [`super::clients`]: connect, run the
//! identity handshake, send a single [`ClientMessage::Notify`], and wait for the
//! daemon's `NotifyAccepted` (or `Error`). The pane/session are read from the
//! `KMUX_PANE` / `KMUX_SESSION` env vars kmux exports into every pane, and an
//! optional Claude Code hook payload piped on stdin enriches the kind/body.

use kmux_protocol::messages::{AttentionKind, ClientMessage, ServerMessage};
use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
use tokio::net::TcpStream;

use super::{authenticate, resolve_connection};

pub struct NotifyConfig<'a> {
    pub server: Option<&'a str>,
    pub ssh_port: Option<u16>,
    /// Pane id override; falls back to `$KMUX_PANE`.
    pub pane: Option<String>,
    /// Kind override; falls back to the piped hook payload, then `TurnDone`.
    pub kind: Option<AttentionKind>,
    /// Title override; falls back to a session-derived summary.
    pub title: Option<String>,
    /// Body override; falls back to the piped hook `message`, then the summary.
    pub body: Option<String>,
}

pub async fn run_notify(cfg: NotifyConfig<'_>) -> anyhow::Result<()> {
    let pane = cfg
        .pane
        .or_else(|| std::env::var("KMUX_PANE").ok())
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "not inside a kmux pane: KMUX_PANE is unset; run this in a kmux \
                 pane or pass --pane <word/idx>"
            )
        })?;

    // Session word for the default title: prefer the env kmux set, else derive
    // it from the pane id.
    let session = std::env::var("KMUX_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| kmux_protocol::pane_word(&pane).map(str::to_string))
        .unwrap_or_default();

    let hook = read_hook_stdin();
    let (kind, title, body) =
        resolve_attention(cfg.kind, cfg.title, cfg.body, &session, hook.as_ref());

    let conn = resolve_connection(cfg.server, cfg.ssh_port).await?;
    let tcp_port = conn.tcp_port.unwrap_or(conn.port);
    let stream = TcpStream::connect(format!("{}:{}", conn.host, tcp_port))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}:{}: {e}", conn.host, tcp_port))?;
    let (mut read_half, mut write_half) = stream.into_split();
    authenticate(&mut read_half, &mut write_half, conn.token).await?;

    write_frame(
        &mut write_half,
        &encode_client(&ClientMessage::Notify {
            request_id: 1,
            pane_id: pane.clone(),
            kind,
            title,
            body,
        })?,
    )
    .await?;

    loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before notify ack"))?;
        match decode_server(&data)? {
            ServerMessage::NotifyAccepted { .. } => return Ok(()),
            ServerMessage::Error { message, .. } => anyhow::bail!("notify failed: {message}"),
            _ => continue,
        }
    }
}

/// Read a Claude Code hook payload from stdin, if one is piped. Returns `None`
/// when stdin is a terminal (interactive use) or carries no JSON object.
fn read_hook_stdin() -> Option<serde_json::Value> {
    use std::io::{IsTerminal, Read};
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut buf = String::new();
    if stdin.read_to_string(&mut buf).is_ok() && !buf.trim().is_empty() {
        serde_json::from_str(&buf).ok()
    } else {
        None
    }
}

/// Resolve the final `(kind, title, body)` from explicit flags, an optional
/// Claude hook payload, and the session word, in that precedence:
///
/// - **kind**: flag → hook `hook_event_name` (`Notification` ⇒ needs-input,
///   anything else ⇒ turn-done) → `TurnDone`.
/// - **title**: flag → `"<session> <summary>"` (or `"kmux <summary>"`).
/// - **body**: flag → hook `message` → the kind summary.
fn resolve_attention(
    kind: Option<AttentionKind>,
    title: Option<String>,
    body: Option<String>,
    session: &str,
    hook: Option<&serde_json::Value>,
) -> (AttentionKind, String, String) {
    let hook_event = hook
        .and_then(|h| h.get("hook_event_name"))
        .and_then(|v| v.as_str());
    let hook_message = hook
        .and_then(|h| h.get("message"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty());

    let kind = kind.unwrap_or(match hook_event {
        Some("Notification") => AttentionKind::NeedsInput,
        // `Stop`, `SubagentStop`, or no/unknown hook ⇒ a turn completed.
        _ => AttentionKind::TurnDone,
    });

    let summary = match kind {
        AttentionKind::TurnDone => "finished a turn",
        AttentionKind::NeedsInput => "needs your input",
    };

    let title = title.unwrap_or_else(|| {
        if session.is_empty() {
            format!("kmux {summary}")
        } else {
            format!("{session} {summary}")
        }
    });

    let body = body
        .or_else(|| hook_message.map(str::to_string))
        .unwrap_or_else(|| summary.to_string());

    (kind, title, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flags_take_precedence() {
        let hook = json!({ "hook_event_name": "Notification", "message": "perm?" });
        let (kind, title, body) = resolve_attention(
            Some(AttentionKind::TurnDone),
            Some("T".to_string()),
            Some("B".to_string()),
            "eagle",
            Some(&hook),
        );
        assert_eq!(kind, AttentionKind::TurnDone);
        assert_eq!(title, "T");
        assert_eq!(body, "B");
    }

    #[test]
    fn infers_needs_input_from_notification_hook() {
        let hook = json!({ "hook_event_name": "Notification", "message": "Approve?" });
        let (kind, title, body) = resolve_attention(None, None, None, "eagle", Some(&hook));
        assert_eq!(kind, AttentionKind::NeedsInput);
        assert_eq!(title, "eagle needs your input");
        assert_eq!(body, "Approve?");
    }

    #[test]
    fn infers_turn_done_from_stop_hook() {
        let hook = json!({ "hook_event_name": "Stop" });
        let (kind, title, body) = resolve_attention(None, None, None, "eagle", Some(&hook));
        assert_eq!(kind, AttentionKind::TurnDone);
        assert_eq!(title, "eagle finished a turn");
        // No hook message ⇒ body falls back to the summary.
        assert_eq!(body, "finished a turn");
    }

    #[test]
    fn defaults_without_hook_or_session() {
        let (kind, title, body) = resolve_attention(None, None, None, "", None);
        assert_eq!(kind, AttentionKind::TurnDone);
        assert_eq!(title, "kmux finished a turn");
        assert_eq!(body, "finished a turn");
    }
}
