//! Nearby mobile health and quick-action gump for ClassicUO.
//!
//! Rows are rebuilt from nearby mobiles with known hit points, sorted by range.
//! The window is an HTML/CSS page (`nearby.html`, `nearby.css`) that takes the
//! client's theme; this mod sizes it, fills it and answers its clicks.

use cuo_mod_sdk::assets::{self, ImageKind};
use cuo_mod_sdk::host::{PageChange, Scope};
use cuo_mod_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

const WINDOW: &str = "nearby-hpbars.root";
const PAGE_NAME: &str = "nearby";
const PAGE_HTML: &str = include_str!("nearby.html");
const PAGE_CSS: &str = include_str!("nearby.css");
const REFRESH_EVERY: u32 = 5;
const STATUS_RETRY_FRAMES: u32 = 300;
const FOLLOW_EVERY: u32 = 30;
const RANGE: u16 = 18;
/// The page's header is at least this tall, inside this body padding: the collapsed window's height.
const HEADER_HEIGHT: f32 = 21.0;
const PADDING: f32 = 4.0;
const WINDOW_WIDTH: f32 = 380.0;
const WINDOW_HEIGHT: f32 = 120.0;
const MIN_WIDTH: f32 = 200.0;
const MIN_HEIGHT: f32 = 80.0;
/// Frames after a collapse toggle during which the window size is not saved,
/// so the collapsed height never overwrites the user's size.
const SIZE_SETTLE_FRAMES: u32 = 10;
/// Two clicks on a name closer than this are a double click (the client's own is 300 ms between presses).
const DOUBLE_CLICK_MS: f32 = 400.0;
const NOTORIETIES: u8 = 8;
const ACTIONS: usize = 8;
const BACKPACK_LAYER: usize = 0x15;
const MAX_CONTAINER_DEPTH: usize = 8;
/// Paperdoll "Options" button art, the page's #options.
const OPTIONS_GUMP: u32 = 0x07D6;

fn setup(app: &mut App) {
    app.add_systems(Schedule::Update, update_gump);
    app.add_systems(Schedule::Update, (item_feed.run_on_change(), clicks));
    app.add_observer(on_mobile_removed);
    app.add_observer(on_item_uncontained);
}

#[derive(Default)]
struct UiState {
    /// The window root.
    page: Option<u64>,
    /// The page's #list: a reload of the page replaces it, and the rows are gone with it.
    list: Option<u64>,
    collapsed_view: Option<bool>,
    toggled_at: u32,
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

/// One positional row of the list: a copy of the page's row template. Rows are
/// reused in place and only the parts whose view changed are edited, so a moving
/// mobile does not restyle the whole list. A page has no call to remove one copy,
/// so a spare row is hidden (`view` none) until the list grows again.
struct RowSlot {
    row: u64,
    name: u64,
    label: u64,
    distance: u64,
    follow: u64,
    quick: [u64; 2],
    view: Option<RowView>,
}

// State shared between the click system and the update system. The guest is
// single-threaded, so plain atomics are enough.
/// The open window, 0 = none.
static PAGE: AtomicU64 = AtomicU64::new(0);
static FOLLOW: AtomicU32 = AtomicU32::new(0);
static RELOAD: AtomicBool = AtomicBool::new(false);
static OPTIONS_OPEN: AtomicBool = AtomicBool::new(false);
static COLLAPSE_TOGGLE: AtomicBool = AtomicBool::new(false);
/// Mobiles despawned since the update system last ran; it evicts them from its cache.
static REMOVED: Mutex<Vec<Entity>> = Mutex::new(Vec::new());
/// Every item inside a container, by entity: where a click finds a bandage, so
/// no system has to be sent all of them each frame.
static CONTAINED: Mutex<Option<HashMap<u64, Contained>>> = Mutex::new(None);

#[derive(Clone, Copy)]
struct Contained {
    serial: u32,
    graphic: u16,
    parent: u64,
}

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
    if let Some((_, node)) = state.page.and_then(|page| windows.get(Entity::from_bits(page))) {
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
        if let Some(page) = state.page {
            host::page_close(page);
            PAGE.store(0, Ordering::Relaxed);
            let ticks = state.ticks;
            let settings = state.settings.take();
            *state = UiState::default();
            state.ticks = ticks;
            state.settings = settings;
        }
        return;
    }
    if state.page.is_none() {
        let settings = state.settings.as_ref().unwrap();
        let Some(page) = open_window(&mut cmds, settings) else {
            return;
        };
        state.page = Some(page);
        PAGE.store(page, Ordering::Relaxed);
        state.collapsed_view = Some(settings.collapsed);
    }
    let page = state.page.unwrap();
    sync_page(state, page, &mut cmds);
    if COLLAPSE_TOGGLE.swap(false, Ordering::Relaxed) {
        let settings = state.settings.as_mut().unwrap();
        settings.collapsed = !settings.collapsed;
        settings.save();
    }
    sync_collapse(state, page, &mut cmds);
    sync_filters(state, page);
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

