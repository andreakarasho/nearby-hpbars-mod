//! Nearby mobile health and quick-action gump for ClassicUO.
//!
//! Rows are rebuilt from nearby mobiles with known hit points, sorted by range.
//! The host owns movement, resizing, hit testing and gump rendering;
//! this mod supplies ordinary `cuo:ui/*` entities and uses client actions.

use cuo_mod_sdk::assets::{self, ImageKind};
use cuo_mod_sdk::host::Scope;
use cuo_mod_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const WINDOW: &str = "nearby-hpbars.root";
const ROW_PREFIX: &str = "nearby-hpbars.row.";
const REFRESH_EVERY: u32 = 5;
const STATUS_RETRY_FRAMES: u32 = 300;
const FOLLOW_EVERY: u32 = 30;
const RANGE: u16 = 18;
const FONT_SIZE: u16 = 16;
const ROW_HEIGHT: f32 = 23.0;
const HEADER_HEIGHT: f32 = 21.0;
const PADDING: f32 = 4.0;
const WINDOW_WIDTH: f32 = 380.0;
const WINDOW_HEIGHT: f32 = 120.0;
const MIN_WIDTH: f32 = 200.0;
const MIN_HEIGHT: f32 = 80.0;
const ACTION_WIDTH: f32 = 60.0;
const FOLLOW_WIDTH: f32 = 54.0;
const DISTANCE_WIDTH: f32 = 28.0;
const COLLAPSE_WIDTH: f32 = 20.0;
/// Frames after a collapse toggle during which the window size is not saved,
/// so the collapsed height never overwrites the user's size.
const SIZE_SETTLE_FRAMES: u32 = 10;
/// Any finite z: the host only needs the root to carry one to treat it as a
/// draggable window, and bumps it to the top on interaction.
const WINDOW_Z: i32 = 60;
const ACTIONS: usize = 8;
const BACKPACK_LAYER: usize = 0x15;
const MAX_CONTAINER_DEPTH: usize = 8;
/// TTF text: the host's regular font with its TtfFlag bit.
const FONT: u16 = 1 | 0x100;
/// Paperdoll "Options" button art.
const OPTIONS_GUMP: u32 = 0x07D6;
const NOTORIETY_NAMES: [&str; 8] =
    ["Unknown", "Innocent", "Ally", "Gray", "Criminal", "Enemy", "Murderer", "Invul"];

fn setup(app: &mut App) {
    app.add_systems(Schedule::Update, update_gump);
    app.add_observer(on_click);
    app.add_observer(configure_action);
    app.add_observer(on_mobile_removed);
}

#[derive(Default)]
struct UiState {
    root: Option<Entity>,
    list: Option<Entity>,
    options: Option<Entity>,
    collapse: Option<Entity>,
    collapsed_view: Option<bool>,
    toggled_at: u32,
    panel: Vec<Entity>,
    panel_view: Option<(bool, Vec<u8>)>,
    rows: Vec<RowSlot>,
    ticks: u32,
    settings: Option<Settings>,
    status_requests: HashMap<u32, u32>,
    /// Names / hits / notoriety of known mobiles, fed by `Changed` queries so the
    /// client only sends what changed instead of every mobile every frame.
    mobs: HashMap<Entity, MobInfo>,
    /// The root's last `Node`, fed by a `Changed<Node>` query.
    root_node: Option<Node>,
}

#[derive(Default)]
struct MobInfo {
    name: Option<String>,
    hits: Option<Hits>,
    notoriety: Option<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    /// Action per quick-button slot, shared by every mobile.
    slots: [usize; 2],
    /// Notorieties whose mobiles are not listed.
    hidden: Vec<u8>,
    width: f32,
    height: f32,
    collapsed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings::new()
    }
}

impl Settings {
    fn new() -> Self {
        Settings {
            slots: [0, 1],
            hidden: Vec::new(),
            width: WINDOW_WIDTH,
            height: WINDOW_HEIGHT,
            collapsed: false,
        }
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_string(self) {
            cuo_mod_sdk::host::storage_set(Scope::Global, &json);
        }
    }
}

/// Everything a row renders; a row's entities are only touched when this changes.
#[derive(Clone, PartialEq)]
struct RowView {
    serial: u32,
    name: String,
    hp_pct: Option<u32>,
    distance: u16,
    notoriety: u8,
    following: bool,
    slots: [usize; 2],
}

