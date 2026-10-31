//! LAN multiplayer architecture (Listen-Server + Headless Dedicated Server) for Warcraft Survivors.
//!
//! Uses a dual-socket protocol on UDP + TCP (default port `25565`) designed for ZeroTier / Tailscale
//! virtual LANs with zero external crate dependencies:
//! - **TCP (`25565`)**: Reliable ordered control stream (Lobby join/ready/class/map sync, Run Start,
//!   Simultaneous Level-Up Card Offers & Picks, Pause/Resume, Game Over, Revive notifications).
//! - **UDP (`25565`)**: Unreliable, sequenced 30 Hz high-frequency state snapshots (Player positions,
//!   velocities, HP/shields, Enemy positions/animations/HP, Gems, and Spell Visual FX triggers) and
//!   60 Hz client input packets (WASD direction, yaw, jump, mobility cast).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 25565;
pub const MAX_PLAYERS: usize = 4;
pub const SNAPSHOT_HZ: f32 = 30.0;
pub const INPUT_HZ: f32 = 60.0;
pub const REVIVE_RADIUS: f32 = 4.0;
pub const REVIVE_SECS: f32 = 3.0;

/// Unique player slot in a party (`0..MAX_PLAYERS`). `0` is the Host in Listen-Server mode.
pub type PlayerId = u8;

/// Current multiplayer role of this process.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum NetRole {
    #[default]
    Offline,
    /// Hosting an in-game Listen Server (player 0) or a Dedicated Server.
    Host,
    /// Connected to a remote Host as a Client (`player_id = 1..3`).
    Client,
}

/// Lobby state for one connected player.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LobbyPlayer {
    pub id: PlayerId,
    pub name: String,
    pub class: usize,
    pub display: u32,
    pub race: u8,
    pub sex: u8,
    pub ready: bool,
}

impl Default for LobbyPlayer {
    fn default() -> Self {
        Self {
            id: 0,
            name: "Player 1".into(),
            class: 0,
            display: 1300,
            race: 1,
            sex: 0,
            ready: false,
        }
    }
}

/// Reliable control messages sent over TCP (length-prefixed JSON).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReliableMsg {
    /// Client -> Server upon connecting.
    Hello {
        name: String,
        class: usize,
        display: u32,
        race: u8,
        sex: u8,
    },
    /// Server -> Client on accept, assigning their `PlayerId` and UDP token.
    Welcome {
        player_id: PlayerId,
        token: u64,
        map: usize,
        players: Vec<LobbyPlayer>,
    },
    /// Client -> Server: updated hero pick or ready status in the menu lobby.
    UpdateLobby {
        class: usize,
        display: u32,
        race: u8,
        sex: u8,
        ready: bool,
    },
    /// Server -> All Clients: full lobby state broadcast.
    LobbyState {
        map: usize,
        players: Vec<LobbyPlayer>,
    },
    /// Server -> All Clients: Host started the run on `map`.
    StartRun {
        map: usize,
        seed: u64,
        players: Vec<LobbyPlayer>,
    },
    /// Server -> All Clients: Simultaneous team level-up pause, with each player's 3 card offers.
    LevelUpOffer {
        level: u32,
        /// Per-player card choices serialized for UI display.
        offers: HashMap<PlayerId, Vec<NetCardChoice>>,
        picked: Vec<PlayerId>,
    },
    /// Client -> Server: Player picked card `index` (`0..3`).
    PickCard {
        index: usize,
    },
    /// Server -> All Clients: Update which players have locked in their level-up card.
    LevelUpProgress {
        picked: Vec<PlayerId>,
    },
    /// Server -> All Clients: All players picked (or no more pending levels); resume `Playing`.
    ResumeFromLevelUp {
        player_builds: HashMap<PlayerId, NetPlayerBuild>,
    },
    /// Client <-> Server: Toggle pause or return to menu.
    SetPhase {
        phase: NetPhase,
        won: bool,
    },
    /// Server -> All Clients: Center-screen banner announcement.
    Banner {
        text: String,
        secs: f32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetPhase {
    Menu,
    Playing,
    LevelUp,
    Paused,
    GameOver,
}

/// Serialized representation of a card offer for a client.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetCardChoice {
    pub kind: NetChoiceKind,
    pub key: String,
    pub next_level: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetChoiceKind {
    NewAbility,
    UpgradeAbility,
    NewPassive,
    UpgradePassive,
    Heal,
}

/// A player's owned abilities and passives for HUD synchronization.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NetPlayerBuild {
    /// `(ability_key, level)`
    pub abilities: Vec<(String, u8)>,
    /// `(passive_key, level)`
    pub passives: Vec<(String, u8)>,
}

