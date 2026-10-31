//! Network synchronization systems for LAN / ZeroTier / Tailscale multiplayer:
//! - CLI / environment bootstrap (`--host`, `--join`, `--server`)
//! - Reliable TCP lobby & level-up state machine synchronization
//! - 60 Hz UDP client input transmission & host remote-hero movement
//! - 30 Hz UDP authoritative world snapshot broadcasting & smooth client interpolation

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use benilla_world::world_point::WorldPoint;
use bevy::prelude::*;

use super::abilities::{GroundArea, Totem};
use super::enemies::{self, Enemy, Gem, GemKind};
use super::hero::{self, AimPoint, CamMode, CamRig, Hero, PartyHero};
use super::hud::{self, Offer};
use super::menu;
use super::net::{
    AreaSnap, ClientInputPacket, EnemySnap, GemSnap, LobbyPlayer, NetFxEvent, NetPhase, NetRole,
    NetSession, PlayerId, PlayerSnap, ReliableMsg, TotemSnap, WorldSnapshot, BTN_JUMP,
    BTN_MOBILITY, DEFAULT_PORT, INPUT_HZ, SNAPSHOT_HZ,
};
use super::units::{self, flat, yaw_toward, Fx, Marks, UnitSpec};
use super::{data, Combatant, NetId, Owned, Phase, Rng, Run, RunEntity, Selection};
use crate::creature_anim::{move_flags, MovementState};
use crate::net::{FieldChanged, Guid, ObjectStore};

