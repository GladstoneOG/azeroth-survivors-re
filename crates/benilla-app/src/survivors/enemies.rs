//! The horde: spawning on a ring outside the view, the chase, contact damage, death, and the
//! experience gems the dead leave. Scales wave counts and HP for co-op parties, and targets the
//! nearest living party member.

use std::collections::HashMap;

use benilla_world::rig_anim::GlobalSeqDrive;
use benilla_world::world_point::WorldPoint;
use bevy::prelude::*;

use super::abilities::Hit;
use super::data::{self, EnemyDef};
use super::hero::{self, PartyHero, HERO_RADIUS};
use super::net::{NetSession, PlayerId};
use super::units::{self, flat, yaw_toward, Fx, Marks, UnitSpec, COLOR_HURT};
use super::{NetId, Phase, Rng, Run, Selection};
use crate::creature_anim::{move_flags, MovementState};
use crate::net::{FieldChanged, Guid, ObjectStore};

#[derive(Component)]
pub(super) struct Enemy {
    pub def: &'static EnemyDef,
    pub hp: f32,
    pub max_hp: f32,
    pub speed: f32,
    pub damage: f32,
    pub radius: f32,
    pub xp: u32,
    pub boss: bool,
    pub attack_cd: f32,
    pub slow: f32,
    pub slow_t: f32,
    pub root_t: f32,
    pub stun_t: f32,
    pub fear_t: f32,
    pub weaken: f32,
    pub weaken_t: f32,
    pub dots: Vec<Dot>,
    pub dead: bool,
}

pub(super) struct Dot {
    pub dps: f32,
    pub left: f32,
    pub tick: f32,
}

/// A corpse playing its death before it fades.
#[derive(Component)]
pub(super) struct Dying(f32);

#[derive(Resource, Default)]
pub(super) struct Spawner {
    timer: f32,
}

/// An enemy died here, worth this much.
#[derive(Message, Clone, Copy)]
pub(super) struct Loot {
    pub pos: Vec3,
    pub xp: u32,
    pub boss: bool,
}

#[derive(Clone, Copy)]
pub(super) enum GemKind {
    Xp(u32),
    Heal(f32),
}

#[derive(Component)]
pub(super) struct Gem {
    pub(super) kind: GemKind,
    pub(super) display: u32,
    pub(super) scale: f32,
    pub(super) pulled_by: Option<PlayerId>,
    pub(super) speed: f32,
}

/// The `GameObjectDisplayInfo` row and scale a pickup wears: a wisp per experience tier, the
/// bigger tiers rarer, and a turkey leg for health.
pub(super) fn look_for(kind: GemKind) -> (u32, f32) {
    match kind {
        // `TurkeyLeg.mdx`.
        GemKind::Heal(_) => (114, 2.5),
        // `BFD_WispSmallPurple.mdx`.
        GemKind::Xp(xp) if xp >= 20 => (1307, 1.0),
        // `BFD_WispSmallGreen.mdx`.
        GemKind::Xp(xp) if xp >= 5 => (1267, 1.0),
        // `BFD_WispSmall.mdx`.
        GemKind::Xp(_) => (426, 1.0),
    }
}

/// Body radius: the big beasts need more room.
pub(super) fn radius_for(def: &EnemyDef) -> f32 {
    (if def.big { 1.3 } else { 0.6 }) * def.scale
}

fn spawn_enemy(
    commands: &mut Commands,
    run: &mut Run,
    def: &'static EnemyDef,
    pos: Vec3,
    hero: Vec3,
    boss: bool,
    extra_players: usize,
) {
    let mp_hp_mult = 1.0 + 0.5 * (extra_players as f32);
    let scale = data::hp_scale(run.elapsed) * mp_hp_mult;
    let serial = run.guid();
    let net_id = run.alloc_net_id();
    let e = units::spawn_unit(
        commands,
        &UnitSpec {
            serial,
            display: def.display,
            scale: def.scale,
            level: def.level,
            faction: units::FACTION_MONSTER,
            bytes0: 1 << 8,
            weapons: &[],
            sheath: 0,
        },
        pos,
        yaw_toward(hero - pos),
    );
    let hp = def.hp * scale;
    commands.entity(e).insert((
        NetId(net_id),
        Enemy {
            def,
            hp,
            max_hp: hp,
            speed: def.speed,
            damage: def.damage * data::damage_scale(run.elapsed),
            radius: radius_for(def),
            xp: def.xp,
            boss,
            attack_cd: 0.5,
            slow: 1.0,
            slow_t: 0.0,
            root_t: 0.0,
            stun_t: 0.0,
            fear_t: 0.0,
            weaken: 1.0,
            weaken_t: 0.0,
            dots: Vec::new(),
            dead: false,
        },
        Marks::debuffs(),
    ));
}

