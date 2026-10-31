//! The hero(es): bodies for the local player and remote party members, WASD movement over the
//! terrain, top-down / orbit camera, health, downed state, and teammate proximity revival.

use benilla_world::view::WorldCamera;
use benilla_world::world_point::WorldPoint;
use bevy::input::mouse::{AccumulatedMouseMotion, MouseScrollUnit, MouseWheel};
use bevy::prelude::*;

use super::data::{self, ClassDef};
use super::net::{NetRole, NetSession, PlayerId, BTN_JUMP, REVIVE_RADIUS, REVIVE_SECS};
use super::units::{self, flat, wow_to_bevy, yaw_toward, Fx, Marks, UnitSpec, COLOR_HEAL};
use super::{Phase, Run, Selection};
use crate::creature_anim::{move_flags, MovementState};
use crate::net::{FieldChanged, Guid, ObjectStore};

/// Marker component on the local player's hero entity (`hero.single()` queries this).
#[derive(Component)]
pub(super) struct Hero {
    /// Vertical velocity in yards/second while airborne; 0 on the ground.
    pub(super) vy: f32,
    /// Height above the ground while airborne. Kept here, not in the transform: the creature
    /// ground clamp (`net::motion::ground_clamp_creatures`) re-seats every unit on the floor at
    /// the top of the frame, so the arc is re-applied over the ground each frame.
    pub(super) air: f32,
}

/// Present on every party member's hero entity (both the local `Hero` and remote teammates).
#[derive(Component, Clone, Copy)]
pub(super) struct PartyHero {
    pub id: PlayerId,
    pub vy: f32,
    pub air: f32,
}

/// The body radius enemies stop at.
pub(super) const HERO_RADIUS: f32 = 0.7;

/// The hop's initial upward speed and the gravity that brings it back down — the reference
/// `CMovement` jump seed and gravity (`player/state.rs`'s `JUMP_SPEED` / `GRAVITY`).
pub(super) const JUMP_SPEED: f32 = 7.955_547;
pub(super) const GRAVITY: f32 = 19.291_105;

/// Radians of orbit per raw mouse unit while right-dragging.
const ORBIT_RATE: f32 = 0.008;
/// Elevation clamp (radians): ~10°..~83° above the horizon, so the camera never dives underground.
const PITCH_MIN: f32 = 0.18;
const PITCH_MAX: f32 = 1.45;
/// The locked view's tilt.
const FIXED_PITCH: f32 = 61.0 * std::f32::consts::PI / 180.0;
/// Wheel zoom range, yards from the hero's shoulders.
const ZOOM_MIN: f32 = 1.5;
const ZOOM_MAX: f32 = 70.0;

/// Whether the camera is locked; `C` toggles it.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum CamMode {
    /// Locked top-down (58°), laptop-friendly — the original feel.
    #[default]
    Fixed,
    /// Unlocked: a mouse drag swings and tilts it around the hero.
    Orbit,
}

#[derive(Resource)]
pub(super) struct CamRig {
    pub dist: f32,
    pub orbit: f32,
    /// Orbit azimuth (radians) around the hero; right-drag swings it during play.
    pub yaw: f32,
    /// Elevation above the horizon (radians), clamped so the camera never dives underground.
    pub pitch: f32,
    pub mode: CamMode,
}

impl Default for CamRig {
    fn default() -> Self {
        Self {
            dist: 30.0,
            orbit: 0.6,
            yaw: 0.0,
            pitch: FIXED_PITCH,
            mode: CamMode::Fixed,
        }
    }
}

/// Where a battleground's runs start.
pub(super) fn arena(map: &data::MapDef) -> Vec3 {
    wow_to_bevy(map.center)
}

/// Offset around the arena center for party slot `id` so players don't spawn stacked on top of each other.
pub(super) fn spawn_offset(id: PlayerId) -> Vec3 {
    match id {
        0 => Vec3::ZERO,
        1 => Vec3::new(2.5, 0.0, 0.0),
        2 => Vec3::new(-2.5, 0.0, 0.0),
        3 => Vec3::new(0.0, 0.0, 2.5),
        _ => Vec3::new(0.0, 0.0, -2.5),
    }
}

/// The world is the battleground being stood in ([`Selection::stage`]): entering one on another
/// continent worldports, which raises the loading screen until the new ground is in.
pub(super) fn follow_map(
    selection: Res<Selection>,
    current: Option<ResMut<benilla_world::world_map::CurrentMap>>,
) {
    let Some(mut current) = current else {
        return;
    };
    let want = selection.stage_def().map;
    if current.0 != want {
        current.0 = want;
    }
}

