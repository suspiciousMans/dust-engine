//! Turn-based multiplayer: an authoritative host validates each action with
//! the game's own rules and sends every seat its own view of the new state.
//!
//! The real-time `NetHost`/`NetClient` stream 20 Hz world snapshots, which
//! suits a shooter but not a board or card game: there, nothing changes
//! between actions, some state is hidden (a player's hand), and every move
//! must be checked against the rules before anyone sees it. This module is
//! for those games.
//!
//! * The game implements [`TurnRules`]: `check` an action, `apply` it, and
//!   build the `view_for` one seat (hide what that seat may not see).
//! * [`TurnHost`] owns the rules and the seats. Remote players arrive as a
//!   [`Link`]; local players (hot-seat, bots) submit through
//!   [`TurnHost::submit_local`]. Every accepted action is broadcast as an
//!   `Update` carrying the action (for animation) and that seat's view.
//! * [`TurnClient`] sends actions and keeps the latest view. It never runs
//!   the rules itself, so a client cannot cheat or desync.
//! * Transport is a [`Link`]: a [`NetConnection`] (TCP, LAN), an in-memory
//!   [`loopback`] pair (tests, a local "remote" player), or a future
//!   WebSocket link for the browser build. Nothing here blocks or spawns.
//!
//! A dropped player keeps their seat; reconnecting with the `token` from
//! `Welcome` takes it back and receives the current view.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::NetConnection;

/// Bump when [`ClientMsg`] / [`ServerMsg`] change shape (bincode is positional).
pub const TURN_PROTOCOL_VERSION: u32 = 1;

/// A seat at the table, 0-based.
pub type Seat = u8;

/// The game's rules, owned by the host.
pub trait TurnRules {
    type Action: Serialize + DeserializeOwned + Clone;
    /// What one seat is allowed to see of the state.
    type View: Serialize + DeserializeOwned + Clone;

    /// Number of seats at the table.
    fn seats(&self) -> usize;
    /// `Err(reason)` when `seat` may not take `action` now (wrong turn,
    /// illegal move). Must not change state.
    fn check(&self, seat: Seat, action: &Self::Action) -> Result<(), String>;
    /// Applies an action that passed `check`.
    fn apply(&mut self, seat: Seat, action: &Self::Action);
    /// The state as `seat` sees it.
    fn view_for(&self, seat: Seat) -> Self::View;
}

/// A bidirectional, non-blocking message pipe carrying opaque payloads.
pub trait Link {
    fn send_raw(&mut self, payload: Vec<u8>);
    /// Every payload that arrived since the last call. Never blocks.
    fn recv_raw(&mut self) -> Vec<Vec<u8>>;
    fn is_closed(&self) -> bool;
}

impl Link for NetConnection {
    fn send_raw(&mut self, payload: Vec<u8>) {
        self.send(&payload);
    }
    fn recv_raw(&mut self) -> Vec<Vec<u8>> {
        self.pump::<Vec<u8>>()
    }
    fn is_closed(&self) -> bool {
        self.is_disconnected()
    }
}

impl Link for Box<dyn Link> {
    fn send_raw(&mut self, payload: Vec<u8>) {
        (**self).send_raw(payload)
    }
    fn recv_raw(&mut self) -> Vec<Vec<u8>> {
        (**self).recv_raw()
    }
    fn is_closed(&self) -> bool {
        (**self).is_closed()
    }
}

/// One end of an in-memory [`loopback`] pair.
pub struct LoopbackLink {
    inbox: Rc<RefCell<VecDeque<Vec<u8>>>>,
    outbox: Rc<RefCell<VecDeque<Vec<u8>>>>,
    closed: Rc<RefCell<bool>>,
}

impl LoopbackLink {
    /// Closes both ends (simulates a dropped connection).
    pub fn close(&self) {
        *self.closed.borrow_mut() = true;
    }
}

impl Link for LoopbackLink {
    fn send_raw(&mut self, payload: Vec<u8>) {
        if !*self.closed.borrow() {
            self.outbox.borrow_mut().push_back(payload);
        }
    }
    fn recv_raw(&mut self) -> Vec<Vec<u8>> {
        self.inbox.borrow_mut().drain(..).collect()
    }
    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }
}