fn ground(world: &WorldPoint, mut p: Vec3, fallback: f32) -> Vec3 {
    p.y = world.terrain_height_under(p).unwrap_or(fallback);
    p
}

/// Keep the horde topped up on a ring outside the view of living players, with the bosses and the
/// encircling waves on the clock. Scales enemy HP (+50% per extra player) and cap (+35% per extra player).
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_enemies(
    mut commands: Commands,
    time: Res<Time>,
    mut run: ResMut<Run>,
    net: Res<NetSession>,
    mut rng: ResMut<Rng>,
    mut spawner: ResMut<Spawner>,
    selection: Res<Selection>,
    party_heroes: Query<(&PartyHero, &Transform)>,
    enemies: Query<&Enemy>,
    world: WorldPoint,
) {
    if net.is_client() || run.dead_t.is_some() {
        return;
    }
    let living_positions: Vec<Vec3> = party_heroes
        .iter()
        .filter(|(ph, _)| run.player(ph.id).is_none_or(|c| !c.is_down()))
        .map(|(_, tf)| tf.translation)
        .collect();
    if living_positions.is_empty() {
        return;
    }
    let extra_players = net.player_count().saturating_sub(1);
    let elapsed = run.elapsed;
    let roster = data::roster(selection.map);
    let pool = data::wave_pool(elapsed, roster.enemies.len());

    let due_bosses = ((elapsed / data::BOSS_EVERY_SECS) as u32).min(roster.bosses.len() as u32);
    while run.bosses < due_bosses {
        let def = &roster.bosses[run.bosses as usize];
        let hpos = living_positions[rng.index(living_positions.len())];
        let a = rng.range(0.0, std::f32::consts::TAU);
        let pos = ground(
            &world,
            hpos + Vec3::new(a.cos(), 0.0, a.sin()) * 30.0,
            hpos.y,
        );
        spawn_enemy(&mut commands, &mut run, def, pos, hpos, true, extra_players);
        run.bosses += 1;
        let msg = format!("{} approaches!", def.name);
        run.banner = Some((msg.clone(), 4.0));
        if let Some(host) = &net.host {
            host.broadcast_reliable(&super::net::ReliableMsg::Banner {
                text: msg,
                secs: 4.0,
            });
        }
    }

    // Half way between bosses, a closing ring of the moment's weakest.
    let due_rings = ((elapsed + data::BOSS_EVERY_SECS / 2.0) / data::BOSS_EVERY_SECS) as u32;
    if run.rings < due_rings {
        run.rings = due_rings;
        let def = &roster.enemies[pool[0]];
        let hpos = living_positions[rng.index(living_positions.len())];
        let n = 28 + (data::progress(elapsed) * 60.0) as usize;
        for i in 0..n {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            let pos = ground(
                &world,
                hpos + Vec3::new(a.cos(), 0.0, a.sin()) * 26.0,
                hpos.y,
            );
            spawn_enemy(&mut commands, &mut run, def, pos, hpos, false, extra_players);
        }
        let msg = "They're surrounding you!".to_string();
        run.banner = Some((msg.clone(), 3.0));
        if let Some(host) = &net.host {
            host.broadcast_reliable(&super::net::ReliableMsg::Banner {
                text: msg,
                secs: 3.0,
            });
        }
    }

    spawner.timer -= time.delta_secs();
    if spawner.timer > 0.0 {
        return;
    }
    spawner.timer = 0.12;
    let alive = enemies.iter().filter(|e| !e.dead).count();
    let count_mult = 1.0 + 0.35 * (extra_players as f32);
    let target = ((data::target_alive(elapsed) as f32) * count_mult) as usize;
    if alive >= target {
        return;
    }
    let n = (((target - alive) as f32) / 6.0).ceil().min(4.0 + extra_players as f32) as usize;
    for _ in 0..n {
        let def = &roster.enemies[pool[rng.index(pool.len())]];
        let hpos = living_positions[rng.index(living_positions.len())];
        let a = rng.range(0.0, std::f32::consts::TAU);
        let r = rng.range(34.0, 42.0);
        let pos = ground(&world, hpos + Vec3::new(a.cos(), 0.0, a.sin()) * r, hpos.y);
        spawn_enemy(&mut commands, &mut run, def, pos, hpos, false, extra_players);
    }
}