/// Spawn the local player's hero in the class trainer's look, weapons drawn.
pub(super) fn spawn_hero(
    commands: &mut Commands,
    class: &'static ClassDef,
    display: u32,
    race: u8,
    sex: u8,
    serial: u64,
    pos: Vec3,
) -> Entity {
    spawn_party_hero(commands, 0, true, class, display, race, sex, serial, pos)
}

/// Spawn a party member's hero (`is_local = true` attaches [`Hero`] for camera/input focus).
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_party_hero(
    commands: &mut Commands,
    id: PlayerId,
    is_local: bool,
    class: &'static ClassDef,
    display: u32,
    race: u8,
    sex: u8,
    serial: u64,
    pos: Vec3,
) -> Entity {
    let bytes0 = u32::from(race)
        | u32::from(class.id) << 8
        | u32::from(sex) << 16
        | u32::from(class.power) << 24;
    let e = units::spawn_unit(
        commands,
        &UnitSpec {
            serial,
            display,
            scale: 1.0,
            level: 60,
            faction: units::FACTION_FRIENDLY,
            bytes0,
            weapons: class.weapons,
            sheath: class.sheath,
        },
        pos,
        0.0,
    );
    commands.entity(e).insert((
        PartyHero {
            id,
            vy: 0.0,
            air: 0.0,
        },
        Marks::buffs(),
    ));
    if is_local {
        commands.entity(e).insert(Hero { vy: 0.0, air: 0.0 });
    }
    e
}

/// Midday over the battleground, whatever the clock says.
pub(super) const MINUTE_OF_DAY: u32 = 780;

pub(super) fn daylight(debug: Option<ResMut<benilla_world::dev_state::DebugState>>) {
    let Some(mut debug) = debug else { return };
    if debug.lighting.follow_server_time || debug.lighting.manual_minute != MINUTE_OF_DAY {
        debug.lighting.follow_server_time = false;
        debug.lighting.manual_minute = MINUTE_OF_DAY;
    }
}

/// The ground under the mouse cursor, `None` with the cursor off the window. The locked camera's
/// hero faces it, and the aimed mobility spells (Blink, Charge) go toward it.
#[derive(Resource, Default)]
pub(super) struct AimPoint(pub(super) Option<Vec3>);

/// Cast the cursor onto the terrain: the camera ray meets the plane at the hero's height, then the
/// terrain height there, a few times over, which settles on hills within a frame.
pub(super) fn aim_cursor(
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    cams: Query<(&Camera, &Transform), With<WorldCamera>>,
    hero: Query<&Transform, (With<Hero>, Without<WorldCamera>)>,
    world: WorldPoint,
    mut aim: ResMut<AimPoint>,
) {
    let point = (|| {
        let cursor = windows.single().ok()?.cursor_position()?;
        let (cam, tf) = cams.single().ok()?;
        let ray = cam
            .viewport_to_world(&GlobalTransform::from(*tf), cursor)
            .ok()?;
        let mut y = hero.single().ok()?.translation.y;
        let mut at = None;
        for _ in 0..4 {
            if ray.direction.y.abs() < 1e-4 {
                break;
            }
            let t = (y - ray.origin.y) / ray.direction.y;
            if t <= 0.0 {
                break;
            }
            let p = ray.origin + *ray.direction * t;
            at = Some(p);
            match world.terrain_height_under(p) {
                Some(h) if (h - y).abs() > 0.05 => y = h,
                _ => break,
            }
        }
        at
    })();
    if aim.0 != point {
        aim.0 = point;
    }
}

