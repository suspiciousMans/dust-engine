//! Turn-based multiplayer: a host-authoritative action log, independent of
//! transport and of the platform (no sockets here, so it also builds for the
//! web).
//!
//! The game supplies a [`TurnGame`]: a `State`, an `Action`, and two pure
//! functions, `validate` and `apply`. The host owns the truth. A client
//! sends an action; the host validates it against the current state, applies
//! it, stamps it with the next sequence number and broadcasts it. Clients
//! apply broadcast actions in order, so every peer replays the same log and
//! stays identical without ever sending the whole state (except in `Welcome`
//! and `Resync`). There is no client-side prediction: a board game can wait
//! one round trip.
//!
//! Moving bytes is left to a [`TurnLink`]. `net::NetConnection` (TCP) is one;
//! [`MemoryLink`] is an in-process pair for tests and hot-seat/AI; a
//! WebSocket link for the web port only has to implement the same two methods.
//!
//! Determinism rule: `apply` must be a pure function of `(state, player,
//! action)`. Randomness belongs in the action (the host rolls dice and puts
//! the result in the action it broadcasts, via [`TurnHost::submit_with`]).

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

pub const TURN_PROTOCOL_VERSION: u32 = 1;
pub const MAX_TURN_PLAYERS: usize = 6;

pub trait TurnGame {
    type State: Clone + Serialize + DeserializeOwned;
    type Action: Clone + Serialize + DeserializeOwned;

    /// Is `action` legal for `player` right now (including "is it their turn")?
    fn validate(state: &Self::State, player: u8, action: &Self::Action) -> Result<(), String>;
    /// Apply a validated action. Must be deterministic.
    fn apply(state: &mut Self::State, player: u8, action: &Self::Action);
}