/// The chase: straight at the nearest non-downed, non-vanished party member, pushed apart from
/// each other, stopped by crowd control, and swinging when in reach.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(super) fn move_enemies(
    time: Res<Time>,
    mut run: ResMut<Run>,
    mut net: ResMut<NetSession>,
    world: WorldPoint,
    party_heroes: Query<(Entity, &PartyHero, &Transform), Without<Enemy>>,
    mut enemies: Query<(Entity, &mut Transform, &mut Enemy, &mut MovementState), Without<PartyHero>>,
    mut fx: Fx,
    mut hits: MessageWriter<Hit>,
) {
    if net.is_client() {
        return;
    }
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let hero_down = run.dead_t.is_some();

    // Sync local player's state into `run.players` before combat calculations.
    if net.role == super::net::NetRole::Offline {
        let local_id = net.local_id;
        run.sync_combatant_from_local(local_id);
    }

    // Living party targets: `(PlayerId, Entity, pos, vanished)`.
    let targets: Vec<(PlayerId, Entity, Vec3, bool)> = party_heroes
        .iter()
        .filter_map(|(ent, ph, tf)| {
            let c = run.player(ph.id);
            if c.is_some_and(|c| c.is_down()) {
                None
            } else {
                let vanished = c.map_or(run.mobile.vanish_t > 0.0, |c| c.mobile.vanish_t > 0.0);
                Some((ph.id, ent, tf.translation, vanished))
            }
        })
        .collect();

    if targets.is_empty() && !hero_down {
        return;
    }

    // Separation over a coarse grid: only neighbours in the same or adjacent cells push.
    const CELL: f32 = 3.0;
    let bodies: Vec<(Entity, Vec3, f32)> = enemies
        .iter()
        .filter(|(_, _, e, _)| !e.dead)
        .map(|(ent, tf, e, _)| (ent, tf.translation, e.radius))
        .collect();
    let key = |p: Vec3| ((p.x / CELL).floor() as i32, (p.z / CELL).floor() as i32);
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, b) in bodies.iter().enumerate() {
        grid.entry(key(b.1)).or_default().push(i);
    }
    let mut push: HashMap<Entity, Vec3> = HashMap::with_capacity(bodies.len());
    for (i, &(ent, pos, r)) in bodies.iter().enumerate() {
        let (cx, cz) = key(pos);
        let mut acc = Vec3::ZERO;
        for dx in -1..=1 {
            for dz in -1..=1 {
                let Some(cell) = grid.get(&(cx + dx, cz + dz)) else {
                    continue;
                };
                for &j in cell {
                    if j == i {
                        continue;
                    }
                    let (_, other, or) = bodies[j];
                    let d = flat(pos - other);
                    let len = d.length();
                    let min = r + or;
                    if len < min && len > 1e-4 {
                        acc += d / len * (min - len);
                    } else if len <= 1e-4 {
                        acc += Vec3::new((i as f32).sin(), 0.0, (i as f32).cos()) * 0.1;
                    }
                }
            }
        }
        push.insert(ent, acc);
    }

    for (ent, mut tf, mut e, mut mv) in &mut enemies {
        let e = &mut *e;
        if e.dead {
            if mv.speed != 0.0 || mv.flags != 0 {
                mv.speed = 0.0;
                mv.flags = 0;
            }
            continue;
        }
        e.attack_cd -= dt;
        for t in [
            &mut e.slow_t,
            &mut e.root_t,
            &mut e.stun_t,
            &mut e.fear_t,
            &mut e.weaken_t,
        ] {
            *t = (*t - dt).max(0.0);
        }
        if e.slow_t <= 0.0 {
            e.slow = 1.0;
        }
        if e.weaken_t <= 0.0 {
            e.weaken = 1.0;
        }
        let pos = tf.translation;

        // Pick the nearest visible living player (or nearest overall if all vanished / boss).
        let chosen = targets
            .iter()
            .filter(|&&(_, _, _, vanished)| !vanished || e.boss)
            .min_by(|a, b| flat(a.2 - pos).length().total_cmp(&flat(b.2 - pos).length()))
            .or_else(|| {
                targets
                    .iter()
                    .min_by(|a, b| flat(a.2 - pos).length().total_cmp(&flat(b.2 - pos).length()))
            })
            .copied();

        let Some((target_pid, hero_e, hpos, target_vanished)) = chosen else {
            mv.speed = 0.0;
            mv.flags = 0;
            continue;
        };

        let to_hero = flat(hpos - pos);
        let dist = to_hero.length();

        // Strays far behind every living player are brought back in front of their target.
        let min_any_dist = targets
            .iter()
            .map(|t| flat(t.2 - pos).length())
            .fold(f32::INFINITY, f32::min);
        if min_any_dist > 75.0 {
            let back = hpos + to_hero.normalize_or_zero() * 38.0;
            tf.translation = ground(&world, back, hpos.y);
            continue;
        }

        // Under Fade the horde has lost the hero: it mills where it stands.
        let lost = target_vanished && !e.boss;
        let held = e.stun_t > 0.0 || e.root_t > 0.0 || lost;
        let contact = dist <= HERO_RADIUS + e.radius + 0.35;
        let mut step = Vec3::ZERO;
        let mut speed = 0.0;
        if !held && !hero_down {
            let dir = if e.fear_t > 0.0 {
                -to_hero.normalize_or_zero()
            } else {
                to_hero.normalize_or_zero()
            };
            if !contact || e.fear_t > 0.0 {
                speed = e.speed * e.slow;
                step = dir * speed;
            }
            if dir != Vec3::ZERO {
                let target = Quat::from_rotation_y(yaw_toward(dir));
                tf.rotation = tf.rotation.slerp(target, (dt * 10.0).min(1.0));
            }
        }
        let shove = push.get(&ent).copied().unwrap_or_default();
        let mut next = pos + step * dt + shove * (dt * 8.0).min(0.5);
        next = ground(&world, next, pos.y);
        tf.translation = next;
        if speed > 0.0 {
            mv.speed = speed;
            mv.flags = move_flags::FORWARD;
        } else if mv.speed != 0.0 || mv.flags != 0 {
            mv.speed = 0.0;
            mv.flags = 0;
        }

        if contact
            && !lost
            && e.stun_t <= 0.0
            && e.fear_t <= 0.0
            && e.attack_cd <= 0.0
            && !hero_down
        {
            e.attack_cd = if e.boss { 1.0 } else { 1.3 };
            let raw = e.damage * e.weaken;
            let (taken, thorns_dmg) = if let Some(c) = run.player_mut(target_pid) {
                let t = hero::hurt_combatant(c, raw);
                let th = if c.thorns_t > 0.0 {
                    Some(c.thorns * c.stats().might)
                } else {
                    None
                };
                (t, th)
            } else {
                let t = hero::hurt_hero(&mut run, raw);
                let th = if run.thorns_t > 0.0 {
                    Some(run.thorns * run.stats().might)
                } else {
                    None
                };
                (t, th)
            };
            fx.swing(&mut net, ent, hero_e, taken);
            if taken > 0.0 {
                fx.number(&mut net, hero_e, taken, false, COLOR_HURT);
            }
            if let Some(th) = thorns_dmg {
                hits.write(Hit::direct(ent, th, false));
            }
        }
    }

    let local_id = net.local_id;
    run.sync_local_from_combatant(local_id);
}