/// WASD over the terrain within the battleground's leash, and the streaming focus on the hero.
#[allow(clippy::too_many_arguments)]
pub(super) fn drive_hero(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    phase: Res<State<Phase>>,
    run: Res<Run>,
    selection: Res<Selection>,
    rig: Res<CamRig>,
    aim: Res<AimPoint>,
    world: WorldPoint,
    auto: Option<Res<super::auto::Auto>>,
    mut net: ResMut<NetSession>,
    mut player: ResMut<crate::player::Player>,
    mut hero: Query<(&mut Transform, &mut MovementState, &mut Hero, Option<&mut PartyHero>)>,
) {
    let local_down = run
        .player(net.local_id)
        .is_some_and(|c| c.is_down())
        || run.dead_t.is_some();

    let focus = match hero.single_mut() {
        Ok((mut tf, mut mv, mut hm, mut ph)) => {
            let dt = time.delta_secs();
            let mut dir = Vec3::ZERO;
            if let Some(auto) = auto
                .as_deref()
                .filter(|_| *phase.get() == Phase::Playing && !local_down)
            {
                dir = super::auto::steer(auto);
            } else if *phase.get() == Phase::Playing && !local_down {
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
                // In orbit mode WASD is camera-relative; in fixed mode it stays world-relative.
                if rig.mode == CamMode::Orbit {
                    let (sy, cy) = rig.yaw.sin_cos();
                    dir = Vec3::new(dir.x * cy + dir.z * sy, 0.0, dir.z * cy - dir.x * sy);
                }
            }
            let mut pos = tf.translation;
            // Locked, the hero faces the cursor and WASD moves him under it, so he backpedals and
            // strafes as a player does; unlocked (or driven), he faces where he runs.
            let face = (rig.mode == CamMode::Fixed
                && auto.is_none()
                && *phase.get() == Phase::Playing
                && !local_down)
                .then(|| aim.0.map(|a| flat(a - pos)))
                .flatten()
                .filter(|d| d.length() > 0.5)
                .map(|d| d.normalize());
            if let Some(look) = face {
                let target = Quat::from_rotation_y(yaw_toward(look));
                tf.rotation = tf.rotation.slerp(target, (dt * 20.0).min(1.0));
            }
            if dir != Vec3::ZERO && dt > 0.0 {
                let dir = dir.normalize();
                let speed = run.stats().speed * run.haste_mult();
                pos += dir * speed * dt;
                mv.speed = speed;
                mv.flags = match face {
                    Some(look) => gait(look, dir),
                    None => {
                        let target = Quat::from_rotation_y(yaw_toward(dir));
                        tf.rotation = tf.rotation.slerp(target, (dt * 20.0).min(1.0));
                        move_flags::FORWARD
                    }
                };
            } else if mv.speed != 0.0 || mv.flags != 0 {
                mv.speed = 0.0;
                mv.flags = 0;
            }
            let map = selection.stage_def();
            let center = arena(map);
            let off = flat(pos - center);
            if off.length() > map.leash * 2.0 {
                // Just entered another battleground: start at its centre.
                pos = center + spawn_offset(net.local_id);
            } else if off.length() > map.leash {
                let held = center + off.normalize() * map.leash;
                pos.x = held.x;
                pos.z = held.z;
            }
            let ground = world.terrain_height_under(pos).unwrap_or(pos.y);
            let playing = *phase.get() == Phase::Playing && !local_down;
            // Space hops: lift off the terrain, then gravity brings the hero back down.
            if playing && keys.just_pressed(KeyCode::Space) && hm.vy == 0.0 && hm.air == 0.0 {
                hm.vy = JUMP_SPEED;
                net.latched_buttons |= BTN_JUMP;
            }
            if playing && (hm.vy != 0.0 || hm.air > 0.0) {
                hm.vy -= GRAVITY * dt;
                hm.air += hm.vy * dt;
                if hm.air <= 0.0 {
                    hm.air = 0.0;
                    hm.vy = 0.0;
                }
            } else {
                hm.air = 0.0;
                hm.vy = 0.0;
            }
            pos.y = ground + hm.air;
            if let Some(ref mut ph) = ph {
                ph.vy = hm.vy;
                ph.air = hm.air;
            }
            // Feed the animation driver: `vertical_speed`'s sign and the `FALLING` flag are what
            // select the Jump (37/38) / Fall (40) animation (`creature_anim::select`).
            mv.vertical_speed = hm.vy;
            if hm.vy != 0.0 {
                mv.flags |= move_flags::FALLING;
            }
            if tf.translation != pos {
                tf.translation = pos;
            }
            pos
        }
        Err(_) => {
            let mut p = arena(selection.stage_def());
            if let Some(h) = world.terrain_height_under(p) {
                p.y = h;
            }
            p
        }
    };
    // The terrain streams and the loading screen waits on the ground under this.
    player.active = true;
    player.detached = false;
    player.settling = false;
    player.world_stale = false;
    player.pos = focus;
}

/// Let go of the mouse buttons when the page loses focus or the pointer leaves it.
pub(super) fn drop_stray_buttons(
    mut focus: MessageReader<bevy::window::WindowFocused>,
    mut left: MessageReader<bevy::window::CursorLeft>,
    mut mouse: ResMut<ButtonInput<MouseButton>>,
) {
    let lost = focus.read().any(|f| !f.focused);
    if lost | (left.read().count() > 0) {
        mouse.release_all();
    }
}

