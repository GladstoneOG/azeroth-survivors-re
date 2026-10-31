//! Warcraft Survivors: a top-down survival mode on the benilla world, started by
//! [`crate::run_survivors`]. Supports offline singleplayer, in-game Listen-Server LAN hosting,
//! LAN client joining (via ZeroTier/Tailscale UDP+TCP), and headless dedicated server mode.

mod abilities;
mod auto;
mod cards;
mod data;
mod enemies;
mod fill;
mod grade;
mod hero;
mod hud;
mod menu;
mod minimap;
mod mobility;
pub(crate) mod net;
mod sync;
mod units;

use benilla_world::schedule::WorldStage;
use bevy::prelude::*;

use data::{AbilityDef, ClassDef, PassiveDef, Stat};
use net::{NetSession, PlayerId};

/// Set by [`crate::run_survivors`] before the app builds. wasm32 has no process environment, so
/// the flag lives here; `WOW_SURVIVORS` (`?survivors=1` / `__wenilla_env.survivors` on the web)
/// still turns it on from outside.
pub(crate) static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn active() -> bool {
    ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
        || crate::webenv::var("WOW_SURVIVORS").is_some()
}

pub(crate) struct SurvivorsPlugin;

impl Plugin for SurvivorsPlugin {
    fn build(&self, app: &mut App) {
        // Present before `OnEnter(InWorld)`, where the UI lifecycle decides not to load FrameXML.
        app.insert_resource(crate::run_mode::CaptureMode)
            .add_plugins(grade::plugin)
            .init_state::<Phase>()
            .init_resource::<Rng>()
            .init_resource::<Selection>()
            .init_resource::<Run>()
            .init_resource::<NetSession>()
            .init_resource::<hero::CamRig>()
            .init_resource::<hero::AimPoint>()
            .init_resource::<abilities::PendingHits>()
            .init_resource::<enemies::Spawner>()
            .init_resource::<hud::Offer>()
            .init_resource::<cards::PendingPick>()
            .add_message::<abilities::Hit>()
            .add_message::<enemies::Loot>()
            .add_plugins(bevy::ui_render::UiMaterialPlugin::<hud::SweepUiMaterial>::default())
            .add_plugins(bevy::ui_render::UiMaterialPlugin::<fill::FillMaterial>::default())
            .add_systems(Startup, (hud::setup_hud, sync::init_multiplayer_from_env))
            .add_systems(Update, hero::daylight)
            .add_systems(
                Update,
                (
                    sync::poll_network,
                    hero::drop_stray_buttons,
                    hero::aim_cursor,
                    mobility::cast_mobility,
                    hero::drive_hero,
                    hero::place_camera,
                    sync::send_client_inputs,
                )
                    .chain()
                    .in_set(WorldStage::Input),
            )
            .add_systems(
                Update,
                (
                    enemies::spawn_enemies,
                    enemies::move_enemies,
                    abilities::cast_abilities,
                    abilities::tick_areas,
                    abilities::apply_hits,
                    enemies::tick_marks,
                    enemies::drop_loot,
                    enemies::collect_gems,
                    hero::hero_vitals,
                    sync::broadcast_host_snapshot,
                )
                    .chain()
                    .after(WorldStage::Input)
                    .run_if(in_state(Phase::Playing))
                    // Wait until local loading is cleared and all connected multiplayer peers are loaded.
                    .run_if(|net: Res<NetSession>, l: Res<crate::loading_screen::LoadingScreen>| {
                        !l.covering() && (!net.is_online() || net.all_loaded)
                    }),
            )
            .add_systems(
                Update,
                (
                    sync::apply_client_snapshot,
                    enemies::reap_dead,
                    enemies::still_gems,
                )
                    .after(WorldStage::Input),
            )
            .add_systems(
                Update,
                (
                    hero::follow_map,
                    hud::menu_preview,
                    hud::buttons,
                    hud::keys,
                    hud::refresh_menu,
                    menu::restore_pick,
                    menu::build_menu,
                    menu::menu_clicks,
                    menu::lobby_text_input,
                    menu::refresh_menu_ui.before(crate::glue::GlueVisuals),
                    cards::deal_cards,
                    cards::choose_card,
                    cards::animate_cards,
                    hud::refresh_hud,
                    hud::refresh_party_frames,
                    hud::refresh_results,
                    hud::fill_icons,
                    hud::update_cooldown_sweeps,
                    hud::refresh_mobility,
                    hud::show_panels,
                )
                    .chain()
                    .after(WorldStage::Input),
            )
            .add_systems(
                Update,
                (
                    hud::apply_ui_scale,
                    hud::fill_art,
                    hud::xp_hover,
                    hud::icon_tooltip,
                    hud::hero_plate,
                    minimap::map_buttons,
                    minimap::zone_text,
                    minimap::compose,
                )
                    .chain()
                    .after(hud::show_panels),
            )
            .add_systems(OnEnter(Phase::Playing), unpause)
            .add_systems(OnEnter(Phase::LevelUp), (pause, hud::roll_offer))
            .add_systems(OnEnter(Phase::Paused), pause)
            .add_systems(OnEnter(Phase::GameOver), unpause)
            .add_systems(OnEnter(Phase::Menu), (unpause, reset_run));
        if let Some(auto) = auto::from_env() {
            app.insert_resource(auto)
                .add_systems(
                    Update,
                    auto::drive.after(hud::keys).before(hud::refresh_menu),
                )
                .add_systems(Update, auto::plan.before(WorldStage::Input));
        }
    }
}