/// Check CLI arguments and environment variables at startup to optionally auto-host, auto-join, or
/// run as a headless dedicated server.
pub(super) fn init_multiplayer_from_env(
    mut net: ResMut<NetSession>,
    mut selection: ResMut<Selection>,
) {
    let args: Vec<String> = std::env::args().collect();
    let mut host_addr: Option<String> = crate::webenv::var("WOW_SURVIVORS_HOST");
    let mut join_addr: Option<String> = crate::webenv::var("WOW_SURVIVORS_JOIN");
    let mut server_addr: Option<String> = crate::webenv::var("WOW_SURVIVORS_SERVER");

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--server" => {
                let next_arg = args.get(i + 1).filter(|s| !s.starts_with("--")).cloned();
                if next_arg.is_some() {
                    i += 1;
                }
                server_addr = Some(next_arg.unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}")));
            }
            "--host" => {
                let next_arg = args.get(i + 1).filter(|s| !s.starts_with("--")).cloned();
                if next_arg.is_some() {
                    i += 1;
                }
                host_addr = Some(next_arg.unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}")));
            }
            "--join" | "--connect" => {
                if let Some(addr) = args.get(i + 1) {
                    join_addr = Some(addr.clone());
                    i += 1;
                }
            }
            "--name" => {
                if let Some(name) = args.get(i + 1) {
                    net.player_name = name.clone();
                    i += 1;
                }
            }
            "--map" => {
                if let Some(m) = args.get(i + 1) {
                    let m_low = m.trim().to_ascii_lowercase();
                    let idx = m_low
                        .parse::<usize>()
                        .ok()
                        .filter(|&k| k < data::MAPS.len())
                        .unwrap_or_else(|| {
                            data::MAPS
                                .iter()
                                .position(|d| d.name.to_ascii_lowercase().contains(&m_low))
                                .unwrap_or(0)
                        });
                    selection.map = idx;
                    selection.stage = idx;
                    selection.version += 1;
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    if let Some(bind) = server_addr {
        let bind = if bind == "1" || bind.eq_ignore_ascii_case("true") || bind.is_empty() {
            format!("0.0.0.0:{DEFAULT_PORT}")
        } else if bind.contains(':') {
            bind
        } else {
            format!("{bind}:{DEFAULT_PORT}")
        };
        net.headless = true;
        let lp = menu::local_lobby_player(&selection, &net);
        net.start_host(&bind, lp);
        info!("survivors: started headless dedicated server on {bind}");
    } else if let Some(bind) = host_addr {
        let bind = if bind == "1" || bind.eq_ignore_ascii_case("true") || bind.is_empty() {
            format!("0.0.0.0:{DEFAULT_PORT}")
        } else if bind.contains(':') {
            bind
        } else {
            format!("{bind}:{DEFAULT_PORT}")
        };
        let lp = menu::local_lobby_player(&selection, &net);
        net.start_host(&bind, lp);
        info!("survivors: started listen server on {bind}");
    } else if let Some(addr) = join_addr {
        net.ip_input = addr.clone();
        let lp = menu::local_lobby_player(&selection, &net);
        net.start_client(&addr, lp);
        info!("survivors: connecting to {addr}");
    }
}

/// Poll TCP/UDP network queues on Host or Client, synchronize lobby/level-up state, ensure all
/// party heroes are spawned in the world, and drive remote party heroes on the Host.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(super) fn poll_network(
    mut commands: Commands,
    time: Res<Time>,
    phase: Res<State<Phase>>,
    mut next: ResMut<NextState<Phase>>,
    mut selection: ResMut<Selection>,
    mut run: ResMut<Run>,
    mut net: ResMut<NetSession>,
    mut offer: ResMut<Offer>,
    mut rng: ResMut<Rng>,
    world: WorldPoint,
    mut party_heroes: Query<(
        Entity,
        &mut Transform,
        &mut MovementState,
        &mut ObjectStore,
        &mut PartyHero,
        Has<Hero>,
    )>,
) {
    // 1. Ensure all `PartyHero` entities in `run.players` exist when in `Phase::Playing`.
    if *phase.get() != Phase::Menu {
        if run.players.is_empty() && !net.headless {
            let class = selection.class_def().unwrap_or(run.class);
            let (display, race, sex) = selection
                .look
                .and_then(|i| selection.looks.get(i))
                .copied()
                .unwrap_or((class.looks[0], run.race.max(1), 0));
            let mut c = Combatant::new(
                net.local_id,
                net.player_name.clone(),
                class,
                display,
                race,
                sex,
            );
            c.hp = run.hp;
            c.abilities = run.abilities.clone();
            c.passives = run.passives.clone();
            run.players.push(c);
        }

        let mut existing_ids: HashSet<PlayerId> = HashSet::new();
        for (e, _, _, _, mut ph, is_local) in &mut party_heroes {
            if is_local {
                if net.headless {
                    commands.entity(e).despawn();
                    continue;
                }
                if ph.id != net.local_id {
                    ph.id = net.local_id;
                }
            }
            existing_ids.insert(ph.id);
        }

        let base_pos = hero::arena(selection.map_def());
        let to_spawn: Vec<(PlayerId, &'static data::ClassDef, u32, u8, u8)> = run
            .players
            .iter()
            .filter(|c| !existing_ids.contains(&c.id))
            .map(|c| (c.id, c.class, c.display, c.race, c.sex))
            .collect();

        for (pid, cls, display, race, sex) in to_spawn {
            let mut pos = base_pos + hero::spawn_offset(pid);
            if let Some(h) = world.terrain_height_under(pos) {
                pos.y = h;
            }
            let serial = run.guid();
            let is_local = !net.headless && pid == net.local_id;
            hero::spawn_party_hero(
                &mut commands,
                pid,
                is_local,
                cls,
                display,
                race,
                sex,
                serial,
                pos,
            );
        }
    }

    match net.role {
        NetRole::Offline => {}
        NetRole::Host => {
            if !net.headless && *phase.get() == Phase::Menu {
                let lp = menu::local_lobby_player(&selection, &net);
                if let Some(first) = net.lobby_players.iter_mut().find(|p| p.id == 0) {
                    if first.display != lp.display || first.class != lp.class {
                        first.class = lp.class;
                        first.display = lp.display;
                        first.race = lp.race;
                        first.sex = lp.sex;
                    }
                }
            }

            let Some((reliable, disconnected)) = net.host.as_ref().map(|h| h.drain_reliable())
            else {
                return;
            };

            if !disconnected.is_empty() {
                for pid in &disconnected {
                    net.lobby_players.retain(|p| p.id != *pid);
                    net.remote_inputs.remove(pid);
                    for (e, _, _, _, ph, _) in &party_heroes {
                        if ph.id == *pid {
                            commands.entity(e).despawn();
                        }
                    }
                    run.players.retain(|p| p.id != *pid);
                }
                if let Some(host) = &net.host {
                    host.broadcast_reliable(&ReliableMsg::LobbyState {
                        map: selection.map,
                        players: net.lobby_players.clone(),
                    });
                }
            }

            for (pid, msg) in reliable {
                match msg {
                    ReliableMsg::Hello {
                        name,
                        class,
                        display,
                        race,
                        sex,
                    } => {
                        let class = class.min(data::CLASSES.len() - 1);
                        if let Some(slot) = net.lobby_players.iter_mut().find(|p| p.id == pid) {
                            slot.name = name.clone();
                            slot.class = class;
                            slot.display = display;
                            slot.race = race;
                            slot.sex = sex;
                        } else {
                            net.lobby_players.push(LobbyPlayer {
                                id: pid,
                                name: name.clone(),
                                class,
                                display,
                                race,
                                sex,
                                ready: false,
                            });
                        }
                        net.status_text = format!(
                            "Hosting ({} / {} players)",
                            net.lobby_players.len(),
                            super::net::MAX_PLAYERS
                        );
                        if let Some(host) = &net.host {
                            host.broadcast_reliable(&ReliableMsg::LobbyState {
                                map: selection.map,
                                players: net.lobby_players.clone(),
                            });
                            if *phase.get() != Phase::Menu {
                                if run.player(pid).is_none() {
                                    let cls = &data::CLASSES[class];
                                    run.players.push(Combatant::new(
                                        pid, name, cls, display, race, sex,
                                    ));
                                }
                                host.send_to(
                                    pid,
                                    &ReliableMsg::StartRun {
                                        map: selection.map,
                                        seed: rng.0,
                                        players: net.lobby_players.clone(),
                                    },
                                );
                            }
                        }
                    }
                    ReliableMsg::UpdateLobby {
                        class,
                        display,
                        race,
                        sex,
                        ready,
                    } => {
                        let class = class.min(data::CLASSES.len() - 1);
                        if let Some(slot) = net.lobby_players.iter_mut().find(|p| p.id == pid) {
                            slot.class = class;
                            slot.display = display;
                            slot.race = race;
                            slot.sex = sex;
                            slot.ready = ready;
                        }
                        if let Some(host) = &net.host {
                            host.broadcast_reliable(&ReliableMsg::LobbyState {
                                map: selection.map,
                                players: net.lobby_players.clone(),
                            });
                        }
                        if net.headless
                            && *phase.get() == Phase::Menu
                            && !net.lobby_players.is_empty()
                            && net.lobby_players.iter().all(|p| p.ready)
                            && hud::start_run_with_net(
                                &mut run,
                                &mut selection,
                                &mut net,
                                &mut rng,
                            )
                        {
                            info!(
                                "survivors server: all {} player(s) ready, launching run on {}",
                                net.lobby_players.len(),
                                selection.map_def().name
                            );
                            next.set(Phase::Playing);
                        }
                    }
                    ReliableMsg::PickCard { index } => {
                        if *phase.get() == Phase::LevelUp {
                            hud::apply_player_card_pick(
                                pid,
                                index,
                                &mut run,
                                &mut net,
                                &mut offer,
                                &mut rng,
                                &mut next,
                            );
                        }
                    }
                    ReliableMsg::SetPhase {
                        phase: net_phase,
                        won,
                    } => {
                        run.won = won;
                        if let Some(host) = &net.host {
                            host.broadcast_reliable(&ReliableMsg::SetPhase {
                                phase: net_phase,
                                won,
                            });
                        }
                        match net_phase {
                            NetPhase::Menu => next.set(Phase::Menu),
                            NetPhase::Playing => next.set(Phase::Playing),
                            NetPhase::Paused => next.set(Phase::Paused),
                            NetPhase::LevelUp => next.set(Phase::LevelUp),
                            NetPhase::GameOver => next.set(Phase::GameOver),
                        }
                    }
                    _ => {}
                }
            }

            // Drain UDP client inputs
            if let Some(inputs) = net.host.as_ref().map(|h| h.take_inputs()) {
                for (pid, pkt) in inputs {
                    if pkt.buttons & BTN_MOBILITY != 0 {
                        net.remote_mobility_requests
                            .push((pid, pkt.aim_pos.map(Vec3::from_array)));
                    }
                    if pkt.buttons & BTN_JUMP != 0 {
                        net.remote_jump_requests.push(pid);
                    }
                    net.remote_inputs.insert(pid, pkt);
                }
            }

            // Drive remote PartyHero entities on the Host
            if *phase.get() == Phase::Playing {
                let dt = time.delta_secs();
                let map = selection.map_def();
                let center = hero::arena(map);
                let jump_requests: HashSet<PlayerId> =
                    net.remote_jump_requests.drain(..).collect();

                for (_, mut tf, mut mv, _, mut ph, is_local) in &mut party_heroes {
                    if is_local && !net.headless {
                        continue;
                    }
                    let pid = ph.id;
                    let downed = run
                        .player(pid)
                        .is_some_and(|c| c.is_down())
                        || run.dead_t.is_some();
                    if downed {
                        mv.speed = 0.0;
                        mv.flags = 0;
                        continue;
                    }
                    let (speed, haste) = run
                        .player(pid)
                        .map_or((super::BASE_SPEED, 1.0), |c| {
                            let h = if c.mobile.haste_t > 0.0 {
                                c.mobile.haste
                            } else {
                                1.0
                            };
                            (c.stats().speed, h)
                        });
                    let move_speed = speed * haste;

                    let mut pos = tf.translation;
                    if let Some(pkt) = net.remote_inputs.get(&pid) {
                        let target_rot = Quat::from_rotation_y(pkt.yaw);
                        tf.rotation = tf.rotation.slerp(target_rot, (dt * 20.0).min(1.0));
                        let dir = Vec3::new(pkt.move_dir[0], 0.0, pkt.move_dir[1]);
                        if dir.length_squared() > 1e-4 && dt > 0.0 {
                            let dir = dir.normalize();
                            pos += dir * move_speed * dt;
                            mv.speed = move_speed;
                            let look =
                                flat(tf.rotation * Vec3::NEG_Z).normalize_or(dir);
                            mv.flags = hero::gait(look, dir);
                        } else {
                            mv.speed = 0.0;
                            mv.flags = 0;
                        }
                    } else {
                        mv.speed = 0.0;
                        mv.flags = 0;
                    }

                    let off = flat(pos - center);
                    if off.length() > map.leash {
                        let clamped = center + off.normalize() * map.leash;
                        pos.x = clamped.x;
                        pos.z = clamped.z;
                    }
                    let ground = world.terrain_height_under(pos).unwrap_or(pos.y);
                    if ph.vy == 0.0 && ph.air == 0.0 && jump_requests.contains(&pid) {
                        ph.vy = hero::JUMP_SPEED;
                    }
                    if ph.vy != 0.0 || ph.air > 0.0 {
                        ph.vy -= hero::GRAVITY * dt;
                        ph.air += ph.vy * dt;
                        if ph.air <= 0.0 {
                            ph.air = 0.0;
                            ph.vy = 0.0;
                        }
                    } else {
                        ph.air = 0.0;
                        ph.vy = 0.0;
                    }
                    pos.y = ground + ph.air;
                    mv.vertical_speed = ph.vy;
                    if ph.vy != 0.0 {
                        mv.flags |= move_flags::FALLING;
                    } else {
                        mv.flags &= !move_flags::FALLING;
                    }
                    tf.translation = pos;
                }
            }
        }
        NetRole::Client => {
            let Some((connected, err, msgs)) = net.client.as_ref().map(|c| {
                let (conn, err) = c.is_connected();
                (conn, err, c.drain_reliable())
            }) else {
                return;
            };

            if !connected {
                let reason = err.unwrap_or_else(|| "Connection closed".into());
                net.disconnect();
                net.status_text = reason;
                if *phase.get() != Phase::Menu {
                    next.set(Phase::Menu);
                }
                return;
            }

            let mut send_initial_lobby = false;
            for msg in msgs {
                match msg {
                    ReliableMsg::Welcome {
                        player_id,
                        token,
                        map,
                        players,
                    } => {
                        net.local_id = player_id;
                        net.token = token;
                        net.lobby_players = players;
                        net.status_text = format!("Connected (Slot {})", player_id + 1);
                        if map < data::MAPS.len() && selection.map != map {
                            selection.map = map;
                            selection.version += 1;
                        }
                        send_initial_lobby = true;
                    }
                    ReliableMsg::LobbyState { map, players } => {
                        net.lobby_players = players;
                        if map < data::MAPS.len() && selection.map != map {
                            selection.map = map;
                            selection.version += 1;
                        }
                    }
                    ReliableMsg::StartRun { map, seed, players } => {
                        for (e, ..) in &party_heroes {
                            commands.entity(e).despawn();
                        }
                        selection.map = map.min(data::MAPS.len() - 1);
                        selection.stage = selection.map;
                        rng.0 = seed | 1;
                        net.lobby_players = players.clone();
                        let next_guid = run.next_guid;
                        let combatants: Vec<Combatant> = players
                            .iter()
                            .map(|lp| {
                                let cls = &data::CLASSES[lp.class.min(data::CLASSES.len() - 1)];
                                Combatant::new(
                                    lp.id,
                                    lp.name.clone(),
                                    cls,
                                    lp.display,
                                    lp.race,
                                    lp.sex,
                                )
                            })
                            .collect();
                        let local_cls = combatants
                            .iter()
                            .find(|c| c.id == net.local_id)
                            .map_or(&data::CLASSES[0], |c| c.class);
                        let local_race = combatants
                            .iter()
                            .find(|c| c.id == net.local_id)
                            .map_or(1, |c| c.race);
                        *run = Run {
                            class: local_cls,
                            race: local_race,
                            next_guid,
                            players: combatants,
                            version: 1,
                            ..Run::default()
                        };
                        run.sync_local_from_combatant(net.local_id);
                        net.client_enemies.clear();
                        net.client_gems.clear();
                        net.client_areas.clear();
                        net.client_totems.clear();
                        net.latest_snap = None;
                        net.last_fx_seq_seen = 0;
                        next.set(Phase::Playing);
                    }
                    ReliableMsg::LevelUpOffer {
                        level,
                        offers,
                        picked,
                    } => {
                        run.level = level;
                        net.levelup_picked = picked;
                        let local_id = net.local_id;
                        run.sync_combatant_from_local(local_id);
                        if let (Some(wires), Some(c)) =
                            (offers.get(&local_id), run.player(local_id))
                        {
                            let choices: Vec<hud::Choice> = wires
                                .iter()
                                .filter_map(|w| hud::choice_from_net(c, w))
                                .collect();
                            offer.set_choices(choices);
                        }
                        next.set(Phase::LevelUp);
                    }
                    ReliableMsg::LevelUpProgress { picked } => {
                        net.levelup_picked = picked;
                    }
                    ReliableMsg::ResumeFromLevelUp { player_builds } => {
                        for (pid, b) in player_builds {
                            if let Some(c) = run.player_mut(pid) {
                                c.abilities = b
                                    .abilities
                                    .into_iter()
                                    .map(|(k, l)| Owned {
                                        def: data::ability(&k),
                                        level: l,
                                        cd: 0.2,
                                    })
                                    .collect();
                                c.passives = b
                                    .passives
                                    .into_iter()
                                    .filter_map(|(k, l)| {
                                        data::PASSIVES
                                            .iter()
                                            .find(|p| p.key == k)
                                            .map(|p| (p, l))
                                    })
                                    .collect();
                            }
                        }
                        run.sync_local_from_combatant(net.local_id);
                        run.version = run.version.wrapping_add(1);
                        offer.set_choices(Vec::new());
                        net.levelup_picked.clear();
                        next.set(Phase::Playing);
                    }
                    ReliableMsg::SetPhase {
                        phase: net_phase,
                        won,
                    } => {
                        run.won = won;
                        match net_phase {
                            NetPhase::Menu => next.set(Phase::Menu),
                            NetPhase::Playing => next.set(Phase::Playing),
                            NetPhase::Paused => next.set(Phase::Paused),
                            NetPhase::LevelUp => next.set(Phase::LevelUp),
                            NetPhase::GameOver => next.set(Phase::GameOver),
                        }
                    }
                    ReliableMsg::Banner { text, secs } => {
                        run.banner = Some((text, secs));
                    }
                    _ => {}
                }
            }

            if send_initial_lobby {
                let lp = menu::local_lobby_player(&selection, &net);
                if let Some(client) = &net.client {
                    client.send_reliable(&ReliableMsg::UpdateLobby {
                        class: lp.class,
                        display: lp.display,
                        race: lp.race,
                        sex: lp.sex,
                        ready: net.local_ready,
                    });
                }
            }
        }
    }
}

/// Transmit the local client's movement vector, yaw, cursor aim, and latched buttons at 60 Hz over UDP.
#[allow(clippy::too_many_arguments)]
pub(super) fn send_client_inputs(
    time: Res<Time<Real>>,
    keys: Res<ButtonInput<KeyCode>>,
    phase: Res<State<Phase>>,
    rig: Res<CamRig>,
    aim: Res<AimPoint>,
    mut net: ResMut<NetSession>,
    hero: Query<&Transform, With<Hero>>,
) {
    if !net.is_client() || *phase.get() != Phase::Playing {
        return;
    }
    if keys.just_pressed(KeyCode::Space) {
        net.latched_buttons |= BTN_JUMP;
    }
    net.input_timer += time.delta_secs();
    if net.input_timer < 1.0 / INPUT_HZ {
        return;
    }
    net.input_timer = 0.0;

    let mut dir = Vec3::ZERO;
    if keys.any_pressed([KeyCode::KeyW, KeyCode::ArrowUp]) {
        dir.z -= 1.0;
    }
    if keys.any_pressed([KeyCode::KeyS, KeyCode::ArrowDown]) {
        dir.z += 1.0;
    }
    if keys.any_pressed([KeyCode::KeyA, KeyCode::ArrowLeft]) {
        dir.x -= 1.0;
    }
    if keys.any_pressed([KeyCode::KeyD, KeyCode::ArrowRight]) {
        dir.x += 1.0;
    }
    if rig.mode == CamMode::Orbit {
        let (sy, cy) = rig.yaw.sin_cos();
        dir = Vec3::new(dir.x * cy + dir.z * sy, 0.0, dir.z * cy - dir.x * sy);
    }
    let dir2 = Vec2::new(dir.x, dir.z).normalize_or_zero();

    let yaw = hero
        .single()
        .map(|tf| tf.rotation.to_euler(EulerRot::YXZ).0)
        .unwrap_or(0.0);

    let buttons = net.latched_buttons;
    net.latched_buttons = 0;
    net.seq = net.seq.wrapping_add(1);

    let pkt = ClientInputPacket {
        player_id: net.local_id,
        token: net.token,
        seq: net.seq,
        move_dir: [dir2.x, dir2.y],
        yaw,
        aim_pos: aim.0.map(|a| a.to_array()),
        buttons,
    };
    if let Some(client) = &net.client {
        client.send_input(&pkt);
    }
}

/// Broadcast the Host's authoritative world snapshot over UDP at 30 Hz.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(super) fn broadcast_host_snapshot(
    time: Res<Time<Real>>,
    mut run: ResMut<Run>,
    mut net: ResMut<NetSession>,
    selection: Res<Selection>,
    party_heroes: Query<(&Transform, &MovementState, &PartyHero, &Marks)>,
    enemies: Query<(&Transform, &Enemy, &Marks, &NetId)>,
    gems: Query<(&Transform, &Gem, &NetId)>,
    areas: Query<(&Transform, &GroundArea, &NetId)>,
    totems: Query<(&Transform, &Totem, &NetId)>,
) {
    if net.role != NetRole::Host {
        return;
    }
    net.snap_timer += time.delta_secs();
    if net.snap_timer < 1.0 / SNAPSHOT_HZ {
        return;
    }
    net.snap_timer = 0.0;
    net.seq = net.seq.wrapping_add(1);

    let local_id = net.local_id;
    run.sync_combatant_from_local(local_id);

    let mut hero_map: HashMap<PlayerId, (Vec3, f32, u32, f32, f32, Vec<u32>)> = HashMap::new();
    for (tf, mv, ph, marks) in &party_heroes {
        let yaw = tf.rotation.to_euler(EulerRot::YXZ).0;
        hero_map.insert(
            ph.id,
            (
                tf.translation,
                yaw,
                mv.flags,
                mv.speed,
                ph.vy,
                marks.active_spells(),
            ),
        );
    }

    let players: Vec<PlayerSnap> = run
        .players
        .iter()
        .map(|c| {
            let stats = c.stats();
            let (pos, yaw, move_flags, speed, vertical_speed, buffs) = hero_map
                .get(&c.id)
                .cloned()
                .unwrap_or((hero::arena(selection.map_def()), 0.0, 0, 0.0, 0.0, Vec::new()));
            PlayerSnap {
                id: c.id,
                pos: pos.to_array(),
                yaw,
                speed,
                move_flags,
                vertical_speed,
                hp: c.hp,
                max_hp: stats.max_hp,
                shield: if c.shield_t > 0.0 { c.shield } else { 0.0 },
                downed: c.downed_revive,
                buffs,
                ability_cds: c.abilities.iter().map(|o| o.cd).collect(),
                mobility_cd: c.mobile.cd,
            }
        })
        .collect();

    let enemy_snaps: Vec<EnemySnap> = enemies
        .iter()
        .map(|(tf, en, marks, nid)| EnemySnap {
            net_id: nid.0,
            display: en.def.display,
            scale: en.def.scale,
            level: en.def.level,
            pos: tf.translation.to_array(),
            yaw: tf.rotation.to_euler(EulerRot::YXZ).0,
            speed: en.speed,
            hp: en.hp,
            max_hp: en.max_hp,
            boss: en.boss,
            dead: en.dead,
            debuffs: marks.active_spells(),
        })
        .collect();

    let gem_snaps: Vec<GemSnap> = gems
        .iter()
        .map(|(tf, g, nid)| GemSnap {
            net_id: nid.0,
            display: g.display,
            scale: g.scale,
            pos: tf.translation.to_array(),
        })
        .collect();

    let area_snaps: Vec<AreaSnap> = areas
        .iter()
        .map(|(tf, a, nid)| AreaSnap {
            net_id: nid.0,
            spell: a.spell,
            radius: a.radius,
            pos: tf.translation.to_array(),
            overhead: a.overhead,
        })
        .collect();

    let totem_snaps: Vec<TotemSnap> = totems
        .iter()
        .map(|(tf, t, nid)| TotemSnap {
            net_id: nid.0,
            display: t.display,
            pos: tf.translation.to_array(),
        })
        .collect();

    let fx_events = std::mem::take(&mut net.pending_fx);

    let snap = WorldSnapshot {
        seq: net.seq,
        elapsed: run.elapsed,
        level: run.level,
        xp: run.xp,
        kills: run.kills,
        bosses: run.bosses,
        damage_done: run.damage_done,
        players,
        enemies: enemy_snaps,
        gems: gem_snaps,
        areas: area_snaps,
        totems: totem_snaps,
        fx_events,
    };

    if let Some(host) = &net.host {
        host.broadcast_snapshot(&snap);
    }
}