/// One positional row of the list. Rows are reused in place and only the
/// parts whose view changed get new components, so a moving mobile does not
/// rebuild (and relayout / re-measure) the whole list.
struct RowSlot {
    row: Entity,
    name: Entity,
    distance: Entity,
    follow: Entity,
    quick: [Entity; 2],
    view: RowView,
}

// State shared between the click observers and the update system. Observers
// get no `Local` of the system, and the guest is single-threaded, so plain
// atomics are enough.
static FOLLOW: AtomicU32 = AtomicU32::new(0);
static RELOAD: AtomicBool = AtomicBool::new(false);
static OPTIONS_OPEN: AtomicBool = AtomicBool::new(false);
static COLLAPSE_TOGGLE: AtomicBool = AtomicBool::new(false);
/// Mobiles despawned since the update system last ran; it evicts them from its cache.
static REMOVED: Mutex<Vec<Entity>> = Mutex::new(Vec::new());

fn follow_serial() -> u32 {
    FOLLOW.load(Ordering::Relaxed)
}

fn set_follow(serial: u32) {
    FOLLOW.store(serial, Ordering::Relaxed);
}

#[system]
fn update_gump(
    mut state: Local<UiState>,
    mut cmds: Commands,
    player: Query<(&Serial, &WorldPosition), With<Player>>,
    // Unfiltered: walking mobiles move without a change tick on the client.
    mobiles: Query<(&Serial, &WorldPosition), With<Mobile>>,
    names: Query<&EntityName, (With<Mobile>, Changed<EntityName>)>,
    hits: Query<&Hits, (With<Mobile>, Changed<Hits>)>,
    notorieties: Query<&Notoriety, (With<Mobile>, Changed<Notoriety>)>,
    // UiResizable first: the snapshot is driven by the first term, so this
    // only hands over resizable windows, not every UI node in the client.
    windows: Query<(&UiResizable, &Node), Changed<Node>>,
) {
    let state = &mut *state;
    state.ticks = state.ticks.wrapping_add(1);
    let now = state.ticks;

    // Changed rows arrive once, so they are folded in every frame, not only on refresh.
    for e in REMOVED.lock().unwrap().drain(..) {
        state.mobs.remove(&e);
    }
    for (e, name) in &names {
        state.mobs.entry(e).or_default().name = Some(name.value.clone());
    }
    for (e, h) in &hits {
        state.mobs.entry(e).or_default().hits = Some(h.clone());
    }
    for (e, n) in &notorieties {
        state.mobs.entry(e).or_default().notoriety = Some(n.value);
    }
    if let Some((_, node)) = state.root.and_then(|root| windows.get(root)) {
        state.root_node = Some(node.clone());
    }

    if RELOAD.swap(false, Ordering::Relaxed) {
        state.settings = None;
    }
    if state.settings.is_none() {
        let mut settings = cuo_mod_sdk::storage::load::<Settings>(Scope::Global)
            .unwrap_or_else(Settings::new);
        settings.width = settings.width.max(MIN_WIDTH);
        settings.height = settings.height.max(MIN_HEIGHT);
        for slot in &mut settings.slots {
            *slot %= ACTIONS;
        }
        state.settings = Some(settings);
    }
    // The player entity only exists once the client is in the world: no gump
    // on the login / character screens, and tear it down again on logout.
    if player.single().is_none() {
        if let Some(root) = state.root {
            cmds.entity(root).despawn();
            let ticks = state.ticks;
            let settings = state.settings.take();
            *state = UiState::default();
            state.ticks = ticks;
            state.settings = settings;
        }
        return;
    }
    if state.root.is_none() {
        let settings = state.settings.as_ref().unwrap();
        let w = spawn_window(&mut cmds, settings);
        state.root = Some(w.root);
        state.options = Some(w.options);
        state.list = Some(w.list);
        state.collapse = Some(w.collapse);
        state.collapsed_view = Some(settings.collapsed);
    }
    if COLLAPSE_TOGGLE.swap(false, Ordering::Relaxed) {
        let settings = state.settings.as_mut().unwrap();
        settings.collapsed = !settings.collapsed;
        settings.save();
    }
    sync_collapse(state, &mut cmds);
    sync_options_panel(state, &mut cmds);
    if now % REFRESH_EVERY != 1 {
        return;
    }

    persist_size(state);

    let Some((_, (player_serial, player_pos))) = player.single() else {
        return;
    };
    let slots = state.settings.as_ref().unwrap().slots;
    let hidden = state.settings.as_ref().unwrap().hidden.clone();
    let follow = follow_serial();
    let mut follow_seen = false;

    let mut nearby = Vec::new();
    let mut in_range = HashSet::new();
    for (mobile_entity, (serial, pos)) in &mobiles {
        if serial.value == 0 || serial.value == player_serial.value {
            continue;
        }
        let Some(info) = state.mobs.get(&mobile_entity) else {
            continue;
        };
        let Some(name) = &info.name else {
            continue;
        };
        let distance = pos.x.abs_diff(player_pos.x).max(pos.y.abs_diff(player_pos.y));
        if serial.value == follow {
            follow_seen = true;
            if distance > 1 && now % FOLLOW_EVERY == 1 {
                actions::walk_to_within(actions::Point { x: pos.x, y: pos.y, z: pos.z }, 1);
            }
        }
        if distance > RANGE {
            continue;
        }
        in_range.insert(serial.value);
        let hp = info
            .hits
            .as_ref()
            .and_then(|h| (h.max_value > 0).then_some((h.value, h.max_value)));
        if hp.is_none() {
            let last = state.status_requests.get(&serial.value).copied();
            if last.is_none_or(|frame| now.wrapping_sub(frame) >= STATUS_RETRY_FRAMES) {
                actions::request_status(Some(serial.value));
                state.status_requests.insert(serial.value, now);
            }
        }
        let notoriety = info.notoriety.unwrap_or(notoriety_flag::UNKNOWN);
        if hidden.contains(&notoriety) {
            continue;
        }
        nearby.push(RowView {
            serial: serial.value,
            name: name.clone(),
            hp_pct: hp.map(|(hp, max)| ((u32::from(hp) * 100) / u32::from(max)).min(100)),
            distance,
            notoriety,
            following: serial.value == follow,
            slots,
        });
    }
    if follow != 0 && !follow_seen {
        set_follow(0);
    }
    state.status_requests.retain(|serial, _| in_range.contains(serial));
    nearby.sort_by(|a, b| a.distance.cmp(&b.distance).then(a.serial.cmp(&b.serial)));

    for slot in state.rows.drain(nearby.len().min(state.rows.len())..) {
        cmds.entity(slot.row).despawn();
    }
    let list = state.list.unwrap();
    for (i, view) in nearby.into_iter().enumerate() {
        match state.rows.get_mut(i) {
            Some(slot) => update_row(&mut cmds, slot, view),
            None => {
                let slot = spawn_row(&mut cmds, list, view);
                state.rows.push(slot);
            }
        }
    }
}