/// Two connected in-memory links (single-threaded; for tests, local
/// players and bots that should go through the same path as remote ones).
pub fn loopback() -> (LoopbackLink, LoopbackLink) {
    let a = Rc::new(RefCell::new(VecDeque::new()));
    let b = Rc::new(RefCell::new(VecDeque::new()));
    let closed = Rc::new(RefCell::new(false));
    (
        LoopbackLink { inbox: a.clone(), outbox: b.clone(), closed: closed.clone() },
        LoopbackLink { inbox: b, outbox: a, closed },
    )
}

/// Accepts TCP connections without blocking; hand each to
/// [`TurnHost::add_peer`].
pub struct TcpAcceptor {
    listener: TcpListener,
}

impl TcpAcceptor {
    pub fn bind(port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener })
    }

    pub fn local_port(&self) -> std::io::Result<u16> {
        Ok(self.listener.local_addr()?.port())
    }

    pub fn accept_new(&self) -> Vec<NetConnection> {
        let mut out = Vec::new();
        while let Ok((stream, _)) = self.listener.accept() {
            match NetConnection::wrap(stream) {
                Ok(c) => out.push(c),
                Err(e) => log::warn!("turn host: could not set up connection: {e}"),
            }
        }
        out
    }
}

/// Connects to a host over TCP. The connect itself blocks briefly (LAN).
pub fn connect_tcp(addr: &str) -> std::io::Result<NetConnection> {
    NetConnection::wrap(TcpStream::connect(addr)?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMsg<A> {
    Hello { protocol: u32, game_id: String, name: String, token: Option<u64> },
    Action { client_seq: u32, action: A },
    Bye,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMsg<A, V> {
    Welcome { seat: Seat, token: u64, version: u64, view: V, names: Vec<Option<String>> },
    Reject { reason: String },
    Update { version: u64, by: Seat, action: A, view: V },
    Refused { client_seq: u32, reason: String },
    SeatJoined { seat: Seat, name: String },
    SeatLeft { seat: Seat },
}

/// What happened on the host during a [`TurnHost::poll`].
#[derive(Clone, Debug, PartialEq)]
pub enum HostEvent<A> {
    Joined { seat: Seat, name: String, reconnect: bool },
    Left { seat: Seat },
    Applied { seat: Seat, action: A },
    Refused { seat: Seat, reason: String },
}

#[derive(Clone, Debug, PartialEq)]
enum SeatState {
    Open,
    Local(String),
    Remote { name: String, token: u64, peer: Option<usize> },
}

struct Peer<L> {
    link: L,
    seat: Option<Seat>,
}

/// The authoritative side of a turn-based session.
pub struct TurnHost<R: TurnRules, L: Link> {
    rules: R,
    game_id: String,
    seats: Vec<SeatState>,
    peers: Vec<Option<Peer<L>>>,
    version: u64,
    token_seed: u64,
}

impl<R: TurnRules, L: Link> TurnHost<R, L> {
    /// `game_id` must match the clients' (e.g. "parabola/0.3") so builds
    /// with different rules refuse each other.
    pub fn new(rules: R, game_id: impl Into<String>) -> Self {
        let seats = vec![SeatState::Open; rules.seats()];
        let token_seed = {
            use std::hash::{BuildHasher, Hasher};
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
            h.finish()
        };
        Self { rules, game_id: game_id.into(), seats, peers: Vec::new(), version: 0, token_seed }
    }

    pub fn rules(&self) -> &R {
        &self.rules
    }

    /// Mutable access for setup before play starts. Changes made here are
    /// not broadcast; call [`Self::resync`] afterwards.
    pub fn rules_mut(&mut self) -> &mut R {
        &mut self.rules
    }

    /// Increments with every applied action.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Reserves `seat` for a player on this machine (hot-seat or bot).
    pub fn claim_local(&mut self, seat: Seat, name: impl Into<String>) -> Result<(), String> {
        match self.seats.get_mut(seat as usize) {
            Some(s @ SeatState::Open) => {
                *s = SeatState::Local(name.into());
                Ok(())
            }
            Some(_) => Err(format!("seat {seat} is taken")),
            None => Err(format!("no seat {seat}")),
        }
    }

    /// Seat names (`None` = open).
    pub fn names(&self) -> Vec<Option<String>> {
        self.seats
            .iter()
            .map(|s| match s {
                SeatState::Open => None,
                SeatState::Local(n) | SeatState::Remote { name: n, .. } => Some(n.clone()),
            })
            .collect()
    }

    /// True when the seat is held by a remote player who is currently connected.
    pub fn is_connected(&self, seat: Seat) -> bool {
        matches!(self.seats.get(seat as usize), Some(SeatState::Remote { peer: Some(_), .. }))
    }

    /// True when every seat is taken (by a local or remote player).
    pub fn is_full(&self) -> bool {
        self.seats.iter().all(|s| *s != SeatState::Open)
    }

    /// Adds a new connection; it gets a seat once its `Hello` arrives.
    pub fn add_peer(&mut self, link: L) {
        let peer = Peer { link, seat: None };
        match self.peers.iter().position(Option::is_none) {
            Some(i) => self.peers[i] = Some(peer),
            None => self.peers.push(Some(peer)),
        }
    }

    /// Applies an action for a local seat. Broadcasts it like a remote one.
    pub fn submit_local(&mut self, seat: Seat, action: R::Action) -> Result<(), String> {
        if !matches!(self.seats.get(seat as usize), Some(SeatState::Local(_))) {
            return Err(format!("seat {seat} is not local"));
        }
        self.rules.check(seat, &action)?;
        self.commit(seat, action);
        Ok(())
    }

    /// Sends every connected seat its current view (after `rules_mut` edits).
    pub fn resync(&mut self) {
        for i in 0..self.peers.len() {
            let Some(seat) = self.peers[i].as_ref().and_then(|p| p.seat) else { continue };
            let names = self.names();
            let token = self.token_of(seat);
            let msg: ServerMsg<R::Action, R::View> = ServerMsg::Welcome {
                seat,
                token,
                version: self.version,
                view: self.rules.view_for(seat),
                names,
            };
            self.send_to(i, &msg);
        }
    }

    /// Pumps every link once. Call each frame.
    pub fn poll(&mut self) -> Vec<HostEvent<R::Action>> {
        let mut events = Vec::new();
        for i in 0..self.peers.len() {
            let Some(peer) = self.peers[i].as_mut() else { continue };
            let inbound = peer.link.recv_raw();
            for bytes in inbound {
                match bincode::deserialize::<ClientMsg<R::Action>>(&bytes) {
                    Ok(msg) => self.handle(i, msg, &mut events),
                    Err(e) => log::warn!("turn host: undecodable message dropped: {e}"),
                }
                if self.peers[i].is_none() {
                    break;
                }
            }
            if self.peers[i].as_ref().is_some_and(|p| p.link.is_closed()) {
                self.drop_peer(i, &mut events);
            }
        }
        events
    }

    fn handle(&mut self, i: usize, msg: ClientMsg<R::Action>, events: &mut Vec<HostEvent<R::Action>>) {
        let seat = self.peers[i].as_ref().and_then(|p| p.seat);
        match (msg, seat) {
            (ClientMsg::Hello { protocol, game_id, name, token }, None) => {
                if protocol != TURN_PROTOCOL_VERSION {
                    return self.reject(i, format!("protocol {protocol}, host speaks {TURN_PROTOCOL_VERSION}"));
                }
                if game_id != self.game_id {
                    return self.reject(i, format!("game {game_id:?}, host runs {:?}", self.game_id));
                }
                let reclaim = token.and_then(|t| {
                    self.seats.iter().position(
                        |s| matches!(s, SeatState::Remote { token, peer: None, .. } if *token == t),
                    )
                });
                let (seat, reconnect) = match reclaim {
                    Some(s) => (s, true),
                    None => match self.seats.iter().position(|s| *s == SeatState::Open) {
                        Some(s) => (s, false),
                        None => return self.reject(i, "the table is full".into()),
                    },
                };
                let seat = seat as Seat;
                let token = match &self.seats[seat as usize] {
                    SeatState::Remote { token, .. } => *token,
                    _ => self.new_token(seat),
                };
                let name = if reconnect {
                    match &self.seats[seat as usize] {
                        SeatState::Remote { name, .. } => name.clone(),
                        _ => name,
                    }
                } else {
                    name
                };
                self.seats[seat as usize] = SeatState::Remote { name: name.clone(), token, peer: Some(i) };
                if let Some(p) = self.peers[i].as_mut() {
                    p.seat = Some(seat);
                }
                let welcome: ServerMsg<R::Action, R::View> = ServerMsg::Welcome {
                    seat,
                    token,
                    version: self.version,
                    view: self.rules.view_for(seat),
                    names: self.names(),
                };
                self.send_to(i, &welcome);
                self.broadcast_except(i, &ServerMsg::<R::Action, R::View>::SeatJoined { seat, name: name.clone() });
                events.push(HostEvent::Joined { seat, name, reconnect });
            }
            (ClientMsg::Hello { .. }, Some(_)) => {}
            (ClientMsg::Action { .. }, None) => self.reject(i, "say Hello first".into()),
            (ClientMsg::Action { client_seq, action }, Some(seat)) => {
                match self.rules.check(seat, &action) {
                    Ok(()) => {
                        self.commit(seat, action.clone());
                        events.push(HostEvent::Applied { seat, action });
                    }
                    Err(reason) => {
                        self.send_to(
                            i,
                            &ServerMsg::<R::Action, R::View>::Refused { client_seq, reason: reason.clone() },
                        );
                        events.push(HostEvent::Refused { seat, reason });
                    }
                }
            }
            (ClientMsg::Bye, _) => self.drop_peer(i, events),
        }
    }

    fn commit(&mut self, seat: Seat, action: R::Action) {
        self.rules.apply(seat, &action);
        self.version += 1;
        for i in 0..self.peers.len() {
            let Some(to) = self.peers[i].as_ref().and_then(|p| p.seat) else { continue };
            let msg: ServerMsg<R::Action, R::View> = ServerMsg::Update {
                version: self.version,
                by: seat,
                action: action.clone(),
                view: self.rules.view_for(to),
            };
            self.send_to(i, &msg);
        }
    }

    fn reject(&mut self, i: usize, reason: String) {
        log::info!("turn host: rejecting peer: {reason}");
        self.send_to(i, &ServerMsg::<R::Action, R::View>::Reject { reason });
        self.peers[i] = None;
    }

    fn drop_peer(&mut self, i: usize, events: &mut Vec<HostEvent<R::Action>>) {
        let Some(peer) = self.peers[i].take() else { return };
        if let Some(seat) = peer.seat {
            if let SeatState::Remote { peer, .. } = &mut self.seats[seat as usize] {
                *peer = None;
            }
            self.broadcast_except(usize::MAX, &ServerMsg::<R::Action, R::View>::SeatLeft { seat });
            events.push(HostEvent::Left { seat });
        }
    }

    fn send_to(&mut self, i: usize, msg: &ServerMsg<R::Action, R::View>) {
        if let (Some(p), Ok(bytes)) = (self.peers[i].as_mut(), bincode::serialize(msg)) {
            p.link.send_raw(bytes);
        }
    }

    fn broadcast_except(&mut self, skip: usize, msg: &ServerMsg<R::Action, R::View>) {
        let Ok(bytes) = bincode::serialize(msg) else { return };
        for (i, p) in self.peers.iter_mut().enumerate() {
            if let Some(p) = p.as_mut().filter(|p| i != skip && p.seat.is_some()) {
                p.link.send_raw(bytes.clone());
            }
        }
    }

    fn token_of(&self, seat: Seat) -> u64 {
        match &self.seats[seat as usize] {
            SeatState::Remote { token, .. } => *token,
            _ => 0,
        }
    }

    fn new_token(&mut self, seat: Seat) -> u64 {
        // splitmix64 step: unpredictable enough for a LAN reconnect token.
        self.token_seed = self.token_seed.wrapping_add(0x9E37_79B9_7F4A_7C15 ^ seat as u64);
        let mut z = self.token_seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)).max(1)
    }
}

/// What a [`TurnClient`] learned during a [`TurnClient::poll`].
#[derive(Clone, Debug, PartialEq)]
pub enum ClientEvent<A> {
    /// Seated (or re-seated); the view is now current.
    Welcomed { seat: Seat, token: u64 },
    /// Turned away; the session is over.
    Rejected { reason: String },
    /// An action was applied (by anyone); the view is now current.
    Applied { by: Seat, action: A, version: u64 },
    /// One of our actions was refused by the host's rules.
    Refused { client_seq: u32, reason: String },
    SeatJoined { seat: Seat, name: String },
    SeatLeft { seat: Seat },
    Disconnected,
}

/// A remote player's side of a turn-based session.
pub struct TurnClient<A, V, L: Link> {
    link: L,
    seat: Option<Seat>,
    token: Option<u64>,
    view: Option<V>,
    version: u64,
    next_seq: u32,
    names: Vec<Option<String>>,
    closed: bool,
    _action: std::marker::PhantomData<A>,
}

impl<A, V, L> TurnClient<A, V, L>
where
    A: Serialize + DeserializeOwned + Clone,
    V: Serialize + DeserializeOwned + Clone,
    L: Link,
{
    /// Sends `Hello`. Pass the `token` from an earlier `Welcomed` to take
    /// back the same seat after a disconnect.
    pub fn join(mut link: L, game_id: &str, name: &str, token: Option<u64>) -> Self {
        let hello: ClientMsg<A> = ClientMsg::Hello {
            protocol: TURN_PROTOCOL_VERSION,
            game_id: game_id.into(),
            name: name.into(),
            token,
        };
        if let Ok(b) = bincode::serialize(&hello) {
            link.send_raw(b);
        }
        Self {
            link,
            seat: None,
            token,
            view: None,
            version: 0,
            next_seq: 0,
            names: Vec::new(),
            closed: false,
            _action: std::marker::PhantomData,
        }
    }

    pub fn seat(&self) -> Option<Seat> {
        self.seat
    }
    pub fn token(&self) -> Option<u64> {
        self.token
    }
    pub fn view(&self) -> Option<&V> {
        self.view.as_ref()
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    pub fn names(&self) -> &[Option<String>] {
        &self.names
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Queues an action for the host; returns its sequence number (echoed
    /// in `Refused`). The view only changes when the host's `Update` arrives.
    pub fn submit(&mut self, action: A) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if let Ok(b) = bincode::serialize(&ClientMsg::Action { client_seq: seq, action }) {
            self.link.send_raw(b);
        }
        seq
    }

    /// Says goodbye; the seat stays reserved for a reconnect.
    pub fn leave(&mut self) {
        if let Ok(b) = bincode::serialize(&ClientMsg::<A>::Bye) {
            self.link.send_raw(b);
        }
        self.closed = true;
    }

    /// Pumps the link once. Call each frame.
    pub fn poll(&mut self) -> Vec<ClientEvent<A>> {
        let mut events = Vec::new();
        if self.closed {
            return events;
        }
        for bytes in self.link.recv_raw() {
            let msg = match bincode::deserialize::<ServerMsg<A, V>>(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("turn client: undecodable message dropped: {e}");
                    continue;
                }
            };
            match msg {
                ServerMsg::Welcome { seat, token, version, view, names } => {
                    self.seat = Some(seat);
                    self.token = Some(token);
                    self.version = version;
                    self.view = Some(view);
                    self.names = names;
                    events.push(ClientEvent::Welcomed { seat, token });
                }
                ServerMsg::Reject { reason } => {
                    self.closed = true;
                    events.push(ClientEvent::Rejected { reason });
                }
                ServerMsg::Update { version, by, action, view } => {
                    self.version = version;
                    self.view = Some(view);
                    events.push(ClientEvent::Applied { by, action, version });
                }
                ServerMsg::Refused { client_seq, reason } => {
                    events.push(ClientEvent::Refused { client_seq, reason })
                }
                ServerMsg::SeatJoined { seat, name } => {
                    if let Some(slot) = self.names.get_mut(seat as usize) {
                        *slot = Some(name.clone());
                    }
                    events.push(ClientEvent::SeatJoined { seat, name });
                }
                ServerMsg::SeatLeft { seat } => events.push(ClientEvent::SeatLeft { seat }),
            }
        }
        if self.link.is_closed() && !self.closed {
            self.closed = true;
            events.push(ClientEvent::Disconnected);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each seat holds a secret card; on your turn you add 1..=3 to a shared
    /// counter. Only you see your own card.
    #[derive(Clone)]
    struct Counter {
        total: u32,
        turn: Seat,
        cards: Vec<u32>,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Add(u32);

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct View {
        total: u32,
        turn: Seat,
        my_card: u32,
        others_cards: Vec<Option<u32>>,
    }

    impl TurnRules for Counter {
        type Action = Add;
        type View = View;
        fn seats(&self) -> usize {
            self.cards.len()
        }
        fn check(&self, seat: Seat, a: &Add) -> Result<(), String> {
            if seat != self.turn {
                return Err("not your turn".into());
            }
            if !(1..=3).contains(&a.0) {
                return Err("add 1 to 3".into());
            }
            Ok(())
        }
        fn apply(&mut self, _seat: Seat, a: &Add) {
            self.total += a.0;
            self.turn = (self.turn + 1) % self.cards.len() as Seat;
        }
        fn view_for(&self, seat: Seat) -> View {
            View {
                total: self.total,
                turn: self.turn,
                my_card: self.cards[seat as usize],
                others_cards: (0..self.cards.len()).map(|_| None).collect(),
            }
        }
    }

    type Host = TurnHost<Counter, LoopbackLink>;
    type Client = TurnClient<Add, View, LoopbackLink>;

    fn table(seats: usize) -> Host {
        TurnHost::new(Counter { total: 0, turn: 0, cards: (0..seats as u32).map(|i| 100 + i).collect() }, "test/1")
    }

    fn join(host: &mut Host, name: &str, token: Option<u64>) -> (Client, LoopbackLink) {
        let (a, b) = loopback();
        let keep = LoopbackLink { inbox: a.inbox.clone(), outbox: a.outbox.clone(), closed: a.closed.clone() };
        host.add_peer(a);
        (TurnClient::join(b, "test/1", name, token), keep)
    }

    #[test]
    fn players_are_seated_in_order_and_see_only_their_own_card() {
        let mut host = table(2);
        let (mut alice, _) = join(&mut host, "alice", None);
        let (mut bob, _) = join(&mut host, "bob", None);
        let ev = host.poll();
        assert!(matches!(&ev[0], HostEvent::Joined { seat: 0, name, reconnect: false } if name == "alice"));
        assert!(matches!(&ev[1], HostEvent::Joined { seat: 1, .. }));
        alice.poll();
        bob.poll();
        assert_eq!(alice.seat(), Some(0));
        assert_eq!(bob.seat(), Some(1));
        assert_eq!(alice.view().unwrap().my_card, 100);
        assert_eq!(bob.view().unwrap().my_card, 101);
        assert!(host.is_full());
    }

    #[test]
    fn legal_actions_reach_everyone_and_illegal_ones_only_the_sender() {
        let mut host = table(2);
        let (mut alice, _) = join(&mut host, "alice", None);
        let (mut bob, _) = join(&mut host, "bob", None);
        host.poll();
        alice.poll();
        bob.poll();

        let seq = bob.submit(Add(2)); // not bob's turn
        assert_eq!(host.poll(), vec![HostEvent::Refused { seat: 1, reason: "not your turn".into() }]);
        alice.submit(Add(3));
        assert_eq!(host.poll(), vec![HostEvent::Applied { seat: 0, action: Add(3) }]);

        let a = alice.poll();
        assert_eq!(a, vec![ClientEvent::Applied { by: 0, action: Add(3), version: 1 }]);
        let b = bob.poll();
        assert!(b.contains(&ClientEvent::Refused { client_seq: seq, reason: "not your turn".into() }));
        assert_eq!(bob.view().unwrap().total, 3);
        assert_eq!(bob.view().unwrap().my_card, 101);
        assert_eq!(host.version(), 1);
    }

    #[test]
    fn local_seats_play_through_the_host_and_are_broadcast() {
        let mut host = table(2);
        host.claim_local(0, "bot").unwrap();
        let (mut remote, _) = join(&mut host, "remote", None);
        host.poll();
        remote.poll();
        assert_eq!(remote.seat(), Some(1));
        assert_eq!(host.submit_local(1, Add(1)), Err("seat 1 is not local".into()));
        assert_eq!(host.submit_local(0, Add(9)), Err("add 1 to 3".into()));
        host.submit_local(0, Add(2)).unwrap();
        assert_eq!(remote.poll(), vec![ClientEvent::Applied { by: 0, action: Add(2), version: 1 }]);
        assert_eq!(remote.names(), &[Some("bot".to_string()), Some("remote".to_string())]);
    }

    #[test]
    fn a_dropped_player_reclaims_their_seat_with_the_token() {
        let mut host = table(2);
        let (mut alice, alice_link) = join(&mut host, "alice", None);
        host.poll();
        alice.poll();
        let token = alice.token().unwrap();

        alice_link.close();
        assert_eq!(host.poll(), vec![HostEvent::Left { seat: 0 }]);
        assert!(!host.is_connected(0));

        // A stranger takes the open seat, not alice's.
        let (mut eve, _) = join(&mut host, "eve", Some(12345));
        host.poll();
        eve.poll();
        assert_eq!(eve.seat(), Some(1));

        let (mut back, _) = join(&mut host, "alice-again", Some(token));
        let ev = host.poll();
        assert!(matches!(&ev[0], HostEvent::Joined { seat: 0, name, reconnect: true } if name == "alice"));
        back.poll();
        assert_eq!(back.seat(), Some(0));
        assert_eq!(back.view().unwrap().my_card, 100);
    }

    #[test]
    fn wrong_game_wrong_protocol_and_full_table_are_rejected() {
        let mut host = table(1);
        let (a, b) = loopback();
        host.add_peer(a);
        let mut wrong: Client = TurnClient::join(b, "other/9", "x", None);
        host.poll();
        assert!(matches!(&wrong.poll()[0], ClientEvent::Rejected { .. }));
        assert!(wrong.is_closed());

        let (mut ok, _) = join(&mut host, "ok", None);
        let (mut late, _) = join(&mut host, "late", None);
        host.poll();
        ok.poll();
        assert_eq!(late.poll(), vec![ClientEvent::Rejected { reason: "the table is full".into() }]);
    }

    #[test]
    fn works_over_real_tcp() {
        let acceptor = TcpAcceptor::bind(0).unwrap();
        let port = acceptor.local_port().unwrap();
        let mut host: TurnHost<Counter, NetConnection> =
            TurnHost::new(Counter { total: 0, turn: 0, cards: vec![7, 8] }, "tcp/1");
        host.claim_local(0, "host").unwrap();
        let mut client: TurnClient<Add, View, NetConnection> =
            TurnClient::join(connect_tcp(&format!("127.0.0.1:{port}")).unwrap(), "tcp/1", "guest", None);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seated = false;
        let mut applied = false;
        while std::time::Instant::now() < deadline && !applied {
            for c in acceptor.accept_new() {
                host.add_peer(c);
            }
            host.poll();
            for e in client.poll() {
                match e {
                    ClientEvent::Welcomed { seat: 1, .. } => {
                        seated = true;
                        host.submit_local(0, Add(3)).unwrap();
                    }
                    ClientEvent::Applied { by: 0, action: Add(3), .. } => applied = true,
                    _ => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(seated && applied);
        assert_eq!(client.view().unwrap(), &View { total: 3, turn: 1, my_card: 8, others_cards: vec![None, None] });
    }
}