    if state.list.is_none() {
        return;
    }
    for slot in state.rows.iter_mut().skip(nearby.len()) {
        if slot.view.take().is_some() {
            host::page_edit_element(page, slot.row, &class(true, "hidden"));
        }
    }
    for (i, view) in nearby.into_iter().enumerate() {
        if i == state.rows.len() {
            let Some(slot) = clone_row(page, &mut cmds) else {
                break;
            };
            state.rows.push(slot);
        }
        update_row(page, &mut state.rows[i], view);
    }
}

fn class(on: bool, name: &str) -> PageChange {
    if on { PageChange::AddClass(name.into()) } else { PageChange::RemoveClass(name.into()) }
}

/// A button takes its own clicks, anywhere in its box, instead of dragging the window.
fn no_drag(cmds: &mut Commands, element: u64) {
    cmds.entity(Entity::from_bits(element)).insert((UiNoWindowDrag {}, UiContainsByBounds {}));
}

/// The page loaded or reloaded (the player saved its file, or switched theme):
/// its elements are new and the rows are gone, so they are cloned again.
fn sync_page(state: &mut UiState, page: u64, cmds: &mut Commands) {
    let list = host::page_element(page, "list");
    if list == state.list {
        return;
    }
    state.list = list;
    state.rows.clear();
    if list.is_none() {
        return;
    }
    let filters = (0..NOTORIETIES).map(|flag| format!("noto-{flag}"));
    for id in ["collapse".to_string(), "options".to_string()].into_iter().chain(filters) {
        if let Some(element) = host::page_element(page, &id) {
            no_drag(cmds, element);
        }
    }
}

/// Shrinks the window to its header (or restores the saved height) when the
/// collapsed flag changes. The root's whole `Node` is re-inserted, so its
/// current position is read back first to keep it where the user dragged it.
fn sync_collapse(state: &mut UiState, page: u64, cmds: &mut Commands) {
    let settings = state.settings.as_ref().unwrap();
    let collapsed = settings.collapsed;
    if state.collapsed_view == Some(collapsed) {
        return;
    }
    let Some(node) = &state.root_node else {
        return;
    };
    let mut node = node.clone();
    let (height, min_height) = window_height(settings);
    node.height = Val::px(height);
    node.min_height = Val::px(min_height);
    cmds.entity(Entity::from_bits(page)).insert(node);
    show_collapsed(page, collapsed);
    state.collapsed_view = Some(collapsed);
    state.toggled_at = state.ticks;
}