/// Shrinks the window to its header (or restores the saved height) when the
/// collapsed flag changes. The root's whole `Node` is re-inserted, so its
/// current position is read back first to keep it where the user dragged it.
fn sync_collapse(state: &mut UiState, cmds: &mut Commands) {
    let settings = state.settings.as_ref().unwrap();
    let collapsed = settings.collapsed;
    if state.collapsed_view == Some(collapsed) {
        return;
    }
    let (Some(root), Some(list), Some(options)) = (state.root, state.list, state.options) else {
        return;
    };
    let Some(node) = &state.root_node else {
        return;
    };
    let mut node = node.clone();
    let (height, min_height) = window_height(settings);
    node.height = Val::px(height);
    node.min_height = Val::px(min_height);
    cmds.entity(root).insert(node);
    let display = if collapsed { display::NONE } else { display::FLEX };
    cmds.entity(list).insert(list_node(display));
    cmds.entity(options).insert(options_node(display));
    if let Some(button) = state.collapse {
        cmds.entity(button).insert(Text { value: collapse_label(collapsed).into() });
    }
    state.collapsed_view = Some(collapsed);
    state.toggled_at = state.ticks;
}

fn window_height(settings: &Settings) -> (f32, f32) {
    if settings.collapsed {
        let h = header_height() + PADDING * 2.0;
        (h, h)
    } else {
        (settings.height, MIN_HEIGHT)
    }
}

fn collapse_label(collapsed: bool) -> &'static str {
    if collapsed { "+" } else { "-" }
}

