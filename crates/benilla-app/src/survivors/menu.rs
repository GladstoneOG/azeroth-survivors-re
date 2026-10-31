//! The run's pick screen, laid out like the 1.12 character create and select glue screens and built
//! from their own widgets and art ([`crate::glue`]): the race, gender and class tower on the left,
//! the zone list in the character-select frame on the right, the LAN multiplayer lobby at top
//! centre, and Enter World at the bottom.
//!
//! The survivors HUD lays out on the 768-unit canvas through `UiScale`, so every glue builder here
//! takes a unit scale.

use bevy::prelude::*;
use bevy::ui_render::ui_material::MaterialNode;

use super::hud::{looks_for, rgb, start_run_with_net, text_bundle, HudFont, MenuRoot};
use super::net::{LobbyPlayer, NetRole, NetSession, ReliableMsg, DEFAULT_PORT, MAX_PLAYERS};
use super::{data, Phase, Rng, Run, Selection};
use crate::char_select::race_name;
use crate::glue::art::{class_tc, race_tc, tc_rect, GlueArt, DIM, GOLD, NAME_EDGE};
use crate::glue::backdrop::{backdrop_border, tiled_bg_node};
use crate::glue::widgets::{
    abs, glue_button, icon_button, outlined_text, GlueBtnKind, GlueDisabled, GlueText, Hilight,
    HoverLabel, LockHighlight,
};
use crate::glue::GlueClicks;

/// The create screen's two race columns.
const ALLIANCE: [u8; 4] = [1, 3, 4, 7];
const HORDE: [u8; 4] = [2, 5, 6, 8];

/// `CharacterSelectCharacterFrame`'s border and fill tints.
const FRAME_BORDER: Color = Color::srgb(0.8, 0.8, 0.8);
const FRAME_FILL: Color = Color::srgba(0.09, 0.09, 0.09, 0.85);

/// The tower ends under the class grid and Randomize, where the create screen's appearance dials
/// would start: there is nothing to customise here.
const TOWER_H: f32 = 590.0;

/// The zone rows: the character-select row, compacted so every zone fits the 642-tall frame.
const ROW_TOP: f32 = 44.0;
const ROW_PITCH: f32 = 36.0;
const ROW_H: f32 = 34.0;
const ROW_W: f32 = 228.0;

#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MenuAction {
    Race(u8),
    Sex(u8),
    Class(usize),
    Map(usize),
    Randomize,
    Enter,
    FocusIp,
    HostGame,
    JoinGame,
    LeaveNet,
}

/// On [`MenuRoot`] once the glue art is in and the screen is built.
#[derive(Component)]
pub(super) struct MenuBuilt;

/// A race button's face, re-cropped to the picked gender.
#[derive(Component)]
pub(super) struct RaceFace(u8);

#[derive(Component)]
pub(super) struct PickName;

#[derive(Component)]
pub(super) struct LobbyStatusText;

#[derive(Component)]
pub(super) struct LobbyIpBox;

#[derive(Component)]
pub(super) struct LobbyIpText;

#[derive(Component)]
pub(super) struct LobbySlotText(usize);

#[derive(Component)]
pub(super) struct LobbyHintText;

/// The saved pick, `benilla-config/survivors-pick.txt` (`localStorage` on the page):
/// `class race sex map`, restored at the menu so a reload keeps the last hero and zone.
fn pick_path() -> Option<std::path::PathBuf> {
    crate::local_state::home().map(|h| h.join("survivors-pick.txt"))
}

fn save_pick(sel: &Selection) {
    let (Some(class), Some(&(_, race, sex))) = (sel.class, sel.look.and_then(|i| sel.looks.get(i)))
    else {
        return;
    };
    if let Some(path) = pick_path() {
        let _ =
            crate::local_state::write_atomic(&path, &format!("{class} {race} {sex} {}", sel.map));
    }
}

/// Restore the saved pick once the creature data can resolve its looks; a stale or missing file
/// leaves the default.
pub(super) fn restore_pick(
    creatures: Option<Res<crate::entities::Creatures>>,
    mut selection: ResMut<Selection>,
    mut done: Local<bool>,
) {
    if *done {
        return;
    }
    let Some(creatures) = creatures.as_deref() else {
        return;
    };
    *done = true;
    let Some(text) = pick_path().and_then(|p| crate::local_state::read_to_string(&p).ok()) else {
        return;
    };
    let v: Vec<usize> = text
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    let [class, race, sex, map] = v[..] else {
        return;
    };
    if map < data::MAPS.len() {
        selection.map = map;
        selection.stage = map;
        selection.version += 1;
    }
    if class < data::CLASSES.len() {
        let all = all_looks(creatures);
        if let Some(l) = find(&all[class], Some(race as u8), Some(sex as u8)) {
            set(&mut selection, &all, class, l);
        }
    }
}