/// The screen the mode is on.
#[derive(States, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(super) enum Phase {
    #[default]
    Menu,
    Playing,
    LevelUp,
    Paused,
    GameOver,
}

/// Everything spawned for a run, despawned when the next begins.
#[derive(Component)]
pub(super) struct RunEntity;

/// Network entity identifier for synchronizing enemies, gems, areas, and totems.
#[derive(Component, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct NetId(pub u32);

/// The class, look and battleground picked at the menu; the battleground holds for the run.
#[derive(Resource, Default)]
pub(super) struct Selection {
    pub class: Option<usize>,
    /// `(display, race, sex)`, one per race and sex the class's trainers come in.
    pub looks: Vec<(u32, u8, u8)>,
    pub look: Option<usize>,
    /// An index into [`data::MAPS`].
    pub map: usize,
    /// The battleground the world is standing in: the last one entered (or the saved pick at
    /// boot). Picking another at the menu changes only `map`, so the menu hero stays put and no
    /// loading screen rises until Enter World.
    pub stage: usize,
    /// Bumped on every change, so the menu rebuilds once.
    pub version: u32,
}

impl Selection {
    pub fn class_def(&self) -> Option<&'static ClassDef> {
        self.class.map(|i| &data::CLASSES[i])
    }

    pub fn map_def(&self) -> &'static data::MapDef {
        &data::MAPS[self.map.min(data::MAPS.len() - 1)]
    }

    pub fn stage_def(&self) -> &'static data::MapDef {
        &data::MAPS[self.stage.min(data::MAPS.len() - 1)]
    }
}

/// An ability a hero has, with its own cooldown.
#[derive(Clone)]
pub(super) struct Owned {
    pub def: &'static AbilityDef,
    pub level: u8,
    pub cd: f32,
}

impl Owned {
    pub fn damage_mult(&self) -> f32 {
        1.0 + 0.3 * f32::from(self.level - 1)
    }
    pub fn cooldown(&self, stats: &Stats) -> f32 {
        let base = self.def.cooldown * 0.93f32.powi(i32::from(self.level) - 1);
        (base * (1.0 - stats.haste).max(0.35)).max(0.2)
    }
    pub fn count(&self) -> u32 {
        let extra = [3u8, 5, 7].iter().filter(|&&l| self.level >= l).count() as u32;
        self.def.count + extra
    }
    pub fn radius(&self, stats: &Stats) -> f32 {
        self.def.radius * (1.0 + 0.08 * f32::from(self.level - 1)) * stats.area
    }
    pub fn duration(&self) -> f32 {
        self.def.duration * (1.0 + 0.1 * f32::from(self.level - 1))
    }
}

/// A hero's derived numbers, from the base and the passives.
#[derive(Clone, Copy)]
pub(super) struct Stats {
    pub might: f32,
    pub max_hp: f32,
    pub speed: f32,
    pub regen: f32,
    pub haste: f32,
    pub area: f32,
    pub magnet: f32,
    pub growth: f32,
    pub armor: f32,
    pub crit: f32,
}

pub(super) const BASE_HP: f32 = 120.0;
pub(super) const BASE_SPEED: f32 = 7.0;
pub(super) const BASE_MAGNET: f32 = 4.5;