/// Rebuilds the notoriety-filter toggles when the panel opens, closes or a
/// filter changes.
fn sync_options_panel(state: &mut UiState, cmds: &mut Commands) {
    let open = OPTIONS_OPEN.load(Ordering::Relaxed);
    let hidden = state.settings.as_ref().unwrap().hidden.clone();
    let view = (open, hidden);
    if state.panel_view.as_ref() == Some(&view) {
        return;
    }
    for e in state.panel.drain(..) {
        cmds.entity(e).despawn();
    }
    let options = state.options.unwrap();
    if view.0 {
        for chunk in 0..2u8 {
            let row = cmds
                .spawn((
                    Node {
                        flex_direction: flex_direction::ROW,
                        width: Val::percent(100.0),
                        height: Val::px(ROW_HEIGHT),
                        gap: Val::px(2.0),
                        ..Node::base()
                    },
                    ChildOf { parent: options },
                ))
                .id();
            for flag in chunk * 4..chunk * 4 + 4 {
                let shown = !view.1.contains(&flag);
                let color = if shown { notoriety_color(flag) } else { rgb(40, 40, 46) };
                spawn_button(
                    cmds,
                    row,
                    &format!("{ROW_PREFIX}noto:{flag}"),
                    NOTORIETY_NAMES[flag as usize],
                    Val::grow(),
                    color,
                );
            }
            state.panel.push(row);
        }
    }
    state.panel_view = Some(view);
}

fn persist_size(state: &mut UiState) {
    if state.ticks.wrapping_sub(state.toggled_at) < SIZE_SETTLE_FRAMES {
        return;
    }
    let (Some(settings), Some(node)) = (state.settings.as_mut(), state.root_node.as_ref()) else {
        return;
    };
    if node.width.type_ != val_type::PX || node.height.type_ != val_type::PX {
        return;
    }
    let width = node.width.value.max(MIN_WIDTH);
    // Collapsed, only the width is the user's; the height is the header's.
    let height = node.height.value.max(MIN_HEIGHT);
    let height_changed = !settings.collapsed && (height - settings.height).abs() >= 1.0;
    if (width - settings.width).abs() >= 1.0 || height_changed {
        settings.width = width;
        if !settings.collapsed {
            settings.height = height;
        }
        settings.save();
    }
}

fn spawn_row(cmds: &mut Commands, list: Entity, view: RowView) -> RowSlot {
    let row = cmds
        .spawn((
            Node {
                flex_direction: flex_direction::ROW,
                align_items: align_items::CENTER,
                width: Val::percent(100.0),
                height: Val::px(ROW_HEIGHT),
                gap: Val::px(3.0),
                ..Node::base()
            },
            ChildOf { parent: list },
        ))
        .id();
    let name = spawn_button(
        cmds,
        row,
        &name_key(&view),
        &name_text(&view),
        Val::grow(),
        notoriety_color(view.notoriety),
    );
    let distance = spawn_label(cmds, row, &view.distance.to_string(), DISTANCE_WIDTH);
    let follow = spawn_button(
        cmds,
        row,
        &follow_key(&view),
        follow_text(&view),
        Val::px(FOLLOW_WIDTH),
        follow_color(&view),
    );
    let quick = [0, 1].map(|slot| {
        spawn_button(
            cmds,
            row,
            &quick_key(&view, slot),
            action_label(view.slots[slot]),
            Val::px(ACTION_WIDTH),
            quick_color(slot),
        )
    });
    RowSlot { row, name, distance, follow, quick, view }
}

fn update_row(cmds: &mut Commands, slot: &mut RowSlot, view: RowView) {
    let old = &slot.view;
    if *old == view {
        return;
    }
    let serial = old.serial != view.serial;
    if serial || old.name != view.name || old.hp_pct != view.hp_pct || old.following != view.following {
        cmds.entity(slot.name).insert(Text { value: name_text(&view) });
    }
    if serial || old.notoriety != view.notoriety {
        cmds.entity(slot.name)
            .insert(BackgroundColor { value: notoriety_color(view.notoriety) });
    }
    if old.distance != view.distance {
        cmds.entity(slot.distance).insert(Text { value: view.distance.to_string() });
    }
    if serial || old.following != view.following {
        cmds.entity(slot.follow).insert((
            Text { value: follow_text(&view).into() },
            BackgroundColor { value: follow_color(&view) },
        ));
    }
    if serial {
        cmds.entity(slot.name).insert(UiName { value: name_key(&view) });
        cmds.entity(slot.follow).insert(UiName { value: follow_key(&view) });
    }
    for i in 0..2 {
        if serial {
            cmds.entity(slot.quick[i]).insert(UiName { value: quick_key(&view, i) });
        }
        if old.slots[i] != view.slots[i] {
            cmds.entity(slot.quick[i]).insert(Text { value: action_label(view.slots[i]).into() });
        }
    }
    slot.view = view;
}

