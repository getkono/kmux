use tokio::sync::broadcast;

use crate::process::ExitStatus;

/// Events that can occur during the lifecycle of a PTY session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// The session was successfully created and the process spawned.
    Spawned { name: String },
    /// The child process exited.
    Exited { name: String, status: ExitStatus },
    /// The PTY window was resized.
    Resized { name: String, rows: u16, cols: u16 },
    /// The session was closed by the user.
    Closed { name: String },
    /// A timeout occurred.
    Timeout { name: String, kind: TimeoutKind },
}

/// The kind of timeout that fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeoutKind {
    WallClock,
    Idle,
}

/// A broadcast channel for session lifecycle events.
///
/// Clone `EventBus` to subscribe additional receivers.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<SessionEvent>,
}

impl EventBus {
    /// Create a new event bus with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Subscribe to events. The receiver will receive all future events.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.tx.subscribe()
    }

    /// Publish an event (errors are ignored if no receivers are active).
    pub fn emit(&self, event: SessionEvent) {
        let _ = self.tx.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_and_receive() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.emit(SessionEvent::Spawned {
            name: "test".to_string(),
        });

        let event = rx.recv().await.expect("should receive event");
        assert!(matches!(event, SessionEvent::Spawned { .. }));
    }

    #[tokio::test]
    async fn no_events_before_subscribe() {
        let bus = EventBus::new(16);
        bus.emit(SessionEvent::Closed {
            name: "gone".to_string(),
        });
        // Subscribe AFTER the emit -- should not see the past event
        let mut rx = bus.subscribe();
        bus.emit(SessionEvent::Closed {
            name: "present".to_string(),
        });
        let event = rx.recv().await.expect("should receive second event");
        assert!(matches!(event, SessionEvent::Closed { name, .. } if name == "present"));
    }
}