/// A slow orbit at the menu; in play, top-down until `C` unlocks it to swing and tilt.
pub(super) fn place_camera(
    time: Res<Time<Real>>,
    phase: Res<State<Phase>>,
    mut rig: ResMut<CamRig>,
    mut wheel: MessageReader<MouseWheel>,
    mouse: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    keys: Res<ButtonInput<KeyCode>>,
    player: Res<crate::player::Player>,
    world: WorldPoint,
    hero: Query<&Transform, (With<Hero>, Without<WorldCamera>)>,
    mut cams: Query<&mut Transform, With<WorldCamera>>,
) {
    if keys.just_pressed(KeyCode::KeyC) {
        rig.mode = match rig.mode {
            CamMode::Fixed => CamMode::Orbit,
            CamMode::Orbit => {
                rig.yaw = 0.0;
                rig.pitch = FIXED_PITCH;
                CamMode::Fixed
            }
        };
    }
    let focus = hero.single().map_or(player.pos, |t| t.translation);
    for w in wheel.read() {
        let notches = match w.unit {
            MouseScrollUnit::Line => w.y,
            MouseScrollUnit::Pixel => w.y * 0.02,
        };
        rig.dist = (rig.dist * (1.0 - 0.12 * notches).clamp(0.5, 1.5)).clamp(ZOOM_MIN, ZOOM_MAX);
    }
    let (eye, look) = if *phase.get() == Phase::Menu {
        rig.orbit += time.delta_secs() * 0.2;
        let d = 7.0;
        (
            focus + Vec3::new(rig.orbit.sin() * d, 2.4, rig.orbit.cos() * d),
            focus + Vec3::Y * 1.1,
        )
    } else {
        if rig.mode == CamMode::Orbit && mouse.any_pressed([MouseButton::Left, MouseButton::Right])
        {
            rig.yaw -= motion.delta.x * ORBIT_RATE;
            rig.pitch = (rig.pitch + motion.delta.y * ORBIT_RATE).clamp(PITCH_MIN, PITCH_MAX);
        }
        let (sy, cy) = rig.yaw.sin_cos();
        let (sp, cp) = rig.pitch.sin_cos();
        let look = focus + Vec3::Y * 1.4;
        let mut eye = look + Vec3::new(cp * sy, sp, cp * cy) * rig.dist;
        if let Some(h) = world.terrain_height_under(eye) {
            eye.y = eye.y.max(h + 0.6);
        }
        (eye, look)
    };
    let placed = Transform::from_translation(eye).looking_at(look, Vec3::Y);
    for mut cam in &mut cams {
        *cam = placed;
    }
}

/// Damage a specific party member takes from one blow, after immunity, armour and shield.
pub(super) fn hurt_combatant(c: &mut super::Combatant, raw: f32) -> f32 {
    if c.immune_t > 0.0 || c.is_down() {
        return 0.0;
    }
    let mut dmg = (raw - c.stats().armor).max(1.0);
    if c.shield_t > 0.0 && c.shield > 0.0 {
        let absorbed = dmg.min(c.shield);
        c.shield -= absorbed;
        dmg -= absorbed;
    }
    c.hp = (c.hp - dmg).max(0.0);
    dmg
}

/// Damage the local hero takes (singleplayer fallback / sync helper).
pub(super) fn hurt_hero(run: &mut Run, raw: f32) -> f32 {
    if run.immune_t > 0.0 || run.dead_t.is_some() {
        return 0.0;
    }
    let mut dmg = (raw - run.stats().armor).max(1.0);
    if run.shield_t > 0.0 && run.shield > 0.0 {
        let absorbed = dmg.min(run.shield);
        run.shield -= absorbed;
        dmg -= absorbed;
    }
    run.hp = (run.hp - dmg).max(0.0);
    dmg
}

