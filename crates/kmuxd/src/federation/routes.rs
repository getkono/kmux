//! Where the answers to forwarded requests go (issue #227).
//!
//! A hub is one client to its peer, speaking for every client of its own. So
//! each answer the peer sends has to be traced back to the client that asked.
//! Two kinds of answer need two means:
//!
//! - **An answer with a `request_id`.** The hub sends every forwarded request
//!   under an id of its own ([`Routes::route`]), so ids from different
//!   clients never collide with each other or with the hub's own requests,
//!   and keeps a [`Route`] from that id back to the client and the id it
//!   used.
//! - **An answer without one** — an `Error { request_id: None }` for input,
//!   a `Signal`, a layout nudge or an `Attach`; and
//!   `SessionRenamed`. A peer handles one connection's messages in order and
//!   answers each before it reads the next (`client_handler::session`), on
//!   one ordered lane. So the answer belongs to the *oldest* message the peer
//!   may still be answering. The hub groups what it sends into runs, one per
//!   sender in a row ([`Routes::sent`]); when the sender changes it pings the
//!   peer, and the peer's `Pong` for that ping marks the run before it
//!   answered in full ([`Routes::ponged`]). An answer without an id is the
//!   front run's ([`Routes::front`]). One client in a row costs no ping at
//!   all, and the liveness ping the hub sends anyway closes a run as well.
//!
//! A route whose request the peer answers with nothing at all (`TabRename`)
//! is dropped with its run, once the `Pong` shows the peer is past it. This
//! relies on the peer answering in order, which a peer that is itself a hub
//! forwarding the request on does not; chained hubs are not supported.
//!
//! Pure bookkeeping: no I/O, no locks. The caller sends what it says to.

use std::collections::{HashMap, VecDeque};

use kmux_protocol::messages::RequestId;

pub(crate) use crate::app::Requester;

/// Where the answer to one forwarded request goes.
pub(super) struct Route {
    /// Who asked.
    pub(super) from: Requester,
    /// The id it asked under, which its answer carries back.
    pub(super) request_id: RequestId,
    /// The session the request named, by the word the requester knows it
    /// under, which the answer's ids are put back into. `None` for a create:
    /// its session has no word yet.
    pub(super) local_word: Option<String>,
}

/// What one sender sent in a row, and the ping that closed it.
struct Run {
    from: Requester,
    /// The hub's ids of its requests, whose routes go with it.
    routed: Vec<RequestId>,
    /// The `seq` of the ping sent after its last message, once another
    /// sender (or the liveness ping) closed it.
    barrier: Option<u64>,
}

/// The routes of one link. A new link starts with new ones.
pub(super) struct Routes {
    /// The next id the hub sends a request under; 1 is the handshake's list.
    next_id: RequestId,
    /// The next `seq` the hub pings under.
    next_ping: u64,
    routes: HashMap<RequestId, Route>,
    runs: VecDeque<Run>,
}

impl Default for Routes {
    fn default() -> Self {
        Self {
            next_id: 2,
            next_ping: 0,
            routes: HashMap::new(),
            runs: VecDeque::new(),
        }
    }
}

impl Routes {
    /// An id for a request of the hub's own (a list refresh, an overview).
    pub(super) fn next_id(&mut self) -> RequestId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// `from` is about to send a message. When that ends another sender's
    /// run, returns the `seq` of the ping to send first.
    pub(super) fn sent(&mut self, from: &Requester) -> Option<u64> {
        let open = self.runs.back().filter(|run| run.barrier.is_none());
        if open.is_some_and(|run| run.from.is(from)) {
            return None;
        }
        let barrier = open.is_some().then(|| self.ping());
        self.runs.push_back(Run {
            from: from.clone(),
            routed: Vec::new(),
            barrier: None,
        });
        barrier
    }

    /// Record `route` for the request its sender is sending (after
    /// [`Self::sent`]), returning the id to send it under.
    pub(super) fn route(&mut self, route: Route) -> RequestId {
        let id = self.next_id();
        if let Some(open) = self.runs.back_mut() {
            open.routed.push(id);
        }
        self.routes.insert(id, route);
        id
    }

    /// The hub is pinging the peer: the `seq` to ping under. It closes the
    /// open run.
    pub(super) fn ping(&mut self) -> u64 {
        let seq = self.next_ping;
        self.next_ping += 1;
        if let Some(open) = self.runs.back_mut()
            && open.barrier.is_none()
        {
            open.barrier = Some(seq);
        }
        seq
    }

    /// The peer answered the ping `seq`, and so everything sent before it:
    /// those runs are done, and so is every route the peer answered with
    /// nothing.
    pub(super) fn ponged(&mut self, seq: u64) {
        while let Some(run) = self.runs.front() {
            if run.barrier.is_none_or(|barrier| barrier > seq) {
                break;
            }
            for id in &run.routed {
                self.routes.remove(id);
            }
            self.runs.pop_front();
        }
    }