/// Per-player combat & progression state in a co-op or solo run.
#[derive(Clone)]
pub(super) struct Combatant {
    pub id: PlayerId,
    pub name: String,
    pub class: &'static ClassDef,
    pub display: u32,
    pub race: u8,
    pub sex: u8,
    pub hp: f32,
    pub abilities: Vec<Owned>,
    pub passives: Vec<(&'static PassiveDef, u8)>,
    pub shield: f32,
    pub shield_t: f32,
    pub immune_t: f32,
    pub thorns: f32,
    pub thorns_t: f32,
    pub thorns_spell: u32,
    pub heal_rate: f32,
    pub heal_t: f32,
    pub(self) mobile: mobility::MobilityState,
    /// `Some(revive_progress_secs)` when downed (0 HP); reaches `net::REVIVE_SECS` to revive.
    pub downed_revive: Option<f32>,
}

impl Combatant {
    pub fn new(id: PlayerId, name: String, class: &'static ClassDef, display: u32, race: u8, sex: u8) -> Self {
        let mut c = Self {
            id,
            name,
            class,
            display,
            race,
            sex,
            hp: BASE_HP,
            abilities: vec![Owned {
                def: data::ability(class.start),
                level: 1,
                cd: 0.5,
            }],
            passives: Vec::new(),
            shield: 0.0,
            shield_t: 0.0,
            immune_t: 0.0,
            thorns: 0.0,
            thorns_t: 0.0,
            thorns_spell: 0,
            heal_rate: 0.0,
            heal_t: 0.0,
            mobile: mobility::MobilityState::default(),
            downed_revive: None,
        };
        c.hp = c.stats().max_hp;
        c
    }

    pub fn stats(&self) -> Stats {
        compute_stats(self.class, &self.passives)
    }