/// Apply incoming UDP `WorldSnapshot`s on Clients and smoothly interpolate entity positions every frame.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(super) fn apply_client_snapshot(
    mut commands: Commands,
    time: Res<Time<Real>>,
    phase: Res<State<Phase>>,
    selection: Res<Selection>,
    mut run: ResMut<Run>,
    mut net: ResMut<NetSession>,
    mut party_heroes: Query<(
        Entity,
        &Guid,
        &mut Transform,
        &mut MovementState,
        &mut ObjectStore,
        &mut Marks,
        &PartyHero,
        Has<Hero>,
    )>,
    mut enemies: Query<
        (
            Entity,
            &Guid,
            &mut Transform,
            &mut MovementState,
            &mut Enemy,
            &mut ObjectStore,
            &mut Marks,
        ),
        Without<PartyHero>,
    >,
    mut gems: Query<
        (Entity, &mut Transform),
        (With<Gem>, Without<PartyHero>, Without<Enemy>),
    >,
    mut fx: Fx,
    mut fields: MessageWriter<FieldChanged>,
) {
    if !net.is_client() || *phase.get() == Phase::Menu {
        return;
    }

    if let Some(snap) = net.client.as_ref().and_then(|c| c.take_snapshot()) {
        run.elapsed = snap.elapsed;
        run.level = snap.level;
        run.xp = snap.xp;
        run.kills = snap.kills;
        run.bosses = snap.bosses;
        run.damage_done = snap.damage_done;

        // 1. Update combatant vitals & cooldowns
        for ps in &snap.players {
            if let Some(c) = run.player_mut(ps.id) {
                c.hp = ps.hp;
                c.shield = ps.shield;
                c.shield_t = if ps.shield > 0.0 { 1.0 } else { 0.0 };
                c.downed_revive = ps.downed;
                c.mobile.cd = ps.mobility_cd;
                for (o, &cd) in c.abilities.iter_mut().zip(ps.ability_cds.iter()) {
                    o.cd = cd;
                }
            }
        }
        if snap.players.iter().all(|p| p.downed.is_some()) && !snap.players.is_empty() {
            if run.dead_t.is_none() {
                run.dead_t = Some(0.0);
                run.banner = Some(("Your party has fallen".into(), 3.0));
            }
        } else {
            run.dead_t = None;
        }
        run.sync_local_from_combatant(net.local_id);

        // 2. Update PartyHero animations, buffs, & health state
        for (entity, guid, mut tf, mut mv, mut store, mut marks, ph, is_local) in &mut party_heroes
        {
            if let Some(ps) = snap.players.iter().find(|p| p.id == ph.id) {
                let target_pos = Vec3::from_array(ps.pos);
                if ps.downed.is_some() {
                    units::kill_unit(&mut store);
                    marks.clear();
                    marks.flush(entity, guid.0, &mut store, &mut fields);
                } else {
                    units::revive_unit(&mut store);
                    marks.set(&ps.buffs);
                    marks.flush(entity, guid.0, &mut store, &mut fields);
                }
                if !is_local {
                    mv.flags = ps.move_flags;
                    mv.speed = ps.speed;
                    if flat(tf.translation - target_pos).length() > 12.0 {
                        tf.translation = target_pos;
                    }
                } else if ps.downed.is_some() || flat(tf.translation - target_pos).length() > 7.5 {
                    tf.translation = target_pos;
                }
            }
        }

        // 3. Reconcile Enemies
        let roster = data::roster(selection.stage);
        let mut seen_enemies: HashSet<u32> = HashSet::with_capacity(snap.enemies.len());
        for es in &snap.enemies {
            seen_enemies.insert(es.net_id);
            let target_pos = Vec3::from_array(es.pos);
            if let Some(&entity) = net.client_enemies.get(&es.net_id) {
                if let Ok((e, guid, mut tf, mut mv, mut en, mut store, mut marks)) =
                    enemies.get_mut(entity)
                {
                    en.hp = es.hp;
                    en.max_hp = es.max_hp;
                    if flat(tf.translation - target_pos).length() > 15.0 {
                        tf.translation = target_pos;
                    }
                    if es.dead && !en.dead {
                        en.dead = true;
                        mv.speed = 0.0;
                        mv.flags = 0;
                        enemies::mark_dying(&mut commands, e);
                        units::kill_unit(&mut store);
                        marks.clear();
                    } else if !es.dead {
                        marks.set(&es.debuffs);
                    }
                    marks.flush(e, guid.0, &mut store, &mut fields);
                    continue;
                }
            }
            if !es.dead {
                let def = if es.boss {
                    roster
                        .bosses
                        .iter()
                        .find(|d| d.display == es.display)
                        .unwrap_or(&roster.bosses[0])
                } else {
                    roster
                        .enemies
                        .iter()
                        .find(|d| d.display == es.display)
                        .unwrap_or(&roster.enemies[0])
                };
                let serial = run.guid();
                let e = units::spawn_unit(
                    &mut commands,
                    &UnitSpec {
                        serial,
                        display: es.display,
                        scale: es.scale,
                        level: es.level,
                        faction: units::FACTION_MONSTER,
                        bytes0: 1 << 8,
                        weapons: &[],
                        sheath: 0,
                    },
                    target_pos,
                    es.yaw,
                );
                commands.entity(e).insert((
                    NetId(es.net_id),
                    Enemy {
                        def,
                        hp: es.hp,
                        max_hp: es.max_hp,
                        damage: def.damage,
                        speed: es.speed,
                        xp: def.xp,
                        boss: es.boss,
                        radius: enemies::radius_for(def),
                        attack_cd: 0.5,
                        dots: Vec::new(),
                        slow: 1.0,
                        slow_t: 0.0,
                        root_t: 0.0,
                        stun_t: 0.0,
                        fear_t: 0.0,
                        weaken: 1.0,
                        weaken_t: 0.0,
                        dead: false,
                    },
                    Marks::debuffs(),
                ));
                net.client_enemies.insert(es.net_id, e);
            }
        }
        let stale_enemies: Vec<u32> = net
            .client_enemies
            .keys()
            .copied()
            .filter(|id| !seen_enemies.contains(id))
            .collect();
        for id in stale_enemies {
            if let Some(entity) = net.client_enemies.remove(&id) {
                if let Ok((e, guid, _, mut mv, mut en, mut store, mut marks)) =
                    enemies.get_mut(entity)
                {
                    if !en.dead {
                        en.dead = true;
                        mv.speed = 0.0;
                        mv.flags = 0;
                        enemies::mark_dying(&mut commands, e);
                        units::kill_unit(&mut store);
                        marks.clear();
                        marks.flush(e, guid.0, &mut store, &mut fields);
                    }
                }
            }
        }

        // 4. Reconcile Gems
        let mut seen_gems: HashSet<u32> = HashSet::with_capacity(snap.gems.len());
        for gs in &snap.gems {
            seen_gems.insert(gs.net_id);
            if let Some(&entity) = net.client_gems.get(&gs.net_id) {
                if gems.get(entity).is_ok() {
                    continue;
                }
            }
            let serial = run.guid();
            let pos = Vec3::from_array(gs.pos);
            let e = units::spawn_object(&mut commands, serial, gs.display, gs.scale, pos);
            commands.entity(e).insert((
                NetId(gs.net_id),
                Gem {
                    kind: GemKind::Xp(1),
                    display: gs.display,
                    scale: gs.scale,
                    pulled_by: None,
                    speed: 0.0,
                },
            ));
            net.client_gems.insert(gs.net_id, e);
        }
        let stale_gems: Vec<u32> = net
            .client_gems
            .keys()
            .copied()
            .filter(|id| !seen_gems.contains(id))
            .collect();
        for id in stale_gems {
            if let Some(e) = net.client_gems.remove(&id) {
                commands.entity(e).despawn();
            }
        }

        // 5. Reconcile GroundAreas
        let mut seen_areas: HashSet<u32> = HashSet::with_capacity(snap.areas.len());
        for asnap in &snap.areas {
            seen_areas.insert(asnap.net_id);
            if net.client_areas.contains_key(&asnap.net_id) {
                continue;
            }
            let pos = Vec3::from_array(asnap.pos);
            let e = if asnap.overhead {
                commands
                    .spawn((
                        Transform::from_translation(pos),
                        RunEntity,
                        NetId(asnap.net_id),
                    ))
                    .id()
            } else {
                let serial = run.guid();
                let ent = units::spawn_area_object(
                    &mut commands,
                    serial,
                    asnap.spell,
                    asnap.radius,
                    pos,
                );
                commands.entity(ent).insert(NetId(asnap.net_id));
                ent
            };
            net.client_areas.insert(asnap.net_id, e);
        }
        let stale_areas: Vec<u32> = net
            .client_areas
            .keys()
            .copied()
            .filter(|id| !seen_areas.contains(id))
            .collect();
        for id in stale_areas {
            if let Some(e) = net.client_areas.remove(&id) {
                commands.entity(e).despawn();
            }
        }

        // 6. Reconcile Totems
        let mut seen_totems: HashSet<u32> = HashSet::with_capacity(snap.totems.len());
        for ts in &snap.totems {
            seen_totems.insert(ts.net_id);
            if net.client_totems.contains_key(&ts.net_id) {
                continue;
            }
            let serial = run.guid();
            let pos = Vec3::from_array(ts.pos);
            let e = units::spawn_unit(
                &mut commands,
                &UnitSpec {
                    serial,
                    display: ts.display,
                    scale: 1.0,
                    level: 60,
                    faction: units::FACTION_FRIENDLY,
                    bytes0: 0,
                    weapons: &[],
                    sheath: 0,
                },
                pos,
                0.0,
            );
            commands.entity(e).insert(NetId(ts.net_id));
            net.client_totems.insert(ts.net_id, e);
        }
        let stale_totems: Vec<u32> = net
            .client_totems
            .keys()
            .copied()
            .filter(|id| !seen_totems.contains(id))
            .collect();
        for id in stale_totems {
            if let Some(e) = net.client_totems.remove(&id) {
                commands
                    .entity(e)
                    .insert(benilla_world::model_fade::DespawnFade::default());
            }
        }

        // 7. Replay Visual FX Events
        let hero_entities: HashMap<PlayerId, Entity> = party_heroes
            .iter()
            .map(|(e, .., ph, _)| (ph.id, e))
            .collect();
        let resolve = |is_player: bool, id: u32, net: &NetSession| -> Option<Entity> {
            if is_player {
                hero_entities.get(&(id as PlayerId)).copied()
            } else {
                net.client_enemies
                    .get(&id)
                    .or_else(|| net.client_totems.get(&id))
                    .or_else(|| net.client_areas.get(&id))
                    .copied()
            }
        };

        for ev in &snap.fx_events {
            match ev {
                NetFxEvent::Cast {
                    seq,
                    caster_is_player,
                    caster_id,
                    spell,
                    targets,
                    dest,
                } => {
                    if *seq <= net.last_fx_seq_seen {
                        continue;
                    }
                    net.last_fx_seq_seen = *seq;
                    if let Some(c_ent) = resolve(*caster_is_player, *caster_id, &net) {
                        let t_ents: Vec<Entity> = targets
                            .iter()
                            .filter_map(|&tid| {
                                net.client_enemies
                                    .get(&tid)
                                    .copied()
                                    .or_else(|| hero_entities.get(&(tid as PlayerId)).copied())
                            })
                            .collect();
                        fx.cast_local(c_ent, *spell, &t_ents, dest.map(Vec3::from_array));
                    }
                }
                NetFxEvent::Impact {
                    seq,
                    target_is_player,
                    target_id,
                    spell,
                } => {
                    if *seq <= net.last_fx_seq_seen {
                        continue;
                    }
                    net.last_fx_seq_seen = *seq;
                    if let Some(t_ent) = resolve(*target_is_player, *target_id, &net) {
                        fx.impact_local(t_ent, *spell);
                    }
                }
                NetFxEvent::Swing {
                    seq,
                    attacker_is_player,
                    attacker_id,
                    victim_is_player,
                    victim_id,
                    damage,
                } => {
                    if *seq <= net.last_fx_seq_seen {
                        continue;
                    }
                    net.last_fx_seq_seen = *seq;
                    if let (Some(a_ent), Some(v_ent)) = (
                        resolve(*attacker_is_player, *attacker_id, &net),
                        resolve(*victim_is_player, *victim_id, &net),
                    ) {
                        fx.swing_local(a_ent, v_ent, *damage);
                    }
                }
                NetFxEvent::CombatNumber {
                    seq,
                    anchor_is_player,
                    anchor_id,
                    amount,
                    crit,
                    color,
                } => {
                    if *seq <= net.last_fx_seq_seen {
                        continue;
                    }
                    net.last_fx_seq_seen = *seq;
                    if let Some(t_ent) = resolve(*anchor_is_player, *anchor_id, &net) {
                        fx.number_local(t_ent, *amount, *crit, *color);
                    }
                }
            }
        }

        net.latest_snap = Some(snap);
        net.last_snap_time = Some(Instant::now());
    }

    // Smooth per-frame interpolation of remote heroes, enemies, and gems toward latest snapshot
    let Some(snap) = &net.latest_snap else {
        return;
    };
    let dt = time.delta_secs();
    let alpha = (dt * 18.0).min(1.0);

    for (_, _, mut tf, _, _, _, ph, is_local) in &mut party_heroes {
        if is_local {
            continue;
        }
        if let Some(ps) = snap.players.iter().find(|p| p.id == ph.id) {
            let target_pos = Vec3::from_array(ps.pos);
            tf.translation = tf.translation.lerp(target_pos, alpha);
            let target_rot = Quat::from_rotation_y(ps.yaw);
            tf.rotation = tf.rotation.slerp(target_rot, alpha);
        }
    }

    for es in &snap.enemies {
        if es.dead {
            continue;
        }
        if let Some(&entity) = net.client_enemies.get(&es.net_id) {
            if let Ok((_, _, mut tf, mut mv, en, ..)) = enemies.get_mut(entity) {
                let target_pos = Vec3::from_array(es.pos);
                let d = flat(target_pos - tf.translation);
                if d.length() > 0.08 && !en.dead {
                    mv.speed = en.speed;
                    mv.flags = move_flags::FORWARD;
                    let target_rot = Quat::from_rotation_y(yaw_toward(d));
                    tf.rotation = tf.rotation.slerp(target_rot, alpha);
                } else {
                    mv.speed = 0.0;
                    mv.flags = 0;
                }
                tf.translation = tf.translation.lerp(target_pos, alpha);
            }
        }
    }

    for gs in &snap.gems {
        if let Some(&entity) = net.client_gems.get(&gs.net_id) {
            if let Ok((_, mut tf)) = gems.get_mut(entity) {
                let target_pos = Vec3::from_array(gs.pos);
                tf.translation = tf.translation.lerp(target_pos, alpha);
            }
        }
    }
}