/// The clock, regeneration, timed buffs & state visuals, downed state, teammate proximity revive,
/// party wipe death, and victory.
#[allow(clippy::too_many_arguments)]
pub(super) fn hero_vitals(
    time: Res<Time>,
    mut run: ResMut<Run>,
    mut net: ResMut<NetSession>,
    mut next: ResMut<NextState<Phase>>,
    mut party_heroes: Query<(Entity, &PartyHero, &Transform, &Guid, &mut ObjectStore, &mut Marks)>,
    mut fields: MessageWriter<FieldChanged>,
    mut fx: Fx,
) {
    if net.is_client() {
        return;
    }
    let dt = time.delta_secs();
    if let Some(t) = run.dead_t.as_mut() {
        *t += dt;
        if *t > 3.0 {
            next.set(Phase::GameOver);
            if let Some(host) = &net.host {
                host.broadcast_reliable(&super::net::ReliableMsg::SetPhase {
                    phase: super::net::NetPhase::GameOver,
                    won: false,
                });
            }
        }
        return;
    }

    run.elapsed += dt;

    // If `run.players` is empty (e.g. legacy singleplayer path), keep top-level `run` synced into slot 0.
    if run.players.is_empty() {
        let c = super::Combatant::new(0, "Hero".into(), run.class, 1300, run.race, 0);
        run.players.push(c);
        let local_id = net.local_id;
        run.sync_combatant_from_local(local_id);
    } else if net.role == NetRole::Offline {
        let local_id = net.local_id;
        run.sync_combatant_from_local(local_id);
    }

    // Gather positions of all party heroes for proximity revive checks.
    let positions: Vec<(PlayerId, Entity, Vec3)> = party_heroes
        .iter()
        .map(|(e, ph, tf, ..)| (ph.id, e, tf.translation))
        .collect();

    let multi = run.players.len() > 1;
    let mut newly_downed: Vec<String> = Vec::new();
    let mut newly_revived: Vec<(PlayerId, String)> = Vec::new();

    // First pass: tick vitals, buffs, and check downed status.
    for i in 0..run.players.len() {
        let stats = run.players[i].stats();
        let c = &mut run.players[i];
        if c.is_down() {
            continue;
        }
        for t in [&mut c.shield_t, &mut c.immune_t, &mut c.thorns_t] {
            *t = (*t - dt).max(0.0);
        }
        let mut heal = stats.regen * dt;
        if c.heal_t > 0.0 {
            heal += c.heal_rate * dt.min(c.heal_t);
            c.heal_t = (c.heal_t - dt).max(0.0);
        }
        c.hp = (c.hp + heal).min(stats.max_hp);
        if c.hp <= 0.0 {
            c.hp = 0.0;
            c.downed_revive = Some(0.0);
            c.shield = 0.0;
            c.shield_t = 0.0;
            c.thorns_t = 0.0;
            c.heal_t = 0.0;
            if multi {
                newly_downed.push(c.name.clone());
            }
        }
    }

    // Second pass: check proximity revives for downed players when at least one teammate is alive.
    if multi {
        let alive_positions: Vec<(PlayerId, Vec3)> = positions
            .iter()
            .filter(|&&(id, _, _)| run.player(id).is_some_and(|c| !c.is_down()))
            .map(|&(id, _, pos)| (id, pos))
            .collect();

        for c in &mut run.players {
            let max_hp = c.stats().max_hp;
            if let Some(ref mut progress) = c.downed_revive {
                let my_pos = positions
                    .iter()
                    .find(|&&(id, _, _)| id == c.id)
                    .map(|&(_, _, p)| p);
                let rescuer_near = my_pos.is_some_and(|mp| {
                    alive_positions
                        .iter()
                        .any(|&(_, ap)| flat(ap - mp).length() <= REVIVE_RADIUS)
                });
                if rescuer_near {
                    *progress += dt;
                    if *progress >= REVIVE_SECS {
                        c.downed_revive = None;
                        c.hp = max_hp * 0.5;
                        c.immune_t = 3.0;
                        newly_revived.push((c.id, c.name.clone()));
                    }
                } else {
                    *progress = (*progress - dt * 0.5).max(0.0);
                }
            }
        }
    }

    // Update unit visual state (death pose vs live pose + aura marks) on each party hero entity.
    for (entity, ph, _, guid, mut store, mut marks) in &mut party_heroes {
        let Some(c) = run.player(ph.id) else { continue };
        if c.is_down() {
            units::kill_unit(&mut store);
            marks.clear();
            marks.flush(entity, guid.0, &mut store, &mut fields);
        } else {
            units::revive_unit(&mut store);
            let mut shown: Vec<u32> = Vec::new();
            if c.shield_t > 0.0 && c.shield > 0.0 {
                shown.push(17);
            }
            if c.immune_t > 0.0 {
                shown.push(642);
            }
            if c.thorns_t > 0.0 {
                shown.push(c.thorns_spell);
            }
            if c.heal_t > 0.0 {
                shown.push(774);
            }
            for owned in &c.abilities {
                if owned.def.key == "lightning_shield" {
                    shown.push(owned.def.spell);
                }
            }
            if c.passives.iter().any(|(p, _)| p.key == "renew") {
                shown.push(139);
            }
            marks.set(&shown);
            marks.flush(entity, guid.0, &mut store, &mut fields);
        }
    }

    for (rev_id, rev_name) in newly_revived {
        let msg = format!("{rev_name} has been revived!");
        run.banner = Some((msg.clone(), 3.0));
        if let Some(host) = &net.host {
            host.broadcast_reliable(&super::net::ReliableMsg::Banner {
                text: msg,
                secs: 3.0,
            });
        }
        if let Some(&(_, ent, _)) = positions.iter().find(|&&(id, _, _)| id == rev_id) {
            fx.cast(&mut net, ent, 2006, &[ent], None);
            if let Some(c) = run.player(rev_id) {
                fx.number(&mut net, ent, c.hp, false, COLOR_HEAL);
            }
        }
    }

    for down_name in newly_downed {
        let msg = format!("{down_name} is down! Stand nearby to revive!");
        run.banner = Some((msg.clone(), 3.5));
        if let Some(host) = &net.host {
            host.broadcast_reliable(&super::net::ReliableMsg::Banner {
                text: msg,
                secs: 3.5,
            });
        }
    }

    // Sync local player's combatant back to top-level `Run` fields for HUD.
    let local_id = net.local_id;
    run.sync_local_from_combatant(local_id);

    // Check if ALL party members are down -> Wipe (GameOver after 3s death animation).
    let all_down = !run.players.is_empty() && run.players.iter().all(|c| c.is_down());
    if all_down {
        run.hp = 0.0;
        run.dead_t = Some(0.0);
    } else if run.elapsed >= data::VICTORY_SECS {
        run.won = true;
        for &(_, ent, _) in &positions {
            fx.cast(&mut net, ent, 642, &[ent], None);
        }
        next.set(Phase::GameOver);
        if let Some(host) = &net.host {
            host.broadcast_reliable(&super::net::ReliableMsg::SetPhase {
                phase: super::net::NetPhase::GameOver,
                won: true,
            });
        }
    }
}