/// Mark timers run down; the state kits reap with them.
pub(super) fn tick_marks(
    time: Res<Time>,
    net: Res<NetSession>,
    mut units: Query<(Entity, &Guid, &mut ObjectStore, &mut Marks), With<Enemy>>,
    mut fields: MessageWriter<FieldChanged>,
) {
    if net.is_client() {
        return;
    }
    let dt = time.delta_secs();
    for (entity, guid, mut store, mut marks) in &mut units {
        marks.tick(dt);
        marks.flush(entity, guid.0, &mut store, &mut fields);
    }
}

/// Corpses fade once their death has played.
pub(super) fn reap_dead(
    mut commands: Commands,
    time: Res<Time>,
    mut dying: Query<(Entity, &mut Dying)>,
) {
    for (e, mut d) in &mut dying {
        d.0 += time.delta_secs();
        if d.0 > 2.5 {
            commands
                .entity(e)
                .remove::<Dying>()
                .insert(benilla_world::model_fade::DespawnFade::default());
        }
    }
}

pub(super) fn mark_dying(commands: &mut Commands, e: Entity) {
    commands.entity(e).insert(Dying(0.0));
}

/// Most gems on the ground at once; past it, a kill's worth joins an existing gem.
const MAX_GEMS: usize = 350;

pub(super) fn drop_loot(
    mut commands: Commands,
    net: Res<NetSession>,
    mut drops: MessageReader<Loot>,
    mut rng: ResMut<Rng>,
    mut run: ResMut<Run>,
    mut gems: Query<&mut Gem>,
) {
    if net.is_client() {
        return;
    }
    let mut count = gems.iter().count();
    for d in drops.read() {
        if d.boss || rng.f32() < 0.015 {
            let heal = GemKind::Heal(if d.boss { 1000.0 } else { 30.0 });
            let serial = run.guid();
            let net_id = run.alloc_net_id();
            spawn_gem(&mut commands, serial, net_id, d.pos + Vec3::X * 0.8, heal);
        }
        if count >= MAX_GEMS && !d.boss {
            if let Some(mut g) = gems.iter_mut().find(|g| matches!(g.kind, GemKind::Xp(_))) {
                if let GemKind::Xp(xp) = &mut g.kind {
                    *xp += d.xp;
                }
                continue;
            }
        }
        let serial = run.guid();
        let net_id = run.alloc_net_id();
        spawn_gem(&mut commands, serial, net_id, d.pos, GemKind::Xp(d.xp));
        count += 1;
    }
}