    /// The route of the answer carrying `id`, taken: an answer goes once.
    pub(super) fn take(&mut self, id: RequestId) -> Option<Route> {
        self.routes.remove(&id)
    }

    /// Who an answer without a `request_id` is for: the sender of the oldest
    /// run the peer may still be answering.
    pub(super) fn front(&self) -> Option<&Requester> {
        self.runs.front().map(|run| &run.from)
    }

    /// Every route still waiting, for a link that dropped. Nothing is left.
    pub(super) fn drain(&mut self) -> Vec<Route> {
        self.runs.clear();
        self.routes.drain().map(|(_, route)| route).collect()
    }
}

#[cfg(test)]
mod tests {
    use kmux_protocol::messages::{ClientId, ServerMessage};

    use super::*;
    use crate::fixtures::make_outbound;

    fn client(id: u64) -> (Requester, crate::outbound::OutboundRx) {
        let (ctrl, rx) = make_outbound();
        (Requester::client(ClientId(id), ctrl), rx)
    }

    fn route(from: &Requester, request_id: RequestId) -> Route {
        Route {
            from: from.clone(),
            request_id,
            local_word: Some("hawk".into()),
        }
    }

    /// One client sending in a row costs no ping; the next sender closes
    /// its run with one, and answers without an id go to the oldest run
    /// until the peer's pong shows it answered in full.
    #[test]
    fn a_change_of_sender_closes_the_run_with_a_ping() {
        let (a, _a_rx) = client(1);
        let (b, _b_rx) = client(2);
        let mut routes = Routes::default();
        assert_eq!(routes.sent(&a), None, "the first run needs no barrier");
        assert_eq!(routes.sent(&a), None, "nor does the same sender again");
        assert_eq!(routes.sent(&b), Some(0), "another sender closes it");
        let front_is = |routes: &Routes, who: &Requester| routes.front().is_some_and(|f| f.is(who));
        assert!(front_is(&routes, &a));
        assert_eq!(routes.sent(&Requester::Hub), Some(1));

        routes.ponged(0);
        assert!(front_is(&routes, &b));
        routes.ponged(1);
        assert!(front_is(&routes, &Requester::Hub), "the hub's own");
        routes.ponged(5);
        assert!(
            routes.front().is_some(),
            "the open run waits for its own barrier"
        );
    }

    /// The liveness ping closes the open run too, so the same client's next
    /// message starts a new one.
    #[test]
    fn the_liveness_ping_closes_the_open_run() {
        let (a, _rx) = client(1);
        let mut routes = Routes::default();
        routes.sent(&a);
        assert_eq!(routes.ping(), 0);
        assert_eq!(routes.sent(&a), None, "a closed run needs no second ping");
        assert_eq!(routes.ping(), 1);
        routes.ponged(0);
        assert!(routes.front().is_some(), "the second run is still open");
        routes.ponged(1);
        assert!(routes.front().is_none());
    }

    /// A request goes up under the hub's own id and comes back to its
    /// sender's; one the peer never answered goes with its run.
    #[test]
    fn a_route_is_taken_once_or_dropped_with_its_run() {
        let (a, _rx) = client(1);
        let mut routes = Routes::default();
        routes.sent(&a);
        let answered = routes.route(route(&a, 7));
        let unanswered = routes.route(route(&a, 8));
        assert_eq!(
            (answered, unanswered),
            (2, 3),
            "the hub's ids, after the handshake's"
        );
        assert_eq!(routes.next_id(), 4);

        assert_eq!(routes.take(answered).map(|r| r.request_id), Some(7));
        assert!(routes.take(answered).is_none(), "an answer goes once");
        let barrier = routes.ping();
        routes.ponged(barrier);
        assert!(routes.take(unanswered).is_none(), "dropped with its run");
    }

    /// A dropped link hands back every waiting route and forgets the runs.
    #[test]
    fn draining_empties_the_routes() {
        let (a, _rx) = client(1);
        let mut routes = Routes::default();
        routes.sent(&a);
        routes.route(route(&a, 7));
        let drained: Vec<_> = routes.drain().into_iter().map(|r| r.request_id).collect();
        assert_eq!(drained, vec![7]);
        assert!(routes.front().is_none());
        assert!(routes.drain().is_empty());
    }

    /// Two clients are two senders, and so is one client over two channels;
    /// only a client answers.
    #[test]
    fn a_sender_is_a_client_on_one_channel() {
        let (a, mut a_rx) = client(1);
        let (again, _rx) = client(1);
        assert!(a.is(&a.clone()));
        assert!(!a.is(&again), "another channel");
        assert!(!a.is(&Requester::Hub));
        assert!(Requester::Hub.is(&Requester::Hub));
        a.answer(ServerMessage::Pong { seq: 3 });
        Requester::Hub.answer(ServerMessage::Pong { seq: 4 });
        assert!(matches!(
            a_rx.try_recv(),
            Ok(ServerMessage::Pong { seq: 3 })
        ));
    }
}