/// The movement flags for running `dir` while facing `look`: forward, backpedal or strafe, by
/// which quarter the run falls in, so the walk, backpedal and strafe animations play.
pub(super) fn gait(look: Vec3, dir: Vec3) -> u32 {
    let ahead = look.dot(dir);
    let left = look.cross(dir).y;
    let quarter = std::f32::consts::FRAC_1_SQRT_2;
    if ahead >= quarter {
        move_flags::FORWARD
    } else if ahead <= -quarter {
        move_flags::BACKWARD
    } else if left > 0.0 {
        move_flags::STRAFE_LEFT
    } else {
        move_flags::STRAFE_RIGHT
    }
}

pub(super) fn reach(hero: Vec3, other: Vec3) -> f32 {
    flat(other - hero).length()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::window::{CursorLeft, WindowFocused};

    fn held_left() -> World {
        let mut world = World::new();
        world.init_resource::<Messages<WindowFocused>>();
        world.init_resource::<Messages<CursorLeft>>();
        let mut mouse = ButtonInput::<MouseButton>::default();
        mouse.press(MouseButton::Left);
        world.insert_resource(mouse);
        world
    }

    fn left_held(world: &World) -> bool {
        world.resource::<ButtonInput<MouseButton>>().pressed(MouseButton::Left)
    }

    #[test]
    fn a_held_button_is_dropped_when_the_page_loses_focus() {
        let mut world = held_left();
        world.run_system_once(drop_stray_buttons).unwrap();
        assert!(left_held(&world), "nothing happened: the drag goes on");
        world.write_message(WindowFocused { window: Entity::PLACEHOLDER, focused: true });
        world.run_system_once(drop_stray_buttons).unwrap();
        assert!(left_held(&world), "gaining focus is no release");
        world.write_message(WindowFocused { window: Entity::PLACEHOLDER, focused: false });
        world.run_system_once(drop_stray_buttons).unwrap();
        assert!(!left_held(&world));
    }

    #[test]
    fn a_held_button_is_dropped_when_the_pointer_leaves_the_page() {
        let mut world = held_left();
        world.write_message(CursorLeft { window: Entity::PLACEHOLDER });
        world.run_system_once(drop_stray_buttons).unwrap();
        assert!(!left_held(&world));
    }
}