fn name_key(view: &RowView) -> String {
    format!("{ROW_PREFIX}context:{}", view.serial)
}

fn follow_key(view: &RowView) -> String {
    format!("{ROW_PREFIX}follow:{}", view.serial)
}

fn quick_key(view: &RowView, slot: usize) -> String {
    format!("{ROW_PREFIX}quick:{}:{slot}", view.serial)
}

fn name_text(view: &RowView) -> String {
    let hp = match view.hp_pct {
        Some(pct) if pct < 100 => format!(" {pct}%"),
        Some(_) => String::new(),
        None => " ?".to_string(),
    };
    let mark = if view.following { "> " } else { "" };
    format!("{mark}{}{hp}", view.name)
}

fn follow_text(view: &RowView) -> &'static str {
    if view.following { "Stop" } else { "Follow" }
}

fn follow_color(view: &RowView) -> Color {
    if view.following { rgb(150, 110, 40) } else { rgb(70, 70, 80) }
}

fn quick_color(slot: usize) -> Color {
    if slot == 0 { rgb(65, 130, 75) } else { rgb(70, 105, 155) }
}

#[system]
fn on_mobile_removed(removed: On<Remove, Mobile>) {
    REMOVED.lock().unwrap().push(removed.entity());
}

#[system]
fn on_click(
    click: On<Event, UiClick>,
    names: Query<&UiName>,
    mouse: Res<MouseInput>,
    targeting: Option<Res<TargetingState>>,
    war_mode: Query<&ServerFlags, With<Player>>,
    items: Query<(&Serial, &Graphic, &ChildOf)>,
    player: Query<&EquipmentSlots, With<Player>>,
) {
    let Some(name) = names.get(click.entity()) else {
        return;
    };
    let Some(data) = name.value.strip_prefix(ROW_PREFIX) else {
        return;
    };
    let Some((kind, tail)) = data.split_once(':') else {
        return;
    };
    let mut parts = tail.split(':');
    let Some(Ok(serial)) = parts.next().map(str::parse::<u32>) else {
        return;
    };
    match kind {
        "context" => {
            if targeting.as_ref().is_some_and(|t| t.is_targeting) {
                actions::target_object(serial);
                return;
            }
            let in_war = war_mode
                .single()
                .is_some_and(|(_, flags)| flags.value & flags::WAR_MODE != 0);
            if in_war {
                // A single click must not open the menu, or the double-click
                // that attacks would land on the popup.
                if mouse.left_double {
                    actions::attack(serial);
                }
                return;
            }
            actions::request_context_menu(serial);
        }
        "collapse" => {
            COLLAPSE_TOGGLE.store(true, Ordering::Relaxed);
        }
        "options" => {
            OPTIONS_OPEN.fetch_xor(true, Ordering::Relaxed);
        }
        "noto" => {
            let Ok(flag) = u8::try_from(serial) else {
                return;
            };
            let mut settings = cuo_mod_sdk::storage::load::<Settings>(Scope::Global)
                .unwrap_or_else(Settings::new);
            if let Some(i) = settings.hidden.iter().position(|&h| h == flag) {
                settings.hidden.remove(i);
            } else {
                settings.hidden.push(flag);
            }
            settings.save();
            RELOAD.store(true, Ordering::Relaxed);
        }
        "follow" => {
            if follow_serial() == serial {
                set_follow(0);
                actions::cancel_walk_to();
            } else {
                set_follow(serial);
            }
        }
        "quick" => {
            let Some(Ok(slot)) = parts.next().map(str::parse::<usize>) else {
                return;
            };
            let settings = cuo_mod_sdk::storage::load::<Settings>(Scope::Global)
                .unwrap_or_else(Settings::new);
            let mode = settings.slots.get(slot).copied().unwrap_or(slot) % ACTIONS;
            if mode == 0 {
                if let Some(bandage) = find_bandage(&items, &player) {
                    actions::use_item_on(bandage, serial);
                }
            } else if let Some(spell) = spell_id(mode) {
                actions::cast_spell(spell);
                actions::target_object(serial);
            }
        }
        _ => {}
    }
}