/// Byte mover for one peer. Never blocks.
pub trait TurnLink {
    fn send<T: Serialize>(&mut self, msg: &T);
    /// Messages received since the last call (also flushes pending writes).
    fn pump<T: DeserializeOwned>(&mut self) -> Vec<T>;
    fn is_closed(&self) -> bool;
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ClientMsg<A> {
    Hello { version: u32, name: String },
    Action(A),
    /// Ask for a fresh copy of the state (after a detected mismatch).
    ResyncRequest,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ServerMsg<A, S> {
    Welcome { you: u8, names: Vec<String>, seq: u64, state: S },
    Reject { reason: String },
    Applied { seq: u64, player: u8, action: A },
    Refused { reason: String },
    PlayerJoined { player: u8, name: String },
    PlayerLeft { player: u8 },
    Resync { seq: u64, state: S },
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnEvent<A> {
    /// An action was applied to the local state (including your own).
    Applied { seq: u64, player: u8, action: A },
    /// The host refused an action you submitted.
    Refused(String),
    PlayerJoined { player: u8, name: String },
    PlayerLeft { player: u8 },
    /// Client only: state was replaced wholesale.
    Resynced,
    /// Client only: connection lost.
    Disconnected,
}

// ---------------------------------------------------------------- host

struct Peer<L> {
    link: L,
    player: Option<u8>,
}

pub struct TurnHost<G: TurnGame, L: TurnLink> {
    pub state: G::State,
    seq: u64,
    names: Vec<Option<String>>, // index = player id; 0 is the host
    peers: Vec<Peer<L>>,
    max_players: usize,
}

impl<G: TurnGame, L: TurnLink> TurnHost<G, L> {
    pub fn new(state: G::State, host_name: &str, max_players: usize) -> Self {
        Self {
            state,
            seq: 0,
            names: vec![Some(host_name.to_string())],
            peers: Vec::new(),
            max_players: max_players.clamp(2, MAX_TURN_PLAYERS),
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Player ids currently connected (host is always 0).
    pub fn players(&self) -> Vec<u8> {
        self.names.iter().enumerate().filter(|(_, n)| n.is_some()).map(|(i, _)| i as u8).collect()
    }

    /// Hand over a freshly accepted connection. It joins after its `Hello`.
    pub fn add_link(&mut self, link: L) {
        self.peers.push(Peer { link, player: None });
    }

    /// Submit the host's own action.
    pub fn submit(&mut self, action: G::Action) -> Result<TurnEvent<G::Action>, String> {
        self.accept(0, action)
    }

    /// Like `submit`, but builds the action from the current state so the
    /// host can roll randomness into it.
    pub fn submit_with(
        &mut self,
        make: impl FnOnce(&G::State) -> G::Action,
    ) -> Result<TurnEvent<G::Action>, String> {
        let a = make(&self.state);
        self.accept(0, a)
    }

    fn accept(&mut self, player: u8, action: G::Action) -> Result<TurnEvent<G::Action>, String> {
        G::validate(&self.state, player, &action)?;
        G::apply(&mut self.state, player, &action);
        self.seq += 1;
        let msg = ServerMsg::<G::Action, G::State>::Applied { seq: self.seq, player, action: action.clone() };
        self.broadcast(&msg);
        Ok(TurnEvent::Applied { seq: self.seq, player, action })
    }

    fn broadcast(&mut self, msg: &ServerMsg<G::Action, G::State>) {
        for p in self.peers.iter_mut().filter(|p| p.player.is_some()) {
            p.link.send(msg);
        }
    }

    /// Drive the session once per frame. Returns events for remote actions
    /// applied and for joins/leaves; host-submitted actions are returned by
    /// `submit` directly.
    pub fn poll(&mut self) -> Vec<TurnEvent<G::Action>> {
        let mut events = Vec::new();
        let mut i = 0;
        while i < self.peers.len() {
            let msgs: Vec<ClientMsg<G::Action>> = self.peers[i].link.pump();
            let mut drop_peer = self.peers[i].link.is_closed();
            for m in msgs {
                match (m, self.peers[i].player) {
                    (ClientMsg::Hello { version, name }, None) => {
                        if version != TURN_PROTOCOL_VERSION {
                            self.peers[i].link.send(&ServerMsg::<G::Action, G::State>::Reject {
                                reason: format!("protocol {version} != {TURN_PROTOCOL_VERSION}"),
                            });
                            drop_peer = true;
                        } else if let Some(slot) = self.free_slot() {
                            let name = name.chars().take(24).collect::<String>();
                            self.names[slot] = Some(name.clone());
                            self.peers[i].player = Some(slot as u8);
                            let names = self.names.iter().map(|n| n.clone().unwrap_or_default()).collect();
                            let welcome = ServerMsg::<G::Action, G::State>::Welcome {
                                you: slot as u8,
                                names,
                                seq: self.seq,
                                state: self.state.clone(),
                            };
                            self.peers[i].link.send(&welcome);
                            for (j, p) in self.peers.iter_mut().enumerate() {
                                if j != i && p.player.is_some() {
                                    p.link.send(&ServerMsg::<G::Action, G::State>::PlayerJoined {
                                        player: slot as u8,
                                        name: name.clone(),
                                    });
                                }
                            }
                            events.push(TurnEvent::PlayerJoined { player: slot as u8, name });
                        } else {
                            self.peers[i].link.send(&ServerMsg::<G::Action, G::State>::Reject {
                                reason: "game is full".into(),
                            });
                            drop_peer = true;
                        }
                    }
                    (ClientMsg::Action(a), Some(pl)) => match self.accept(pl, a) {
                        Ok(ev) => events.push(ev),
                        Err(reason) => {
                            self.peers[i].link.send(&ServerMsg::<G::Action, G::State>::Refused { reason })
                        }
                    },
                    (ClientMsg::ResyncRequest, Some(_)) => {
                        let m = ServerMsg::<G::Action, G::State>::Resync { seq: self.seq, state: self.state.clone() };
                        self.peers[i].link.send(&m);
                    }
                    _ => {} // out-of-order message from an unjoined peer: ignore
                }
            }
            if drop_peer {
                let p = self.peers.remove(i);
                if let Some(pl) = p.player {
                    self.names[pl as usize] = None;
                    self.broadcast(&ServerMsg::PlayerLeft { player: pl });
                    events.push(TurnEvent::PlayerLeft { player: pl });
                }
            } else {
                i += 1;
            }
        }
        events
    }

    fn free_slot(&mut self) -> Option<usize> {
        if let Some(i) = self.names.iter().position(|n| n.is_none()) {
            return Some(i);
        }
        if self.names.len() < self.max_players {
            self.names.push(None);
            return Some(self.names.len() - 1);
        }
        None
    }
}

// -------------------------------------------------------------- client

pub struct TurnClient<G: TurnGame, L: TurnLink> {
    link: L,
    pub state: Option<G::State>,
    pub me: Option<u8>,
    pub names: Vec<String>,
    seq: u64,
    rejected: Option<String>,
    gone: bool,
}

impl<G: TurnGame, L: TurnLink> TurnClient<G, L> {
    pub fn connect(mut link: L, name: &str) -> Self {
        link.send(&ClientMsg::<G::Action>::Hello { version: TURN_PROTOCOL_VERSION, name: name.to_string() });
        Self { link, state: None, me: None, names: Vec::new(), seq: 0, rejected: None, gone: false }
    }

    pub fn is_joined(&self) -> bool {
        self.state.is_some()
    }
    pub fn rejected(&self) -> Option<&str> {
        self.rejected.as_deref()
    }
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Ask the host to apply an action. The state changes when the host's
    /// `Applied` comes back through `poll`.
    pub fn submit(&mut self, action: G::Action) {
        self.link.send(&ClientMsg::Action(action));
    }

    pub fn poll(&mut self) -> Vec<TurnEvent<G::Action>> {
        let mut events = Vec::new();
        let msgs: Vec<ServerMsg<G::Action, G::State>> = self.link.pump();
        for m in msgs {
            match m {
                ServerMsg::Welcome { you, names, seq, state } => {
                    self.me = Some(you);
                    self.names = names;
                    self.seq = seq;
                    self.state = Some(state);
                    events.push(TurnEvent::Resynced);
                }
                ServerMsg::Reject { reason } => self.rejected = Some(reason),
                ServerMsg::Applied { seq, player, action } => {
                    let Some(state) = self.state.as_mut() else { continue };
                    if seq != self.seq + 1 {
                        // Missed or duplicated an action: ask for the truth.
                        self.link.send(&ClientMsg::<G::Action>::ResyncRequest);
                        continue;
                    }
                    G::apply(state, player, &action);
                    self.seq = seq;
                    events.push(TurnEvent::Applied { seq, player, action });
                }
                ServerMsg::Refused { reason } => events.push(TurnEvent::Refused(reason)),
                ServerMsg::PlayerJoined { player, name } => {
                    let i = player as usize;
                    if self.names.len() <= i {
                        self.names.resize(i + 1, String::new());
                    }
                    self.names[i] = name.clone();
                    events.push(TurnEvent::PlayerJoined { player, name });
                }
                ServerMsg::PlayerLeft { player } => events.push(TurnEvent::PlayerLeft { player }),
                ServerMsg::Resync { seq, state } => {
                    self.seq = seq;
                    self.state = Some(state);
                    events.push(TurnEvent::Resynced);
                }
            }
        }
        if self.link.is_closed() && !self.gone {
            self.gone = true;
            events.push(TurnEvent::Disconnected);
        }
        events
    }
}

// ---------------------------------------------------- in-memory link

#[derive(Default)]
struct Pipe {
    q: VecDeque<Vec<u8>>,
    closed: bool,
}

/// One end of an in-process connection. Create both ends with
/// [`MemoryLink::pair`].
pub struct MemoryLink {
    tx: Rc<RefCell<Pipe>>,
    rx: Rc<RefCell<Pipe>>,
}

impl MemoryLink {
    pub fn pair() -> (MemoryLink, MemoryLink) {
        let a = Rc::new(RefCell::new(Pipe::default()));
        let b = Rc::new(RefCell::new(Pipe::default()));
        (MemoryLink { tx: a.clone(), rx: b.clone() }, MemoryLink { tx: b, rx: a })
    }
    pub fn close(&self) {
        self.tx.borrow_mut().closed = true;
    }
}

impl TurnLink for MemoryLink {
    fn send<T: Serialize>(&mut self, msg: &T) {
        if let Ok(b) = bincode::serialize(msg) {
            self.tx.borrow_mut().q.push_back(b);
        }
    }
    fn pump<T: DeserializeOwned>(&mut self) -> Vec<T> {
        let mut rx = self.rx.borrow_mut();
        let out = rx.q.drain(..).filter_map(|b| bincode::deserialize(&b).ok()).collect();
        out
    }
    fn is_closed(&self) -> bool {
        self.rx.borrow().closed && self.rx.borrow().q.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two players alternately add to a counter; first to reach 10 wins.
    struct Counter;
    #[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
    struct S {
        total: u32,
        turn: u8,
        players: u8,
    }
    #[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
    struct Add(u32);
    impl TurnGame for Counter {
        type State = S;
        type Action = Add;
        fn validate(s: &S, p: u8, a: &Add) -> Result<(), String> {
            if p != s.turn {
                return Err("not your turn".into());
            }
            if !(1..=3).contains(&a.0) {
                return Err("add 1 to 3".into());
            }
            Ok(())
        }
        fn apply(s: &mut S, _p: u8, a: &Add) {
            s.total += a.0;
            s.turn = (s.turn + 1) % s.players;
        }
    }

    fn setup() -> (TurnHost<Counter, MemoryLink>, TurnClient<Counter, MemoryLink>) {
        let mut host = TurnHost::<Counter, _>::new(S { total: 0, turn: 0, players: 2 }, "host", 2);
        let (a, b) = MemoryLink::pair();
        host.add_link(a);
        let mut client = TurnClient::<Counter, _>::connect(b, "guest");
        host.poll();
        client.poll();
        (host, client)
    }

    #[test]
    fn join_and_replay() {
        let (mut host, mut client) = setup();
        assert_eq!(client.me, Some(1));
        assert_eq!(client.state, Some(host.state.clone()));
        host.submit(Add(2)).unwrap();
        client.poll();
        client.submit(Add(3));
        let ev = host.poll();
        assert_eq!(ev, vec![TurnEvent::Applied { seq: 2, player: 1, action: Add(3) }]);
        client.poll();
        assert_eq!(client.state.as_ref().unwrap(), &host.state);
        assert_eq!(host.state.total, 5);
    }

    #[test]
    fn illegal_actions_refused() {
        let (mut host, mut client) = setup();
        client.submit(Add(1)); // host's turn
        host.poll();
        assert_eq!(client.poll(), vec![TurnEvent::Refused("not your turn".into())]);
        assert_eq!(host.state.total, 0);
        assert!(host.submit(Add(9)).is_err());
    }

    #[test]
    fn full_game_is_rejected() {
        let (mut host, _c) = setup();
        let (a, b) = MemoryLink::pair();
        host.add_link(a);
        let mut late = TurnClient::<Counter, _>::connect(b, "late");
        host.poll();
        late.poll();
        assert_eq!(late.rejected(), Some("game is full"));
        assert!(!late.is_joined());
    }

    #[test]
    fn version_mismatch_rejected() {
        let mut host = TurnHost::<Counter, _>::new(S { total: 0, turn: 0, players: 2 }, "h", 2);
        let (a, mut b) = MemoryLink::pair();
        host.add_link(a);
        b.send(&ClientMsg::<Add>::Hello { version: 99, name: "x".into() });
        host.poll();
        let r: Vec<ServerMsg<Add, S>> = b.pump();
        assert!(matches!(r[0], ServerMsg::Reject { .. }));
    }

    #[test]
    fn gap_triggers_resync() {
        let (mut host, mut client) = setup();
        host.submit(Add(1)).unwrap();
        // Simulate a lost message by discarding it client-side.
        let _: Vec<ServerMsg<Add, S>> = client.link.pump();
        client.submit(Add(1));
        host.poll(); // applies seq 2, broadcasts it
        client.poll(); // sees seq 2 with seq 0 local: requests resync
        host.poll(); // answers
        let ev = client.poll();
        assert!(ev.contains(&TurnEvent::Resynced));
        assert_eq!(client.state.as_ref().unwrap(), &host.state);
        assert_eq!(client.seq(), host.seq());
    }

    #[test]
    fn leave_frees_slot() {
        let (mut host, client) = setup();
        client.link.close();
        let ev = host.poll();
        assert_eq!(ev, vec![TurnEvent::PlayerLeft { player: 1 }]);
        assert_eq!(host.players(), vec![0]);
        let (a, b) = MemoryLink::pair();
        host.add_link(a);
        let mut again = TurnClient::<Counter, _>::connect(b, "again");
        host.poll();
        again.poll();
        assert_eq!(again.me, Some(1));
    }

    #[cfg(not(target_os = "emscripten"))]
    #[test]
    fn works_over_tcp() {
        use crate::net::NetConnection;
        use std::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let cs = TcpStream::connect(addr).unwrap();
        let (ss, _) = l.accept().unwrap();
        let mut host = TurnHost::<Counter, NetConnection>::new(S { total: 0, turn: 0, players: 2 }, "h", 2);
        host.add_link(NetConnection::wrap(ss).unwrap());
        let mut client = TurnClient::<Counter, NetConnection>::connect(NetConnection::wrap(cs).unwrap(), "g");
        let t = std::time::Instant::now();
        while !client.is_joined() && t.elapsed().as_secs() < 5 {
            host.poll();
            client.poll();
        }
        assert!(client.is_joined());
        host.submit(Add(2)).unwrap();
        while client.seq() < 1 && t.elapsed().as_secs() < 5 {
            host.poll();
            client.poll();
        }
        assert_eq!(client.state.as_ref().unwrap(), &host.state);
    }
}