/// The page hides the list and the filters of a collapsed window.
fn show_collapsed(page: u64, collapsed: bool) {
    host::page_edit(page, "frame", &class(collapsed, "collapsed"));
    host::page_edit(page, "collapse", &PageChange::Text(collapse_label(collapsed).into()));
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

/// Shows the notoriety filters while they are open, and which ones are off.
fn sync_filters(state: &mut UiState, page: u64) {
    let open = OPTIONS_OPEN.load(Ordering::Relaxed);
    let hidden = state.settings.as_ref().unwrap().hidden.clone();
    let view = (open, hidden);
    if state.panel_view.as_ref() == Some(&view) {
        return;
    }
    host::page_edit(page, "frame", &class(view.0, "filters"));
    for flag in 0..NOTORIETIES {
        host::page_edit(page, &format!("noto-{flag}"), &class(view.1.contains(&flag), "off"));
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

/// A new copy of the row template at the end of the list.
fn clone_row(page: u64, cmds: &mut Commands) -> Option<RowSlot> {
    let row = host::page_clone(page, "list", "row")?;
    let part = |name: &str| host::page_part(page, row, name);
    let parts = (|| Some((part("name")?, part("label")?, part("distance")?, part("follow")?, part("quick0")?, part("quick1")?)))();
    let Some((name, label, distance, follow, quick0, quick1)) = parts else {
        // An edited page whose row lacks a part: don't pile up a copy per refresh.
        host::page_clear(page, "list");
        return None;
    };
    for button in [name, follow, quick0, quick1] {
        no_drag(cmds, button);
    }
    Some(RowSlot { row, name, label, distance, follow, quick: [quick0, quick1], view: None })
}

/// Edits the parts of a row whose view changed: all of them on a new or hidden row.
fn update_row(page: u64, slot: &mut RowSlot, view: RowView) {
    if slot.view.as_ref() == Some(&view) {
        return;
    }
    let old = slot.view.take();
    let changed = |same: &dyn Fn(&RowView) -> bool| !old.as_ref().is_some_and(same);
    let edit = |element: u64, change: PageChange| host::page_edit_element(page, element, &change);
    let attr = |element: u64, name: &str, value: String| edit(element, PageChange::Attr((name.into(), value)));
    if old.is_none() {
        edit(slot.row, class(false, "hidden"));
    }
    if changed(&|o| o.serial == view.serial) {
        attr(slot.name, "data-arg", format!("context:{}", view.serial));
        attr(slot.follow, "data-arg", format!("follow:{}", view.serial));
        for i in 0..2 {
            attr(slot.quick[i], "data-arg", format!("quick:{}:{i}", view.serial));
        }
    }
    if changed(&|o| name_text(o) == name_text(&view)) {
        edit(slot.label, PageChange::Text(name_text(&view)));
    }
    if changed(&|o| o.notoriety == view.notoriety) {
        attr(slot.name, "data-noto", view.notoriety.to_string());
    }
    if changed(&|o| o.distance == view.distance) {
        edit(slot.distance, PageChange::Text(view.distance.to_string()));
    }
    if changed(&|o| o.following == view.following) {
        edit(slot.follow, PageChange::Text(follow_text(&view).into()));
        edit(slot.follow, class(view.following, "on"));
    }
    for i in 0..2 {
        if changed(&|o| o.slots[i] == view.slots[i]) {
            edit(slot.quick[i], PageChange::Text(action_label(view.slots[i]).into()));
        }
    }
    slot.view = Some(view);
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

#[system]
fn on_mobile_removed(removed: On<Remove, Mobile>) {
    REMOVED.lock().unwrap().push(removed.entity());
}

/// True when `serial` was also clicked less than `DOUBLE_CLICK_MS` ago. A double click starts over.
fn is_double_click(last: &mut Option<(u32, f32)>, serial: u32, now: f32) -> bool {
    let double = last.is_some_and(|(s, at)| s == serial && now - at <= DOUBLE_CLICK_MS);
    *last = (!double).then_some((serial, now));
    double
}

/// Answers the page's clicks: the `data-arg` of each clicked element, and
/// `rclick:<x>:<y>:quick:<slot>` for a right-click on a quick button.
#[system]
fn clicks(
    mut last_name_click: Local<Option<(u32, f32)>>,
    time: Res<Time>,
    targeting: Option<Res<TargetingState>>,
    war_mode: Query<&ServerFlags, With<Player>>,
    player: Query<&EquipmentSlots, With<Player>>,
) {
    let page = PAGE.load(Ordering::Relaxed);
    if page == 0 {
        return;
    }
    for click in host::page_clicks(page) {
        let mut parts = click.split(':');
        let kind = parts.next().unwrap_or_default();
        let mut number = || parts.next().and_then(|part| part.parse::<u32>().ok());
        match kind {
            "context" => {
                let Some(serial) = number() else {
                    continue;
                };
                if targeting.as_ref().is_some_and(|t| t.is_targeting) {
                    actions::target_object(serial);
                    continue;
                }
                let in_war = war_mode
                    .single()
                    .is_some_and(|(_, flags)| flags.value & flags::WAR_MODE != 0);
                if in_war {
                    // A single click must not open the menu, or the double-click
                    // that attacks would land on the popup.
                    if is_double_click(&mut last_name_click, serial, time.total) {
                        actions::attack(serial);
                    }
                    continue;
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
                let Some(flag) = number().and_then(|flag| u8::try_from(flag).ok()) else {
                    continue;
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
                let Some(serial) = number() else {
                    continue;
                };
                if follow_serial() == serial {
                    set_follow(0);
                    actions::cancel_walk_to();
                } else {
                    set_follow(serial);
                }
            }
            "quick" => {
                let (Some(serial), Some(slot)) = (number(), number()) else {
                    continue;
                };
                let slot = slot as usize;
                let settings = cuo_mod_sdk::storage::load::<Settings>(Scope::Global)
                    .unwrap_or_else(Settings::new);
                let mode = settings.slots.get(slot).copied().unwrap_or(slot) % ACTIONS;
                if mode == 0 {
                    if let Some(bandage) = find_bandage(&player) {
                        actions::use_item_on(bandage, serial);
                    }
                } else if let Some(spell) = spell_id(mode) {
                    actions::cast_spell(spell);
                    actions::target_object(serial);
                }
            }
            "rclick" => {
                let Some(Ok(slot)) = click.rsplit(':').next().map(str::parse::<usize>) else {
                    continue;
                };
                let mut settings = cuo_mod_sdk::storage::load::<Settings>(Scope::Global)
                    .unwrap_or_else(Settings::new);
                let Some(mode) = settings.slots.get_mut(slot) else {
                    continue;
                };
                *mode = (*mode + 1) % ACTIONS;
                settings.save();
                // The update system reloads settings and relabels the rows.
                RELOAD.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

/// Keeps `CONTAINED` in step: an item entering a container or changing container
/// (its first sight counts), and a contained item changing graphic. Leaving is
/// `on_item_uncontained`.
#[system]
fn item_feed(
    moved: Query<(&Serial, &Graphic, &ChildOf), (With<ContainedInto>, Changed<ChildOf>)>,
    regraphed: Query<(&Serial, &Graphic, &ChildOf), (With<ContainedInto>, Changed<Graphic>)>,
) {
    let mut contained = CONTAINED.lock().unwrap();
    let contained = contained.get_or_insert_with(HashMap::new);
    for (entity, (serial, graphic, child)) in moved.iter().chain(regraphed.iter()) {
        let item = Contained { serial: serial.value, graphic: graphic.value, parent: child.parent.to_bits() };
        contained.insert(entity.to_bits(), item);
    }
}

/// A contained item left its container (dropped, equipped, used up) or the client.
/// Every contained item has a slot position and nothing else does: a `ChildOf`
/// removal would also fire for every UI node the client despawns.
#[system]
fn on_item_uncontained(gone: On<Remove, ContainerSlotPosition>) {
    if let Some(contained) = CONTAINED.lock().unwrap().as_mut() {
        contained.remove(&gone.entity().to_bits());
    }
}

/// A bandage anywhere inside the backpack, including nested bags.
fn find_bandage(player: &Query<&EquipmentSlots, With<Player>>) -> Option<u32> {
    let (_, equipment) = player.single()?;
    let backpack = *equipment.serials.get(BACKPACK_LAYER)?;
    let backpack = host::resolve_serial(backpack)?;
    bandage_in(CONTAINED.lock().unwrap().as_ref()?, backpack)
}

fn bandage_in(contained: &HashMap<u64, Contained>, backpack: u64) -> Option<u32> {
    contained.values().find_map(|item| {
        if item.graphic != helpers::BANDAGE {
            return None;
        }
        let mut parent = item.parent;
        for _ in 0..MAX_CONTAINER_DEPTH {
            if parent == backpack {
                return Some(item.serial);
            }
            parent = contained.get(&parent)?.parent;
        }
        None
    })
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

/// Opens the window where it always starts, at its saved size.
fn open_window(cmds: &mut Commands, settings: &Settings) -> Option<u64> {
    let page = host::page_spawn(PAGE_NAME, PAGE_HTML, PAGE_CSS, 30, 150)?;
    let (height, min_height) = window_height(settings);
    cmds.entity(Entity::from_bits(page)).insert((
        Node {
            display: display::FLEX,
            min_width: Val::px(MIN_WIDTH),
            min_height: Val::px(min_height),
            ..Node::abs(30.0, 150.0, settings.width, height)
        },
        UiName { value: WINDOW.into() },
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
    ));
    show_collapsed(page, settings.collapsed);
    Some(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bandage_is_found_in_the_backpack_and_its_bags_only() {
        let item = |serial, graphic, parent| Contained { serial, graphic, parent };
        let (backpack, bag, inner, bank) = (1, 2, 3, 9);
        let mut contained = HashMap::from([
            (bag, item(20, 0x0E75, backpack)),
            (inner, item(30, 0x0E75, bag)),
            (10, item(100, 0x0F0E, backpack)),
            (11, item(110, helpers::BANDAGE, bank)),
        ]);
        assert_eq!(bandage_in(&contained, backpack), None, "another container's bandage");
        contained.insert(12, item(120, helpers::BANDAGE, inner));
        assert_eq!(bandage_in(&contained, backpack), Some(120), "two bags deep");
        contained.remove(&12);
        assert_eq!(bandage_in(&contained, backpack), None, "it left its container");
        contained.insert(13, item(130, helpers::BANDAGE, 77));
        assert_eq!(bandage_in(&contained, backpack), None, "a parent nobody knows");
    }

    #[test]
    fn a_second_click_on_the_same_name_in_time_is_a_double_click() {
        let mut last = None;
        assert!(!is_double_click(&mut last, 7, 1000.0));
        assert!(!is_double_click(&mut last, 8, 1100.0), "another mobile");
        assert!(is_double_click(&mut last, 8, 1300.0));
        assert!(!is_double_click(&mut last, 8, 1400.0), "the third click starts over");
        assert!(!is_double_click(&mut last, 8, 1400.0 + DOUBLE_CLICK_MS + 1.0), "too late");
    }
}

export_mod!(setup);