/// A bandage anywhere inside the backpack, including nested bags.
fn find_bandage(
    items: &Query<(&Serial, &Graphic, &ChildOf)>,
    player: &Query<&EquipmentSlots, With<Player>>,
) -> Option<u32> {
    let (_, equipment) = player.single()?;
    let backpack = *equipment.serials.get(BACKPACK_LAYER)?;
    let backpack = host::resolve_serial(backpack)?;
    let parents: HashMap<u64, u64> = items
        .iter()
        .map(|(entity, (_, _, child))| (entity.to_bits(), child.parent.to_bits()))
        .collect();
    items.iter().find_map(|(entity, (serial, graphic, _))| {
        if graphic.value != helpers::BANDAGE {
            return None;
        }
        let mut current = entity.to_bits();
        for _ in 0..MAX_CONTAINER_DEPTH {
            let parent = *parents.get(&current)?;
            if parent == backpack {
                return Some(serial.value);
            }
            current = parent;
        }
        None
    })
}

#[system]
fn configure_action(click: On<Event, UiRightClick>, names: Query<&UiName>) {
    let Some(name) = names.get(click.entity()) else {
        return;
    };
    let Some(tail) = name
        .value
        .strip_prefix(ROW_PREFIX)
        .and_then(|data| data.strip_prefix("quick:"))
    else {
        return;
    };
    let Some(Ok(slot)) = tail.split(':').nth(1).map(str::parse::<usize>) else {
        return;
    };
    let mut settings = cuo_mod_sdk::storage::load::<Settings>(Scope::Global)
        .unwrap_or_else(Settings::new);
    let Some(mode) = settings.slots.get_mut(slot) else {
        return;
    };
    *mode = (*mode + 1) % ACTIONS;
    settings.save();
    // The update system reloads settings and rebuilds rows with the new label.
    RELOAD.store(true, Ordering::Relaxed);
}

fn action_label(mode: usize) -> &'static str {
    ["Bandage", "Heal", "G Heal", "Cure", "A Cure", "Curse", "Wounds", "Winds"][mode % ACTIONS]
}

fn spell_id(mode: usize) -> Option<u16> {
    match mode {
        1 => Some(4),   // Heal
        2 => Some(29),  // Greater Heal
        3 => Some(11),  // Cure
        4 => Some(28),  // Arch Cure
        5 => Some(209), // Remove Curse
        6 => Some(202), // Close Wounds
        7 => Some(688), // Cleansing Winds
        _ => None,
    }
}

struct Window {
    root: Entity,
    options: Entity,
    list: Entity,
    collapse: Entity,
}

/// The header is at least as tall as the options art.
fn header_height() -> f32 {
    options_art_size().1.max(HEADER_HEIGHT)
}

/// The art's own size; none = no assets loaded (headless host).
fn options_art_size() -> (f32, f32) {
    assets::image_size(ImageKind::Gump, OPTIONS_GUMP)
        .map(|s| (s.width as f32, s.height as f32))
        .unwrap_or((60.0, 22.0))
}

fn list_node(display: u8) -> Node {
    Node {
        flex_direction: flex_direction::COLUMN,
        width: Val::percent(100.0),
        height: Val::grow(),
        gap: Val::px(2.0),
        overflow: overflow::SCROLL,
        display,
        ..Node::base()
    }
}

fn options_node(display: u8) -> Node {
    Node {
        flex_direction: flex_direction::COLUMN,
        width: Val::percent(100.0),
        gap: Val::px(2.0),
        display,
        ..Node::base()
    }
}