/// Every class's looks, `(display, race, sex)`, indexed like [`data::CLASSES`].
fn all_looks(creatures: &crate::entities::Creatures) -> Vec<Vec<(u32, u8, u8)>> {
    data::CLASSES
        .iter()
        .map(|c| looks_for(c, creatures))
        .collect()
}

fn find(looks: &[(u32, u8, u8)], race: Option<u8>, sex: Option<u8>) -> Option<usize> {
    looks
        .iter()
        .position(|&(_, r, s)| race.is_none_or(|x| x == r) && sex.is_none_or(|x| x == s))
}

fn set(sel: &mut Selection, all: &[Vec<(u32, u8, u8)>], class: usize, look: usize) {
    sel.class = Some(class);
    sel.looks = all[class].clone();
    sel.look = Some(look);
    sel.version += 1;
}

pub(super) fn local_lobby_player(sel: &Selection, net: &NetSession) -> LobbyPlayer {
    let class = sel.class.unwrap_or(0);
    let (display, race, sex) = sel
        .look
        .and_then(|i| sel.looks.get(i))
        .copied()
        .unwrap_or((data::CLASSES[class].looks[0], 1, 0));
    LobbyPlayer {
        id: net.local_id,
        name: net.player_name.clone(),
        class,
        display,
        race,
        sex,
        ready: if net.role == NetRole::Host {
            true
        } else {
            net.local_ready
        },
    }
}