    pub fn is_down(&self) -> bool {
        self.downed_revive.is_some()
    }
}

pub(super) fn compute_stats(class: &ClassDef, passives: &[(&'static PassiveDef, u8)]) -> Stats {
    let mut s = Stats {
        might: 1.0,
        max_hp: BASE_HP,
        speed: BASE_SPEED,
        regen: 0.0,
        haste: 0.0,
        area: 1.0,
        magnet: BASE_MAGNET,
        growth: 1.0,
        armor: 0.0,
        crit: 0.05,
    };
    match class.id {
        1 | 2 => s.max_hp += 40.0,
        7 | 11 | 3 | 4 => s.max_hp += 15.0,
        _ => s.speed += 0.5,
    }
    for &(p, level) in passives {
        let v = p.per_level * f32::from(level);
        match p.stat {
            Stat::Might => s.might += v,
            Stat::MaxHp => s.max_hp *= 1.0 + v,
            Stat::Speed => s.speed *= 1.0 + v,
            Stat::Regen => s.regen += v,
            Stat::Haste => s.haste += v,
            Stat::Area => s.area += v,
            Stat::Magnet => s.magnet *= 1.0 + v,
            Stat::Growth => s.growth += v,
            Stat::Armor => s.armor += v,
            Stat::Crit => s.crit += v,
        }
    }
    s
}

/// One run's state. Holds the local player's fields directly for HUD compatibility and `players`
/// for all co-op party members (1..=4).
#[derive(Resource)]
pub(super) struct Run {
    pub class: &'static ClassDef,
    pub race: u8,
    pub elapsed: f32,
    pub kills: u32,
    pub level: u32,
    pub xp: f32,
    pub hp: f32,
    pub abilities: Vec<Owned>,
    pub passives: Vec<(&'static PassiveDef, u8)>,
    pub shield: f32,
    pub shield_t: f32,
    pub immune_t: f32,
    pub thorns: f32,
    pub thorns_t: f32,
    pub thorns_spell: u32,
    pub heal_rate: f32,
    pub heal_t: f32,
    /// The class mobility's cooldown and running effect.
    mobile: mobility::MobilityState,
    pub pending_levels: u32,
    pub bosses: u32,
    pub rings: u32,
    pub won: bool,
    /// Seconds since the hero (or entire party) fell, while the death plays out.
    pub dead_t: Option<f32>,
    pub next_guid: u64,
    pub next_net_id: u32,
    pub damage_done: f64,
    /// Bumped whenever abilities or passives change, so the HUD rebuilds the icon rows once.
    pub version: u32,
    /// A centre-screen announcement and its seconds left.
    pub banner: Option<(String, f32)>,
    /// Co-op combatants indexed by `PlayerId`. In singleplayer, holds 1 entry (`id = 0`).
    pub players: Vec<Combatant>,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            class: &data::CLASSES[0],
            race: 0,
            elapsed: 0.0,
            kills: 0,
            level: 1,
            xp: 0.0,
            hp: BASE_HP,
            abilities: Vec::new(),
            passives: Vec::new(),
            shield: 0.0,
            shield_t: 0.0,
            immune_t: 0.0,
            thorns: 0.0,
            thorns_t: 0.0,
            thorns_spell: 0,
            heal_rate: 0.0,
            heal_t: 0.0,
            mobile: mobility::MobilityState::default(),
            pending_levels: 0,
            bosses: 0,
            rings: 0,
            won: false,
            dead_t: None,
            next_guid: 1,
            next_net_id: 1,
            damage_done: 0.0,
            version: 0,
            banner: None,
            players: Vec::new(),
        }
    }
}

impl Run {
    pub fn stats(&self) -> Stats {
        compute_stats(self.class, &self.passives)
    }

    pub fn guid(&mut self) -> u64 {
        self.next_guid += 1;
        self.next_guid
    }

    pub fn alloc_net_id(&mut self) -> u32 {
        self.next_net_id = self.next_net_id.wrapping_add(1).max(1);
        self.next_net_id
    }

    pub fn player(&self, id: PlayerId) -> Option<&Combatant> {
        self.players.iter().find(|p| p.id == id)
    }

    pub fn player_mut(&mut self, id: PlayerId) -> Option<&mut Combatant> {
        self.players.iter_mut().find(|p| p.id == id)
    }

    /// Synchronize the top-level local player fields (`hp`, `abilities`, `passives`, `shield`, `mobile`)
    /// from `self.players[local_id]` so the local HUD and tooltips stay seamlessly up-to-date.
    pub fn sync_local_from_combatant(&mut self, local_id: PlayerId) {
        if let Some(c) = self.players.iter().find(|p| p.id == local_id) {
            self.class = c.class;
            self.race = c.race;
            self.hp = c.hp;
            self.shield = c.shield;
            self.shield_t = c.shield_t;
            self.immune_t = c.immune_t;
            self.thorns = c.thorns;
            self.thorns_t = c.thorns_t;
            self.thorns_spell = c.thorns_spell;
            self.heal_rate = c.heal_rate;
            self.heal_t = c.heal_t;
            self.mobile = c.mobile;
            let build_changed = self.abilities.len() != c.abilities.len()
                || self.passives.len() != c.passives.len()
                || self
                    .abilities
                    .iter()
                    .zip(&c.abilities)
                    .any(|(a, b)| a.def.key != b.def.key || a.level != b.level)
                || self
                    .passives
                    .iter()
                    .zip(&c.passives)
                    .any(|(a, b)| a.0.key != b.0.key || a.1 != b.1);
            self.abilities = c.abilities.clone();
            self.passives = c.passives.clone();
            if build_changed {
                self.version = self.version.wrapping_add(1);
            }
        }
    }

    /// Push local top-level changes into `self.players[local_id]`.
    pub fn sync_combatant_from_local(&mut self, local_id: PlayerId) {
        let class = self.class;
        let race = self.race;
        let hp = self.hp;
        let abilities = self.abilities.clone();
        let passives = self.passives.clone();
        let shield = self.shield;
        let shield_t = self.shield_t;
        let immune_t = self.immune_t;
        let thorns = self.thorns;
        let thorns_t = self.thorns_t;
        let thorns_spell = self.thorns_spell;
        let heal_rate = self.heal_rate;
        let heal_t = self.heal_t;
        let mobile = self.mobile;
        if let Some(c) = self.players.iter_mut().find(|p| p.id == local_id) {
            c.class = class;
            c.race = race;
            c.hp = hp;
            c.abilities = abilities;
            c.passives = passives;
            c.shield = shield;
            c.shield_t = shield_t;
            c.immune_t = immune_t;
            c.thorns = thorns;
            c.thorns_t = thorns_t;
            c.thorns_spell = thorns_spell;
            c.heal_rate = heal_rate;
            c.heal_t = heal_t;
            c.mobile = mobile;
        }
    }
}

/// xorshift64*: the mode's dice, seeded from the clock.
#[derive(Resource)]
pub(super) struct Rng(pub u64);

impl Default for Rng {
    fn default() -> Self {
        let seed = web_time::SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self(seed | 1)
    }
}

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `[0, 1)`.
    pub fn f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.f32()
    }
    pub fn index(&mut self, len: usize) -> usize {
        (self.next_u64() % len.max(1) as u64) as usize
    }
}

fn pause(mut time: ResMut<Time<Virtual>>) {
    time.pause();
}

fn unpause(mut time: ResMut<Time<Virtual>>) {
    time.unpause();
}

/// Back at the menu: everything the last run spawned goes, and the run starts over.
fn reset_run(
    mut commands: Commands,
    spawned: Query<Entity, With<RunEntity>>,
    mut run: ResMut<Run>,
    mut pending: ResMut<abilities::PendingHits>,
    mut spawner: ResMut<enemies::Spawner>,
    mut selection: ResMut<Selection>,
    mut net: ResMut<NetSession>,
) {
    for e in &spawned {
        commands.entity(e).despawn();
    }
    let next_guid = run.next_guid;
    let next_net_id = run.next_net_id;
    *run = Run {
        next_guid,
        next_net_id,
        ..Run::default()
    };
    pending.0.clear();
    *spawner = enemies::Spawner::default();
    net.client_enemies.clear();
    net.client_gems.clear();
    net.client_areas.clear();
    net.client_totems.clear();
    net.latest_snap = None;
    net.levelup_picked.clear();
    net.host_offers.clear();
    // The preview hero is respawned for the look still picked.
    selection.version += 1;
}