/// High-frequency Client -> Server input packet sent over UDP (or TCP fallback).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ClientInputPacket {
    pub player_id: PlayerId,
    pub token: u64,
    pub seq: u32,
    /// World-space normalized XZ movement direction.
    pub move_dir: [f32; 2],
    /// Desired facing yaw (radians).
    pub yaw: f32,
    /// Aim cursor world position (if any) for mobility spells like Blink/Charge.
    pub aim_pos: Option<[f32; 3]>,
    /// Bit 0: jump pressed, Bit 1: mobility (F) pressed.
    pub buttons: u8,
}

pub const BTN_JUMP: u8 = 1 << 0;
pub const BTN_MOBILITY: u8 = 1 << 1;

/// High-frequency Server -> Client world snapshot sent over UDP at 30 Hz.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorldSnapshot {
    pub seq: u32,
    pub elapsed: f32,
    pub level: u32,
    pub xp: f32,
    pub kills: u32,
    pub bosses: u32,
    pub damage_done: f64,
    pub players: Vec<PlayerSnap>,
    pub enemies: Vec<EnemySnap>,
    pub gems: Vec<GemSnap>,
    pub areas: Vec<AreaSnap>,
    pub totems: Vec<TotemSnap>,
    pub fx_events: Vec<NetFxEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerSnap {
    pub id: PlayerId,
    pub pos: [f32; 3],
    pub yaw: f32,
    pub speed: f32,
    pub move_flags: u32,
    pub vertical_speed: f32,
    pub hp: f32,
    pub max_hp: f32,
    pub shield: f32,
    /// `Some(revive_progress_0_to_1)` when the player is downed and awaiting revive.
    pub downed: Option<f32>,
    pub buffs: Vec<u32>,
    pub ability_cds: Vec<f32>,
    pub mobility_cd: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnemySnap {
    pub net_id: u32,
    pub display: u32,
    pub scale: f32,
    pub level: u32,
    pub pos: [f32; 3],
    pub yaw: f32,
    pub speed: f32,
    pub hp: f32,
    pub max_hp: f32,
    pub boss: bool,
    pub dead: bool,
    pub debuffs: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GemSnap {
    pub net_id: u32,
    pub display: u32,
    pub scale: f32,
    pub pos: [f32; 3],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AreaSnap {
    pub net_id: u32,
    pub spell: u32,
    pub radius: f32,
    pub pos: [f32; 3],
    pub overhead: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TotemSnap {
    pub net_id: u32,
    pub display: u32,
    pub pos: [f32; 3],
}

/// Visual/audio spell events triggered since the last snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NetFxEvent {
    Cast {
        seq: u32,
        caster_is_player: bool,
        caster_id: u32,
        spell: u32,
        targets: Vec<u32>,
        dest: Option<[f32; 3]>,
    },
    Impact {
        seq: u32,
        target_is_player: bool,
        target_id: u32,
        spell: u32,
    },
    Swing {
        seq: u32,
        attacker_is_player: bool,
        attacker_id: u32,
        victim_is_player: bool,
        victim_id: u32,
        damage: f32,
    },
    CombatNumber {
        seq: u32,
        anchor_is_player: bool,
        anchor_id: u32,
        amount: f32,
        crit: bool,
        color: u32,
    },
}

// ── Binary Wire Framing for UDP & TCP ───────────────────────────────────────────────────────────

const MAGIC_UDP_INPUT: u8 = 0xA1;
const MAGIC_UDP_SNAP: u8 = 0xA2;

fn write_tcp_msg(stream: &mut TcpStream, msg: &ReliableMsg) -> io::Result<()> {
    let bytes = serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = (bytes.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn read_tcp_msg(stream: &mut TcpStream) -> io::Result<Option<ReliableMsg>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 4 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    stream.set_nonblocking(false)?;
    let res = stream.read_exact(&mut buf);
    let _ = stream.set_nonblocking(true);
    res?;
    let msg = serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}

// ── Server Transport State ──────────────────────────────────────────────────────────────────────

struct ClientPeer {
    id: PlayerId,
    token: u64,
    tcp: TcpStream,
    udp_addr: Option<SocketAddr>,
}

struct ServerShared {
    peers: Vec<ClientPeer>,
    incoming_reliable: Vec<(PlayerId, ReliableMsg)>,
    latest_inputs: HashMap<PlayerId, ClientInputPacket>,
    disconnected: Vec<PlayerId>,
    next_token: u64,
}

pub struct HostTransport {
    shared: Arc<Mutex<ServerShared>>,
    udp: UdpSocket,
    pub bind_addr: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for HostTransport {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl HostTransport {
    pub fn bind(bind_addr: &str) -> io::Result<Self> {
        let tcp_listener = TcpListener::bind(bind_addr)?;
        tcp_listener.set_nonblocking(true)?;
        let udp = UdpSocket::bind(bind_addr)?;
        udp.set_nonblocking(true)?;
        let local_addr = tcp_listener.local_addr()?.to_string();

        let shared = Arc::new(Mutex::new(ServerShared {
            peers: Vec::new(),
            incoming_reliable: Vec::new(),
            latest_inputs: HashMap::new(),
            disconnected: Vec::new(),
            next_token: 0x517C_C1B7_2722_0A95,
        }));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let shared_bg = Arc::clone(&shared);
        let stop_bg = Arc::clone(&stop);
        let udp_bg = udp.try_clone()?;

        thread::Builder::new()
            .name("survivors-net-host".into())
            .spawn(move || {
                let mut udp_buf = vec![0u8; 65536];
                while !stop_bg.load(std::sync::atomic::Ordering::Relaxed) {
                    // 1. Accept new TCP connections
                    while let Ok((stream, addr)) = tcp_listener.accept() {
                        let _ = stream.set_nodelay(true);
                        let _ = stream.set_nonblocking(true);
                        let mut guard = shared_bg.lock().unwrap();
                        let used: Vec<PlayerId> = guard.peers.iter().map(|p| p.id).collect();
                        if let Some(id) = (1..MAX_PLAYERS as u8).find(|id| !used.contains(id)) {
                            guard.next_token = guard.next_token.wrapping_mul(6364136223846793005).wrapping_add(1);
                            let token = guard.next_token;
                            info!("multiplayer host: accepted peer {addr} as player {id}");
                            guard.peers.push(ClientPeer {
                                id,
                                token,
                                tcp: stream,
                                udp_addr: None,
                            });
                        } else {
                            warn!("multiplayer host: party full, rejecting {addr}");
                        }
                    }

                    // 2. Poll TCP messages from connected peers
                    {
                        let mut guard = shared_bg.lock().unwrap();
                        let mut dead = Vec::new();
                        let mut msgs = Vec::new();
                        for peer in &mut guard.peers {
                            loop {
                                match read_tcp_msg(&mut peer.tcp) {
                                    Ok(Some(msg)) => msgs.push((peer.id, msg)),
                                    Ok(None) => break,
                                    Err(_) => {
                                        dead.push(peer.id);
                                        break;
                                    }
                                }
                            }
                        }
                        for (id, msg) in msgs {
                            guard.incoming_reliable.push((id, msg));
                        }
                        if !dead.is_empty() {
                            guard.peers.retain(|p| !dead.contains(&p.id));
                            guard.disconnected.extend(dead);
                        }
                    }

                    // 3. Poll UDP input datagrams
                    while let Ok((n, src)) = udp_bg.recv_from(&mut udp_buf) {
                        if n > 1 && udp_buf[0] == MAGIC_UDP_INPUT {
                            if let Ok(pkt) = serde_json::from_slice::<ClientInputPacket>(&udp_buf[1..n]) {
                                let mut guard = shared_bg.lock().unwrap();
                                if let Some(peer) = guard.peers.iter_mut().find(|p| p.id == pkt.player_id && p.token == pkt.token) {
                                    peer.udp_addr = Some(src);
                                    let jump_latch = guard.latest_inputs.get(&pkt.player_id).map_or(0, |old| old.buttons & (BTN_JUMP | BTN_MOBILITY));
                                    let mut merged = pkt;
                                    merged.buttons |= jump_latch;
                                    guard.latest_inputs.insert(pkt.player_id, merged);
                                }
                            }
                        }
                    }

                    thread::sleep(Duration::from_millis(2));
                }
            })?;

        Ok(Self {
            shared,
            udp,
            bind_addr: local_addr,
            stop,
        })
    }

    pub fn send_to(&self, player_id: PlayerId, msg: &ReliableMsg) {
        let mut guard = self.shared.lock().unwrap();
        if let Some(peer) = guard.peers.iter_mut().find(|p| p.id == player_id) {
            let _ = write_tcp_msg(&mut peer.tcp, msg);
        }
    }

    pub fn broadcast_reliable(&self, msg: &ReliableMsg) {
        let mut guard = self.shared.lock().unwrap();
        for peer in &mut guard.peers {
            let _ = write_tcp_msg(&mut peer.tcp, msg);
        }
    }

    pub fn broadcast_snapshot(&self, snap: &WorldSnapshot) {
        let Ok(json) = serde_json::to_vec(snap) else { return };
        let mut pkt = Vec::with_capacity(1 + json.len());
        pkt.push(MAGIC_UDP_SNAP);
        pkt.extend_from_slice(&json);

        let guard = self.shared.lock().unwrap();
        for peer in &guard.peers {
            if let Some(addr) = peer.udp_addr {
                let _ = self.udp.send_to(&pkt, addr);
            }
        }
    }

    pub fn drain_reliable(&self) -> (Vec<(PlayerId, ReliableMsg)>, Vec<PlayerId>) {
        let mut guard = self.shared.lock().unwrap();
        (
            std::mem::take(&mut guard.incoming_reliable),
            std::mem::take(&mut guard.disconnected),
        )
    }

    pub fn take_inputs(&self) -> HashMap<PlayerId, ClientInputPacket> {
        let mut guard = self.shared.lock().unwrap();
        let out = guard.latest_inputs.clone();
        // Clear edge-triggered buttons once consumed by a simulation tick
        for pkt in guard.latest_inputs.values_mut() {
            pkt.buttons = 0;
        }
        out
    }
}

// ── Client Transport State ──────────────────────────────────────────────────────────────────────

struct ClientShared {
    incoming_reliable: Vec<ReliableMsg>,
    latest_snapshot: Option<WorldSnapshot>,
    connected: bool,
    error: Option<String>,
}

pub struct ClientTransport {
    shared: Arc<Mutex<ClientShared>>,
    tcp_tx: Mutex<TcpStream>,
    udp: UdpSocket,
    pub server_addr: SocketAddr,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for ClientTransport {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl ClientTransport {
    pub fn connect(addr_str: &str) -> io::Result<Self> {
        let addr_with_port = if addr_str.contains(':') {
            addr_str.to_string()
        } else {
            format!("{addr_str}:{DEFAULT_PORT}")
        };
        let server_addr: SocketAddr = addr_with_port
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let stream = TcpStream::connect_timeout(&server_addr, Duration::from_secs(4))?;
        stream.set_nodelay(true)?;
        stream.set_nonblocking(true)?;
        let tcp_clone = stream.try_clone()?;

        let bind_any = if server_addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
        let udp = UdpSocket::bind(bind_any)?;
        udp.set_nonblocking(true)?;
        let udp_bg = udp.try_clone()?;

        let shared = Arc::new(Mutex::new(ClientShared {
            incoming_reliable: Vec::new(),
            latest_snapshot: None,
            connected: true,
            error: None,
        }));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shared_bg = Arc::clone(&shared);
        let stop_bg = Arc::clone(&stop);

        thread::Builder::new()
            .name("survivors-net-client".into())
            .spawn(move || {
                let mut tcp_rx = tcp_clone;
                let mut udp_buf = vec![0u8; 65536];
                while !stop_bg.load(std::sync::atomic::Ordering::Relaxed) {
                    loop {
                        match read_tcp_msg(&mut tcp_rx) {
                            Ok(Some(msg)) => {
                                shared_bg.lock().unwrap().incoming_reliable.push(msg);
                            }
                            Ok(None) => break,
                            Err(e) => {
                                let mut g = shared_bg.lock().unwrap();
                                g.connected = false;
                                g.error = Some(format!("Disconnected: {e}"));
                                return;
                            }
                        }
                    }

                    while let Ok((n, _)) = udp_bg.recv_from(&mut udp_buf) {
                        if n > 1 && udp_buf[0] == MAGIC_UDP_SNAP {
                            if let Ok(snap) = serde_json::from_slice::<WorldSnapshot>(&udp_buf[1..n]) {
                                let mut g = shared_bg.lock().unwrap();
                                if g.latest_snapshot.as_ref().is_none_or(|old| snap.seq >= old.seq) {
                                    g.latest_snapshot = Some(snap);
                                }
                            }
                        }
                    }

                    thread::sleep(Duration::from_millis(2));
                }
            })?;

        Ok(Self {
            shared,
            tcp_tx: Mutex::new(stream),
            udp,
            server_addr,
            stop,
        })
    }

    pub fn send_reliable(&self, msg: &ReliableMsg) {
        if let Ok(mut stream) = self.tcp_tx.lock() {
            let _ = write_tcp_msg(&mut stream, msg);
        }
    }

    pub fn send_input(&self, pkt: &ClientInputPacket) {
        let Ok(json) = serde_json::to_vec(pkt) else { return };
        let mut buf = Vec::with_capacity(1 + json.len());
        buf.push(MAGIC_UDP_INPUT);
        buf.extend_from_slice(&json);
        let _ = self.udp.send_to(&buf, self.server_addr);
    }

    pub fn drain_reliable(&self) -> Vec<ReliableMsg> {
        let mut g = self.shared.lock().unwrap();
        std::mem::take(&mut g.incoming_reliable)
    }

    pub fn take_snapshot(&self) -> Option<WorldSnapshot> {
        let mut g = self.shared.lock().unwrap();
        g.latest_snapshot.take()
    }

    pub fn is_connected(&self) -> (bool, Option<String>) {
        let g = self.shared.lock().unwrap();
        (g.connected, g.error.clone())
    }
}

// ── Bevy Multiplayer Resource ───────────────────────────────────────────────────────────────────

#[derive(Resource)]
pub struct NetSession {
    pub role: NetRole,
    pub local_id: PlayerId,
    pub token: u64,
    pub player_name: String,
    pub ip_input: String,
    pub ip_focused: bool,
    pub status_text: String,
    pub lobby_players: Vec<LobbyPlayer>,
    pub local_ready: bool,
    pub host: Option<HostTransport>,
    pub client: Option<ClientTransport>,
    pub snap_timer: f32,
    pub input_timer: f32,
    pub seq: u32,
    pub fx_seq: u32,
    pub last_fx_seq_seen: u32,
    pub pending_fx: Vec<NetFxEvent>,
    /// Tracks which players have locked in their card during `Phase::LevelUp`.
    pub levelup_picked: Vec<PlayerId>,
    /// Per-player rolled offers on the Host during `Phase::LevelUp`.
    pub(super) host_offers: HashMap<PlayerId, Vec<super::hud::Choice>>,
    /// Remote entity mapping on Clients: `net_id -> Entity`.
    pub client_enemies: HashMap<u32, Entity>,
    pub client_gems: HashMap<u32, Entity>,
    pub client_areas: HashMap<u32, Entity>,
    pub client_totems: HashMap<u32, Entity>,
    /// Latest interpolated target positions for remote units on Clients.
    pub latest_snap: Option<WorldSnapshot>,
    pub last_snap_time: Option<Instant>,
    /// Latched jump/mobility presses between UDP input sends.
    pub latched_buttons: u8,
    /// Latest inputs per remote client on Host.
    pub remote_inputs: HashMap<PlayerId, ClientInputPacket>,
    /// Queued mobility casts from remote clients on Host.
    pub remote_mobility_requests: Vec<(PlayerId, Option<Vec3>)>,
    /// Queued jump triggers from remote clients on Host.
    pub remote_jump_requests: Vec<PlayerId>,
    /// True when running as a headless dedicated server (`--server` / `WOW_SURVIVORS_SERVER`).
    pub headless: bool,
}

impl Default for NetSession {
    fn default() -> Self {
        let default_ip = crate::webenv::var("WOW_SURVIVORS_JOIN")
            .unwrap_or_else(|| format!("127.0.0.1:{DEFAULT_PORT}"));
        let player_name = crate::webenv::var("WOW_SURVIVORS_NAME")
            .or_else(|| std::env::var("USERNAME").ok())
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "Survivor".into());
        Self {
            role: NetRole::Offline,
            local_id: 0,
            token: 0,
            player_name,
            ip_input: default_ip,
            ip_focused: false,
            status_text: "Singleplayer / Offline".into(),
            lobby_players: Vec::new(),
            local_ready: false,
            host: None,
            client: None,
            snap_timer: 0.0,
            input_timer: 0.0,
            seq: 0,
            fx_seq: 0,
            last_fx_seq_seen: 0,
            pending_fx: Vec::new(),
            levelup_picked: Vec::new(),
            host_offers: HashMap::new(),
            client_enemies: HashMap::new(),
            client_gems: HashMap::new(),
            client_areas: HashMap::new(),
            client_totems: HashMap::new(),
            latest_snap: None,
            last_snap_time: None,
            latched_buttons: 0,
            remote_inputs: HashMap::new(),
            remote_mobility_requests: Vec::new(),
            remote_jump_requests: Vec::new(),
            headless: false,
        }
    }
}

impl NetSession {
    pub fn is_online(&self) -> bool {
        self.role != NetRole::Offline
    }

    pub fn is_client(&self) -> bool {
        self.role == NetRole::Client
    }

    pub fn player_count(&self) -> usize {
        if self.role == NetRole::Offline {
            1
        } else {
            self.lobby_players.len().max(1)
        }
    }

    pub fn start_host(&mut self, bind_addr: &str, local_lobby: LobbyPlayer) {
        self.disconnect();
        match HostTransport::bind(bind_addr) {
            Ok(host) => {
                self.status_text = format!("Hosting on {} (UDP+TCP)", host.bind_addr);
                self.role = NetRole::Host;
                self.local_id = 0;
                self.local_ready = true;
                self.host = Some(host);
                self.lobby_players.clear();
                if !self.headless {
                    let mut p = local_lobby;
                    p.id = 0;
                    p.ready = true;
                    self.lobby_players.push(p);
                }
            }
            Err(e) => {
                self.status_text = format!("Host failed: {e}");
            }
        }
    }

    pub fn start_client(&mut self, server_addr: &str, local_lobby: LobbyPlayer) {
        self.disconnect();
        match ClientTransport::connect(server_addr) {
            Ok(client) => {
                self.status_text = format!("Connecting to {}...", client.server_addr);
                client.send_reliable(&ReliableMsg::Hello {
                    name: local_lobby.name,
                    class: local_lobby.class,
                    display: local_lobby.display,
                    race: local_lobby.race,
                    sex: local_lobby.sex,
                });
                self.role = NetRole::Client;
                self.client = Some(client);
            }
            Err(e) => {
                self.status_text = format!("Join failed: {e}");
            }
        }
    }

    pub fn disconnect(&mut self) {
        self.host = None;
        self.client = None;
        self.role = NetRole::Offline;
        self.local_id = 0;
        self.local_ready = false;
        self.lobby_players.clear();
        self.levelup_picked.clear();
        self.host_offers.clear();
        self.client_enemies.clear();
        self.client_gems.clear();
        self.client_areas.clear();
        self.client_totems.clear();
        self.remote_inputs.clear();
        self.remote_mobility_requests.clear();
        self.remote_jump_requests.clear();
        self.latest_snap = None;
        self.status_text = "Singleplayer / Offline".into();
    }

    pub fn record_fx(&mut self, ev: NetFxEvent) {
        if self.role == NetRole::Host {
            self.pending_fx.push(ev);
        }
    }

    pub fn next_fx_seq(&mut self) -> u32 {
        self.fx_seq = self.fx_seq.wrapping_add(1);
        self.fx_seq
    }
}