fn spawn_window(cmds: &mut Commands, settings: &Settings) -> Window {
    let (height, min_height) = window_height(settings);
    let display = if settings.collapsed { display::NONE } else { display::FLEX };
    let root = cmds
        .spawn((
            Node {
                flex_direction: flex_direction::COLUMN,
                min_width: Val::px(MIN_WIDTH),
                min_height: Val::px(min_height),
                padding: UiRect::splat(Val::px(PADDING)),
                gap: Val::px(2.0),
                overflow: overflow::CLIP,
                ..Node::abs(30.0, 150.0, settings.width, height)
            },
            UiName { value: WINDOW.into() },
            UiMovable {},
            GlobalZIndex { value: WINDOW_Z },
            UiResizable {
                min_w: MIN_WIDTH,
                min_h: MIN_HEIGHT,
                max_w: 900.0,
                max_h: 700.0,
                grip: 10.0,
                grip_inset: 0.0,
                whole_pixels: true,
            },
            UiNoRightClickClose {},
            UiContainsByBounds {},
            BackgroundColor { value: rgb(18, 22, 29) },
            BorderRadius::all(3.0),
        ))
        .id();
    let (bw, bh) = options_art_size();
    let header = cmds
        .spawn((
            Node {
                flex_direction: flex_direction::ROW,
                align_items: align_items::CENTER,
                width: Val::percent(100.0),
                height: Val::px(header_height()),
                gap: Val::px(4.0),
                ..Node::base()
            },
            ChildOf { parent: root },
        ))
        .id();
    let collapse = spawn_button(
        cmds,
        header,
        &format!("{ROW_PREFIX}collapse:0"),
        collapse_label(settings.collapsed),
        Val::px(COLLAPSE_WIDTH),
        rgb(55, 60, 72),
    );
    cmds.spawn((
        Node {
            width: Val::grow(),
            height: Val::px(HEADER_HEIGHT),
            overflow: overflow::CLIP,
            ..Node::base()
        },
        Text { value: "Nearby".into() },
        TextFont { font_id: FONT, size: FONT_SIZE },
        TextColor { value: rgb(205, 205, 205) },
        ChildOf { parent: header },
    ));
    cmds.spawn((
        Node { width: Val::px(bw), height: Val::px(bh), ..Node::base() },
        UiCustom { kind: 0, asset_id: OPTIONS_GUMP, hue_z: 1.0, ..Default::default() },
        Interaction::NONE,
        UiNoWindowDrag {},
        UiContainsByBounds {},
        UiName { value: format!("{ROW_PREFIX}options:0") },
        ChildOf { parent: header },
    ));
    let options = cmds.spawn((options_node(display), ChildOf { parent: root })).id();
    let list = cmds
        .spawn((list_node(display), ScrollPosition::default(), ChildOf { parent: root }))
        .id();
    Window { root, options, list, collapse }
}

fn spawn_label(cmds: &mut Commands, parent: Entity, text: &str, width: f32) -> Entity {
    cmds.spawn((
        Node {
            width: Val::px(width),
            height: Val::px(ROW_HEIGHT - 2.0),
            justify_content: justify_content::CENTER,
            align_items: align_items::CENTER,
            ..Node::base()
        },
        Text { value: text.into() },
        TextFont { font_id: FONT, size: FONT_SIZE },
        TextColor { value: Color::rgb(205, 205, 205) },
        ChildOf { parent },
    ))
    .id()
}

fn spawn_button(
    cmds: &mut Commands,
    parent: Entity,
    name: &str,
    text: &str,
    width: Val,
    color: Color,
) -> Entity {
    cmds.spawn((
        Node {
            width,
            height: Val::px(ROW_HEIGHT - 2.0),
            justify_content: justify_content::CENTER,
            align_items: align_items::CENTER,
            overflow: overflow::CLIP,
            ..Node::base()
        },
        BackgroundColor { value: color },
        BorderRadius::all(2.0),
        Text { value: text.into() },
        TextFont { font_id: FONT, size: FONT_SIZE },
        TextColor { value: Color::rgb(245, 245, 245) },
        Interaction::NONE,
        UiNoWindowDrag {},
        UiContainsByBounds {},
        UiName { value: name.into() },
        ChildOf { parent },
    ))
    .id()
}

fn notoriety_color(notoriety: u8) -> Color {
    match notoriety {
        notoriety_flag::INNOCENT => rgb(57, 112, 190),
        notoriety_flag::ALLY => rgb(54, 155, 84),
        notoriety_flag::GRAY => rgb(150, 150, 150),
        notoriety_flag::CRIMINAL => rgb(190, 165, 55),
        notoriety_flag::ENEMY => rgb(190, 95, 45),
        notoriety_flag::MURDERER => rgb(185, 55, 55),
        notoriety_flag::INVULNERABLE => rgb(140, 80, 170),
        _ => rgb(80, 80, 80),
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::rgb(r, g, b)
}

export_mod!(setup);
