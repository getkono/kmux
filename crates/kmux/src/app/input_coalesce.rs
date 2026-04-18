use crossterm::event::{Event, EventStream};
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Max events drained from the crossterm stream in one event-loop iteration.
/// Caps the coalescing window so server messages and other arms eventually get a turn.
pub const INPUT_DRAIN_CAP: usize = 64;

/// Non-blockingly drain additional crossterm events after `first` into a single batch.
///
/// Uses a noop waker so polling the stream returns `Pending` immediately when the OS
/// event queue is empty, without blocking or registering a real wake-up.
pub fn drain_events(stream: &mut EventStream, first: Event) -> Vec<Event> {
    let mut events = vec![first];
    let waker = futures::task::noop_waker_ref();
    let mut cx = Context::from_waker(waker);
    while events.len() < INPUT_DRAIN_CAP {
        match Pin::new(&mut *stream).poll_next(&mut cx) {
            Poll::Ready(Some(Ok(e))) => events.push(e),
            _ => break,
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_cap_is_in_range() {
        const { assert!(INPUT_DRAIN_CAP >= 8) };
        const { assert!(INPUT_DRAIN_CAP <= 256) };
    }
}