pub(super) fn spawn_gem(
    commands: &mut Commands,
    serial: u64,
    net_id: u32,
    pos: Vec3,
    kind: GemKind,
) -> Entity {
    let (display, scale) = look_for(kind);
    let e = units::spawn_object(commands, serial, display, scale, pos + Vec3::Y * 0.5);
    commands.entity(e).insert((
        NetId(net_id),
        Gem {
            kind,
            display,
            scale,
            pulled_by: None,
            speed: 6.0,
        },
    ));
    e
}

/// Gems in any living party member's magnet reach fly to that hero; XP banks into shared team XP!
pub(super) fn collect_gems(
    mut commands: Commands,
    time: Res<Time>,
    mut run: ResMut<Run>,
    net: Res<NetSession>,
    mut next: ResMut<NextState<Phase>>,
    party_heroes: Query<(&PartyHero, &Transform), Without<Gem>>,
    mut gems: Query<(Entity, &mut Transform, &mut Gem)>,
) {
    if net.is_client() || run.dead_t.is_some() {
        return;
    }
    let dt = time.delta_secs();
    let collectors: Vec<(PlayerId, Vec3, super::Stats)> = party_heroes
        .iter()
        .filter_map(|(ph, tf)| {
            let c = run.player(ph.id);
            if c.is_some_and(|c| c.is_down()) {
                None
            } else {
                let st = c.map_or_else(|| run.stats(), |c| c.stats());
                Some((ph.id, tf.translation + Vec3::Y * 1.0, st))
            }
        })
        .collect();
    if collectors.is_empty() {
        return;
    }

    for (e, mut tf, mut g) in &mut gems {
        if g.pulled_by.is_none() {
            for &(pid, target_pos, st) in &collectors {
                if flat(target_pos - tf.translation).length() <= st.magnet {
                    g.pulled_by = Some(pid);
                    break;
                }
            }
        }
        let Some(pid) = g.pulled_by else {
            continue;
        };
        let Some(&(_, target, st)) = collectors
            .iter()
            .find(|&&(id, _, _)| id == pid)
            .or_else(|| collectors.first())
        else {
            continue;
        };
        let d = target - tf.translation;
        let dist = d.length();
        if dist < 0.9 {
            match g.kind {
                GemKind::Xp(xp) => run.xp += xp as f32 * data::XP_RATE * st.growth,
                GemKind::Heal(hp) => {
                    if let Some(c) = run.player_mut(pid) {
                        let max = c.stats().max_hp;
                        c.hp = (c.hp + hp).min(max);
                    } else {
                        let max = run.stats().max_hp;
                        run.hp = (run.hp + hp).min(max);
                    }
                }
            }
            commands.entity(e).despawn();
            continue;
        }
        g.speed += 30.0 * dt;
        tf.translation += d / dist * (g.speed * dt).min(dist);
    }

    let local_id = net.local_id;
    run.sync_local_from_combatant(local_id);

    while run.xp >= data::xp_to_next(run.level) {
        run.xp -= data::xp_to_next(run.level);
        run.level += 1;
        run.pending_levels += 1;
        if run.level == super::mobility::LEARN_LEVEL {
            if let Some(m) = super::mobility::for_class(run.class.id) {
                run.banner = Some((format!("You have learned {}! Press F.", m.name), 4.0));
            }
        }
    }
    if run.pending_levels > 0 {
        next.set(Phase::LevelUp);
    }
}

/// Pickups hang still: a wisp's own loop (its circling motes and their trails) stops on its first
/// frame, clip and global sequences alike, as soon as the model attaches.
pub(super) fn still_gems(
    mut players: Query<&mut AnimationPlayer, (With<Gem>, Added<AnimationPlayer>)>,
    mut drives: Query<&mut GlobalSeqDrive, (With<Gem>, Added<GlobalSeqDrive>)>,
) {
    for mut player in &mut players {
        for (_, active) in player.playing_animations_mut() {
            active.set_speed(0.0);
        }
    }
    for mut drive in &mut drives {
        drive.set_paused(true);
    }
}