fn sync_local_pick_to_net(sel: &Selection, net: &mut NetSession) {
    let lp = local_lobby_player(sel, net);
    match net.role {
        NetRole::Offline => {}
        NetRole::Host => {
            if let Some(slot) = net.lobby_players.iter_mut().find(|p| p.id == net.local_id) {
                slot.class = lp.class;
                slot.display = lp.display;
                slot.race = lp.race;
                slot.sex = lp.sex;
            }
            if let Some(host) = &net.host {
                host.broadcast_reliable(&ReliableMsg::LobbyState {
                    map: sel.map,
                    players: net.lobby_players.clone(),
                });
            }
        }
        NetRole::Client => {
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

/// Build the screen into [`MenuRoot`] once the archives are readable, so the glue art is there.
pub(super) fn build_menu(
    mut commands: Commands,
    roots: Query<Entity, (With<MenuRoot>, Without<MenuBuilt>)>,
    assets: Option<ResMut<benilla_assets::WorldAssets>>,
    mut art: ResMut<GlueArt>,
    mut images: ResMut<Assets<Image>>,
    mut add_mats: ResMut<Assets<crate::glue::add_material::AddUiMaterial>>,
    font: Res<HudFont>,
) {
    let Ok(root) = roots.single() else { return };
    let Some(mut assets) = assets else { return };
    art.ensure_loaded(&mut assets, &mut images, &mut add_mats);
    let f = &font.0;
    commands.entity(root).insert(MenuBuilt).with_children(|ui| {
        tower(ui, &art, f);
        // `CharacterCreateWoWLogo`, 256×128 at TOPLEFT (3,-7), after the tower so it draws over
        // the border art, as the create screen orders it.
        if let Some(logo) = &art.logo {
            ui.spawn((
                ImageNode::new(logo.clone()),
                abs(1.0, 3.0, 7.0, 256.0, 128.0),
            ));
        }
        zone_frame(ui, &art, f);
        lobby_frame(ui, &art, f);

        // `CharSelectCharacterName`, GlueFontNormalHuge at BOTTOM (0,100).
        ui.spawn(Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(100.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|c| {
            outlined_text(
                c,
                Node::default(),
                (),
                PickName,
                GlueText {
                    text: "",
                    size: 22.0,
                    color: GOLD,
                    wrap: false,
                },
                f,
                1.0,
            );
        });
        // Multiplayer action hint above Enter World.
        ui.spawn(Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(84.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|c| {
            c.spawn((
                LobbyHintText,
                text_bundle(f, "", 13.0, Color::srgb(0.75, 0.88, 1.0)),
            ));
        });
        // `CharSelectEnterWorldButton`, 200×60 at BOTTOM (0,30).
        ui.spawn(Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(30.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|c| {
            glue_button(
                c,
                &art,
                f,
                MenuAction::Enter,
                "Enter World",
                200.0,
                60.0,
                GlueBtnKind::Normal,
                1.0,
            );
        });
    });
}

/// Top-centre LAN / ZeroTier / Tailscale Co-op Lobby panel.
fn lobby_frame(ui: &mut ChildSpawnerCommands, art: &GlueArt, f: &Handle<Font>) {
    let px = Val::Px;
    ui.spawn(Node {
        position_type: PositionType::Absolute,
        left: Val::Percent(50.0),
        top: px(12.0),
        margin: UiRect::left(px(-210.0)),
        width: px(420.0),
        flex_direction: FlexDirection::Column,
        padding: UiRect::axes(px(14.0), px(10.0)),
        row_gap: px(6.0),
        border: UiRect::all(px(1.5)),
        border_radius: BorderRadius::all(px(6.0)),
        ..default()
    })
    .insert((
        BackgroundColor(FRAME_FILL),
        BorderColor::all(Color::srgb(0.55, 0.45, 0.25)),
    ))
    .with_children(|panel| {
        // Header & status
        panel
            .spawn(Node {
                width: Val::Percent(100.0),
                justify_content: JustifyContent::SpaceBetween,
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|top| {
                top.spawn(text_bundle(
                    f,
                    "LAN / ZeroTier / Tailscale Co-op",
                    14.0,
                    GOLD,
                ));
                top.spawn((
                    LobbyStatusText,
                    text_bundle(f, "Offline (Solo)", 12.0, Color::srgb(0.75, 0.75, 0.75)),
                ));
            });

        // IP Input + Host / Join / Leave row
        panel
            .spawn(Node {
                width: Val::Percent(100.0),
                column_gap: px(6.0),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|row| {
                // Clickable IP input box
                row.spawn((
                    LobbyIpBox,
                    MenuAction::FocusIp,
                    Button,
                    Node {
                        width: px(168.0),
                        height: px(28.0),
                        padding: UiRect::axes(px(8.0), px(4.0)),
                        align_items: AlignItems::Center,
                        border: UiRect::all(px(1.5)),
                        border_radius: BorderRadius::all(px(4.0)),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.03, 0.03, 0.05, 0.95)),
                    BorderColor::all(Color::srgb(0.45, 0.45, 0.5)),
                ))
                .with_children(|box_node| {
                    box_node.spawn((
                        LobbyIpText,
                        text_bundle(f, "127.0.0.1:25565", 13.0, Color::WHITE),
                    ));
                });

                glue_button(
                    row,
                    art,
                    f,
                    MenuAction::HostGame,
                    "Host LAN",
                    74.0,
                    28.0,
                    GlueBtnKind::Small,
                    1.0,
                );
                glue_button(
                    row,
                    art,
                    f,
                    MenuAction::JoinGame,
                    "Join IP",
                    70.0,
                    28.0,
                    GlueBtnKind::Small,
                    1.0,
                );
                glue_button(
                    row,
                    art,
                    f,
                    MenuAction::LeaveNet,
                    "Leave",
                    62.0,
                    28.0,
                    GlueBtnKind::Small,
                    1.0,
                );
            });

        // 4-Player Party Roster
        panel
            .spawn(Node {
                width: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                row_gap: px(2.0),
                padding: UiRect::top(px(2.0)),
                ..default()
            })
            .with_children(|roster| {
                for i in 0..MAX_PLAYERS {
                    roster.spawn((
                        LobbySlotText(i),
                        text_bundle(f, "", 12.0, Color::srgb(0.65, 0.65, 0.65)),
                    ));
                }
            });
    });
}

/// `CharacterCreateConfigurationFrame`, 206×600 at TOPLEFT (28,-74): the race columns under the
/// faction banners, the gender pair, the class grid, the class text and Randomize.
fn tower(ui: &mut ChildSpawnerCommands, art: &GlueArt, f: &Handle<Font>) {
    let px = Val::Px;
    ui.spawn(Node {
        position_type: PositionType::Absolute,
        left: px(28.0),
        top: px(74.0),
        width: px(206.0),
        height: px(TOWER_H),
        ..default()
    })
    .with_children(|tower| {
        if let Some(bg) = &art.tower_bg {
            tower.spawn((
                ImageNode::new(bg.clone()),
                abs(1.0, -3.0, 0.0, 218.0, TOWER_H + 6.0),
            ));
        }
        // The create tower's top piece, then its foot piece cut to end where the class grid does.
        if let Some((border, size)) = &art.tower_border {
            for (top, height, tc) in [
                (0.0, 236.0, [0.0, 0.875, 0.0, 0.9375]),
                (
                    236.0,
                    TOWER_H - 236.0 + 6.0,
                    [
                        0.0,
                        0.875,
                        1.0 - (TOWER_H - 236.0 + 6.0) / 256.0 * 0.8203125 / (210.0 / 256.0),
                        1.0,
                    ],
                ),
            ] {
                tower.spawn((
                    ImageNode {
                        image: border.clone(),
                        rect: Some(tc_rect(*size, tc)),
                        ..default()
                    },
                    abs(1.0, -9.0, top, 224.0, height),
                ));
            }
        }
        if let Some(banners) = &art.banners {
            tower.spawn((
                ImageNode::new(banners.clone()),
                abs(1.0, -27.0, 60.0, 256.0, 259.0),
            ));
        }
        for (label, center) in [("Alliance", 51.0), ("Horde", 151.0)] {
            outlined_text(
                tower,
                Node {
                    justify_content: JustifyContent::Center,
                    ..abs(1.0, center - 60.0, 40.0, 120.0, 17.0)
                },
                (),
                (),
                GlueText {
                    text: label,
                    size: 15.0,
                    color: GOLD,
                    wrap: false,
                },
                f,
                1.0,
            );
        }

        for (faction, left) in [(ALLIANCE, 33.0), (HORDE, 127.0)] {
            tower
                .spawn(Node {
                    position_type: PositionType::Absolute,
                    left: px(left),
                    top: px(68.0),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(5.0),
                    ..default()
                })
                .with_children(|col| {
                    for race in faction {
                        let face = art.races.as_ref().and_then(|(sheet, size)| {
                            race_tc(race, 0).map(|tc| (sheet.clone(), tc_rect(*size, tc)))
                        });
                        icon_button(
                            col,
                            f,
                            MenuAction::Race(race),
                            Some(RaceFace(race)),
                            face,
                            None::<()>,
                            race_name(race),
                            art,
                            1.0,
                        );
                    }
                });
        }

        tower
            .spawn(Node {
                position_type: PositionType::Absolute,
                left: px(53.0),
                top: px(303.0),
                flex_direction: FlexDirection::Row,
                column_gap: px(5.0),
                ..default()
            })
            .with_children(|g| {
                for (sex, label) in [(0u8, "Male"), (1u8, "Female")] {
                    let face = art.gender.as_ref().map(|(sheet, size)| {
                        let half = if sex == 0 {
                            [0.0, 0.5, 0.0, 1.0]
                        } else {
                            [0.5, 1.0, 0.0, 1.0]
                        };
                        (sheet.clone(), tc_rect(*size, half))
                    });
                    icon_button(
                        g,
                        f,
                        MenuAction::Sex(sex),
                        None::<()>,
                        face,
                        None::<()>,
                        label,
                        art,
                        1.0,
                    );
                }
            });

        tower
            .spawn(Node {
                position_type: PositionType::Absolute,
                // The gender pair's centre line: 53 + (2·48 + 5)/2 ≈ 103.
                left: px(103.0 - (3.0 * 48.0 + 2.0 * 4.0) / 2.0),
                width: px(3.0 * 48.0 + 2.0 * 4.0),
                top: px(368.0),
                flex_direction: FlexDirection::Row,
                flex_wrap: FlexWrap::Wrap,
                justify_content: JustifyContent::Center,
                column_gap: px(4.0),
                row_gap: px(4.0),
                ..default()
            })
            .with_children(|grid| {
                for (i, class) in data::CLASSES.iter().enumerate() {
                    let face = art.classes.as_ref().and_then(|(sheet, size)| {
                        class_tc(class.id).map(|tc| (sheet.clone(), tc_rect(*size, tc)))
                    });
                    icon_button(
                        grid,
                        f,
                        MenuAction::Class(i),
                        None::<()>,
                        face,
                        None::<()>,
                        class.name,
                        art,
                        1.0,
                    );
                }
            });

        tower
            .spawn(Node {
                position_type: PositionType::Absolute,
                left: px(30.0),
                top: px(TOWER_H - 46.0),
                ..default()
            })
            .with_children(|r| {
                glue_button(
                    r,
                    art,
                    f,
                    MenuAction::Randomize,
                    "Randomize",
                    146.0,
                    30.0,
                    GlueBtnKind::Small,
                    1.0,
                );
            });
    });
}

/// The zone list in `CharacterSelectCharacterFrame` (260×642 at TOPRIGHT (-5,-15)), one
/// character-row per zone, lowest level first.
fn zone_frame(ui: &mut ChildSpawnerCommands, art: &GlueArt, f: &Handle<Font>) {
    let px = Val::Px;
    let mut order: Vec<(usize, u32, u32)> = (0..data::MAPS.len())
        .map(|i| {
            let levels = data::roster(i).enemies.iter().map(|e| e.level);
            let lo = levels.clone().min().unwrap_or(1);
            let hi = levels.max().unwrap_or(lo);
            (i, lo, hi)
        })
        .collect();
    order.sort_by_key(|&(i, lo, _)| (lo, data::MAPS[i].name));

    ui.spawn(Node {
        position_type: PositionType::Absolute,
        right: px(5.0),
        top: px(15.0),
        width: px(260.0),
        height: px(642.0),
        ..default()
    })
    .with_children(|frame| {
        if let (Some(bg), Some(border)) = (&art.tooltip_bg, &art.name_border) {
            frame.spawn((
                tiled_bg_node(bg.clone(), NAME_EDGE, 1.0, FRAME_FILL),
                Node {
                    position_type: PositionType::Absolute,
                    left: px(10.0),
                    right: px(5.0),
                    top: px(4.0),
                    bottom: px(9.0),
                    ..default()
                },
            ));
            backdrop_border(frame, border, NAME_EDGE, FRAME_BORDER);
        } else {
            frame.spawn((BackgroundColor(FRAME_FILL), crate::glue::widgets::overlay()));
        }
        outlined_text(
            frame,
            Node {
                position_type: PositionType::Absolute,
                top: px(12.0),
                width: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                ..default()
            },
            (),
            (),
            GlueText {
                text: "Choose a Zone",
                size: 18.0,
                color: DIM,
                wrap: false,
            },
            f,
            1.0,
        );
        for (row, &(i, lo, hi)) in order.iter().enumerate() {
            let map = &data::MAPS[i];
            frame
                .spawn((
                    MenuAction::Map(i),
                    Button,
                    LockHighlight::default(),
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(24.0),
                        top: px(ROW_TOP + row as f32 * ROW_PITCH),
                        width: px(ROW_W),
                        height: px(ROW_H),
                        ..default()
                    },
                ))
                .with_children(|b| {
                    if let Some(hi) = &art.select_highlight {
                        b.spawn((
                            Hilight,
                            Visibility::Hidden,
                            MaterialNode(hi.clone()),
                            abs(1.0, -20.0, -6.0, ROW_W + 20.0, ROW_H + 12.0),
                        ));
                    }
                    outlined_text(
                        b,
                        abs(1.0, 0.0, 2.0, ROW_W, 16.0),
                        (),
                        (),
                        GlueText {
                            text: map.name,
                            size: 15.0,
                            color: GOLD,
                            wrap: false,
                        },
                        f,
                        1.0,
                    );
                    let levels = if lo == hi {
                        format!("Level {lo}")
                    } else {
                        format!("Level {lo}-{hi}")
                    };
                    outlined_text(
                        b,
                        abs(1.0, 0.0, 19.0, ROW_W, 14.0),
                        (),
                        (),
                        GlueText {
                            text: &format!("{levels}  {}", map.zone),
                            size: 12.0,
                            color: Color::WHITE,
                            wrap: false,
                        },
                        f,
                        1.0,
                    );
                });
        }
    });
}

/// Handle typing an IP:Port / Tailscale / ZeroTier address when the IP box is focused.
pub(super) fn lobby_text_input(
    phase: Res<State<Phase>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut net: ResMut<NetSession>,
) {
    if *phase.get() != Phase::Menu || !net.ip_focused {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) || keys.just_pressed(KeyCode::Enter) {
        net.ip_focused = false;
        return;
    }
    if keys.just_pressed(KeyCode::Backspace) {
        net.ip_input.pop();
    }
    let shift = keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]);
    let table: &[(KeyCode, char, char)] = &[
        (KeyCode::Digit0, '0', ')'),
        (KeyCode::Digit1, '1', '!'),
        (KeyCode::Digit2, '2', '@'),
        (KeyCode::Digit3, '3', '#'),
        (KeyCode::Digit4, '4', '$'),
        (KeyCode::Digit5, '5', '%'),
        (KeyCode::Digit6, '6', '^'),
        (KeyCode::Digit7, '7', '&'),
        (KeyCode::Digit8, '8', '*'),
        (KeyCode::Digit9, '9', '('),
        (KeyCode::Numpad0, '0', '0'),
        (KeyCode::Numpad1, '1', '1'),
        (KeyCode::Numpad2, '2', '2'),
        (KeyCode::Numpad3, '3', '3'),
        (KeyCode::Numpad4, '4', '4'),
        (KeyCode::Numpad5, '5', '5'),
        (KeyCode::Numpad6, '6', '6'),
        (KeyCode::Numpad7, '7', '7'),
        (KeyCode::Numpad8, '8', '8'),
        (KeyCode::Numpad9, '9', '9'),
        (KeyCode::Period, '.', '.'),
        (KeyCode::NumpadDecimal, '.', '.'),
        (KeyCode::Semicolon, ':', ':'),
        (KeyCode::Minus, '-', '_'),
        (KeyCode::KeyA, 'a', 'A'),
        (KeyCode::KeyB, 'b', 'B'),
        (KeyCode::KeyC, 'c', 'C'),
        (KeyCode::KeyD, 'd', 'D'),
        (KeyCode::KeyE, 'e', 'E'),
        (KeyCode::KeyF, 'f', 'F'),
        (KeyCode::KeyG, 'g', 'G'),
        (KeyCode::KeyH, 'h', 'H'),
        (KeyCode::KeyI, 'i', 'I'),
        (KeyCode::KeyJ, 'j', 'J'),
        (KeyCode::KeyK, 'k', 'K'),
        (KeyCode::KeyL, 'l', 'L'),
        (KeyCode::KeyM, 'm', 'M'),
        (KeyCode::KeyN, 'n', 'N'),
        (KeyCode::KeyO, 'o', 'O'),
        (KeyCode::KeyP, 'p', 'P'),
        (KeyCode::KeyQ, 'q', 'Q'),
        (KeyCode::KeyR, 'r', 'R'),
        (KeyCode::KeyS, 's', 'S'),
        (KeyCode::KeyT, 't', 'T'),
        (KeyCode::KeyU, 'u', 'U'),
        (KeyCode::KeyV, 'v', 'V'),
        (KeyCode::KeyW, 'w', 'W'),
        (KeyCode::KeyX, 'x', 'X'),
        (KeyCode::KeyY, 'y', 'Y'),
        (KeyCode::KeyZ, 'z', 'Z'),
    ];
    for &(code, lo, hi) in table {
        if keys.just_pressed(code) && net.ip_input.len() < 42 {
            net.ip_input.push(if shift { hi } else { lo });
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn menu_clicks(
    clicks: Res<GlueClicks>,
    actions: Query<(Entity, &MenuAction)>,
    phase: Res<State<Phase>>,
    creatures: Option<Res<crate::entities::Creatures>>,
    mut selection: ResMut<Selection>,
    mut run: ResMut<Run>,
    mut net: ResMut<NetSession>,
    mut rng: ResMut<Rng>,
    mut next: ResMut<NextState<Phase>>,
) {
    if *phase.get() != Phase::Menu {
        return;
    }
    let Some(creatures) = creatures.as_deref() else {
        return;
    };
    let Some(action) = actions
        .iter()
        .find(|(e, _)| clicks.hit(*e))
        .map(|(_, a)| *a)
    else {
        return;
    };
    if action != MenuAction::FocusIp {
        net.ip_focused = false;
    }
    let all = all_looks(creatures);
    let cur = selection
        .look
        .and_then(|i| selection.looks.get(i))
        .map(|&(_, r, s)| (r, s));
    let (race, sex) = (cur.map(|c| c.0), cur.map_or(0, |c| c.1));
    match action {
        MenuAction::FocusIp => {
            net.ip_focused = !net.ip_focused;
            return;
        }
        MenuAction::HostGame => {
            let lp = local_lobby_player(&selection, &net);
            let bind = format!("0.0.0.0:{DEFAULT_PORT}");
            net.start_host(&bind, lp);
            return;
        }
        MenuAction::JoinGame => {
            let lp = local_lobby_player(&selection, &net);
            let addr = net.ip_input.trim().to_string();
            if !addr.is_empty() {
                net.start_client(&addr, lp);
            }
            return;
        }
        MenuAction::LeaveNet => {
            net.disconnect();
            return;
        }
        MenuAction::Map(i) => {
            // Clients follow the Host's chosen zone.
            if net.is_client() {
                return;
            }
            if selection.map != i {
                selection.map = i;
                selection.version += 1;
                if let Some(host) = &net.host {
                    host.broadcast_reliable(&ReliableMsg::LobbyState {
                        map: selection.map,
                        players: net.lobby_players.clone(),
                    });
                }
            }
        }
        MenuAction::Enter => {
            if start_run_with_net(&mut run, &mut selection, &mut net, &mut rng) {
                next.set(Phase::Playing);
            }
            return;
        }
        MenuAction::Randomize => {
            let picks: Vec<(usize, usize)> = all
                .iter()
                .enumerate()
                .flat_map(|(c, looks)| (0..looks.len()).map(move |l| (c, l)))
                .collect();
            if !picks.is_empty() {
                let (c, l) = picks[rng.index(picks.len())];
                set(&mut selection, &all, c, l);
            }
        }
        // The class keeps the race and gender where it can.
        MenuAction::Class(c) => {
            let looks = &all[c];
            let l = find(looks, race, Some(sex))
                .or_else(|| find(looks, race, None))
                .or_else(|| find(looks, None, Some(sex)))
                .or_else(|| find(looks, None, None));
            if let Some(l) = l {
                set(&mut selection, &all, c, l);
            }
        }
        // The race keeps the class where it can, else takes the first class it has.
        MenuAction::Race(r) => {
            let classes = selection.class.into_iter().chain(0..all.len());
            for c in classes {
                let l = find(&all[c], Some(r), Some(sex)).or_else(|| find(&all[c], Some(r), None));
                if let Some(l) = l {
                    set(&mut selection, &all, c, l);
                    break;
                }
            }
        }
        MenuAction::Sex(s) => {
            if let Some(c) = selection.class {
                if let Some(l) = find(&all[c], race, Some(s)) {
                    set(&mut selection, &all, c, l);
                }
            }
        }
    }
    save_pick(&selection);
    sync_local_pick_to_net(&selection, &mut net);
}

/// The screen's per-frame state: the locked highlights, hover labels, which races and classes
/// the pick allows, the gender on the race faces, the name line, the LAN lobby panel, and Enter World.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(super) fn refresh_menu_ui(
    selection: Res<Selection>,
    net: Res<NetSession>,
    creatures: Option<Res<crate::entities::Creatures>>,
    art: Res<GlueArt>,
    mut buttons: Query<(
        &MenuAction,
        &Interaction,
        &Children,
        Option<&mut LockHighlight>,
        Option<&mut GlueDisabled>,
        &mut Node,
        &mut Visibility,
    )>,
    mut ip_box: Query<&mut BorderColor, With<LobbyIpBox>>,
    mut labels: Query<&mut Visibility, (With<HoverLabel>, Without<MenuAction>)>,
    mut faces: Query<(&RaceFace, &mut ImageNode)>,
    mut texts: ParamSet<(
        Query<(&mut Text, &mut TextColor), With<PickName>>,
        Query<(&mut Text, &mut TextColor), With<LobbyStatusText>>,
        Query<&mut Text, With<LobbyIpText>>,
        Query<(&LobbySlotText, &mut Text, &mut TextColor)>,
        Query<(&mut Text, &mut TextColor), With<LobbyHintText>>,
    )>,
    mut cache: Local<(Option<u32>, bool, Vec<Vec<(u32, u8, u8)>>)>,
) {
    let have = creatures.is_some();
    if cache.0 != Some(selection.version) || cache.1 != have {
        *cache = (
            Some(selection.version),
            have,
            creatures.as_deref().map(all_looks).unwrap_or_default(),
        );
    }
    let all = &cache.2;
    let cur = selection
        .look
        .and_then(|i| selection.looks.get(i))
        .map(|&(_, r, s)| (r, s));

    let all_ready = !net.lobby_players.is_empty() && net.lobby_players.iter().all(|p| p.ready);

    for (action, interaction, children, lock, disabled, mut node, mut vis) in &mut buttons {
        let selected = match *action {
            MenuAction::Race(r) => cur.is_some_and(|c| c.0 == r),
            MenuAction::Sex(s) => cur.is_some_and(|c| c.1 == s),
            MenuAction::Class(i) => selection.class == Some(i),
            MenuAction::Map(i) => selection.map == i,
            _ => false,
        };
        if let Some(mut lock) = lock {
            if lock.0 != selected {
                lock.0 = selected;
            }
        }
        if let Some(mut disabled) = disabled {
            let off = match *action {
                MenuAction::Enter => {
                    cur.is_none() || (net.role == NetRole::Host && !all_ready)
                }
                MenuAction::LeaveNet => !net.is_online(),
                _ => false,
            };
            if disabled.0 != off {
                disabled.0 = off;
            }
        }
        match *action {
            // As on the create screen, a class the race cannot be leaves the grid.
            MenuAction::Class(i) => {
                let ok = cur.is_none_or(|(r, _)| {
                    all.get(i).is_some_and(|l| find(l, Some(r), None).is_some())
                });
                let display = if ok { Display::Flex } else { Display::None };
                if node.display != display {
                    node.display = display;
                }
            }
            MenuAction::Race(r) => {
                let ok = all.is_empty() || all.iter().any(|l| find(l, Some(r), None).is_some());
                let want = if ok {
                    Visibility::Inherited
                } else {
                    Visibility::Hidden
                };
                if *vis != want {
                    *vis = want;
                }
            }
            _ => {}
        }
        let show = selected || *interaction != Interaction::None || art.races.is_none();
        for child in children {
            if let Ok(mut label) = labels.get_mut(*child) {
                let want = if show {
                    Visibility::Inherited
                } else {
                    Visibility::Hidden
                };
                if *label != want {
                    *label = want;
                }
            }
        }
    }

    if let Ok(mut bc) = ip_box.single_mut() {
        *bc = BorderColor::all(if net.ip_focused {
            GOLD
        } else {
            Color::srgb(0.45, 0.45, 0.5)
        });
    }

    if let (Some((_, size)), Some((_, sex))) = (&art.races, cur) {
        for (face, mut img) in &mut faces {
            if let Some(tc) = race_tc(face.0, sex) {
                let rect = Some(tc_rect(*size, tc));
                if img.rect != rect {
                    img.rect = rect;
                }
            }
        }
    }

    let class = selection.class_def();
    if let Ok((mut t, mut color)) = texts.p0().single_mut() {
        let want = match (class, cur) {
            (Some(c), Some((r, _))) => format!("{} {}", race_name(r), c.name),
            _ => String::new(),
        };
        if t.0 != want {
            t.0 = want;
        }
        // In the class's own colour, as the game's raid frames and chat show it.
        let want = class.map_or(GOLD, |c| rgb(c.color));
        if color.0 != want {
            color.0 = want;
        }
    }

    // Update Lobby Status Text
    if let Ok((mut t, mut tc)) = texts.p1().single_mut() {
        if t.0 != net.status_text {
            t.0.clone_from(&net.status_text);
        }
        tc.0 = match net.role {
            NetRole::Offline => Color::srgb(0.7, 0.7, 0.7),
            NetRole::Host => Color::srgb(0.3, 0.95, 0.4),
            NetRole::Client => Color::srgb(0.4, 0.85, 1.0),
        };
    }

    // Update IP Input Text
    if let Ok(mut t) = texts.p2().single_mut() {
        let caret = if net.ip_focused { "|" } else { "" };
        let want = format!("{}{caret}", net.ip_input);
        if t.0 != want {
            t.0 = want;
        }
    }

    // Update 4-Slot Party Roster
    for (slot, mut t, mut tc) in &mut texts.p3() {
        let i = slot.0;
        if !net.is_online() {
            let want = if i == 0 {
                "Solo Mode — Click 'Host LAN' to host or enter ZeroTier/Tailscale IP & 'Join IP'"
                    .to_string()
            } else {
                String::new()
            };
            if t.0 != want {
                t.0 = want;
            }
            tc.0 = Color::srgb(0.65, 0.65, 0.65);
            continue;
        }
        if let Some(lp) = net.lobby_players.get(i) {
            let cls = &data::CLASSES[lp.class.min(data::CLASSES.len() - 1)];
            let badge = if lp.ready { "[READY]" } else { "[WAIT] " };
            let role_tag = if lp.id == 0 {
                " (Host)"
            } else if lp.id == net.local_id {
                " (You)"
            } else {
                ""
            };
            let want = format!(
                "{}. {badge} {}{} — {} {}",
                i + 1,
                lp.name,
                role_tag,
                race_name(lp.race),
                cls.name
            );
            if t.0 != want {
                t.0 = want;
            }
            tc.0 = if lp.ready {
                rgb(cls.color)
            } else {
                Color::srgb(0.75, 0.75, 0.75)
            };
        } else {
            let want = format!("{}. [Open Party Slot]", i + 1);
            if t.0 != want {
                t.0 = want;
            }
            tc.0 = Color::srgb(0.42, 0.42, 0.45);
        }
    }

    // Update Bottom Hint above Enter World button
    if let Ok((mut t, mut tc)) = texts.p4().single_mut() {
        let (want, col) = match net.role {
            NetRole::Offline => (String::new(), Color::WHITE),
            NetRole::Host => {
                if all_ready {
                    (
                        format!(
                            "All {} party member(s) READY — Press Enter World to launch!",
                            net.lobby_players.len()
                        ),
                        Color::srgb(0.35, 1.0, 0.45),
                    )
                } else {
                    (
                        "Waiting for all party members to click Ready...".to_string(),
                        GOLD,
                    )
                }
            }
            NetRole::Client => {
                if net.local_ready {
                    (
                        "You are READY! Waiting for Host to Enter World (Click button to unready)"
                            .to_string(),
                        Color::srgb(0.35, 1.0, 0.45),
                    )
                } else {
                    (
                        "Pick your Race & Class, then click 'Enter World' below to mark READY!"
                            .to_string(),
                        GOLD,
                    )
                }
            }
        };
        if t.0 != want {
            t.0 = want;
        }
        tc.0 = col;
    }
}
