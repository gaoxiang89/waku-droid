//! Native GPUI honeycomb visualization ("Hive" & "Mentions" + Timeline Replay)
//! for Antigravity Swarms.
//!
//! Layout geometry, communication graph derivation, and replay pacing/telemetry
//! are computed via pure helpers and unit-tested directly without requiring a
//! GPUI window. Rendering rides the shared [`crate::ui::motion::pulse`] clock at
//! ~15 fps only while at least one bee is working or replay playback is active,
//! dropping to 0 Hz CPU when idle.

use std::cell::Cell;
use std::collections::HashMap;
use std::f32::consts::{PI, TAU};
use std::rc::Rc;

use chrono::{DateTime, NaiveDateTime, NaiveTime};
use gpui::{
    Bounds, ClickEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, SharedString,
};
use waku_client::antigravity_swarm::{
    AntigravitySwarmMember, AntigravitySwarmMemberStatus, AntigravitySwarmMessage,
    AntigravitySwarmSnapshot,
};

use super::swarm_panel::{
    SwarmPanelTab, localized_member_activity, member_display_status, member_status_color,
    member_status_icon, member_status_label, short_board_time, swarm_badge,
};
use super::*;
use crate::ui::ActivationExt as _;

const HEX_HEIGHT_RATIO: f32 = 1.732_050_8; // sqrt(3) for flat-top hexagon

const WORKER_SPRITE_COLS: usize = 15;
const WORKER_SPRITE_ROWS: usize = 12;
const WORKER_BEE_UP: [&str; WORKER_SPRITE_ROWS] = [
    ".......wW......",
    "......wWWw.wW..",
    "......wWWwwWWw.",
    "...ooooWwwwwW..",
    "..ohbbbooooo...",
    ".ohbbbkbkbkbo..",
    ".oebbbkbkbkbsot",
    ".oebgbbbbbbbs..",
    "..osbbkbkbkbo..",
    "...ooooooooo...",
    ".....o...o.....",
    "...............",
];
const WORKER_BEE_DOWN: [&str; WORKER_SPRITE_ROWS] = [
    "...............",
    "...............",
    "......wW..wW...",
    "...ooowWWwWWo..",
    "..ohbbwwWWwbo..",
    ".ohbbbkbkbkbo..",
    ".oebbbkbkbkbsot",
    ".oebgbbbbbbbs..",
    "..osbbkbkbkbo..",
    "...ooooooooo...",
    ".....o...o.....",
    "...............",
];

const QUEEN_SPRITE_COLS: usize = 16;
const QUEEN_SPRITE_ROWS: usize = 14;
const QUEEN_BEE_UP: [&str; QUEEN_SPRITE_ROWS] = [
    "....C.C.C.......",
    "....ccccc.wW....",
    "......wWWwWWw...",
    "......wWWwwWWw..",
    "...ooooWwwwwW...",
    "..ohbbbbooooo...",
    ".ohbbbbkbkbkbo..",
    ".oebbbbkbkbkbsot",
    ".oebgbbbbbbbbs..",
    "..osbbbkbkbkbo..",
    "...osbbkbkbko...",
    "....oooooooo....",
    "......o...o.....",
    "................",
];
const QUEEN_BEE_DOWN: [&str; QUEEN_SPRITE_ROWS] = [
    "....C.C.C.......",
    "....ccccc.......",
    "......wW..wW....",
    "...ooowWWwWWo...",
    "..ohbbwwWWwbo...",
    "..ohbbbbooooo...",
    ".ohbbbbkbkbkbo..",
    ".oebbbbkbkbkbsot",
    ".oebgbbbbbbbbs..",
    "..osbbbkbkbkbo..",
    "...osbbkbkbko...",
    "....oooooooo....",
    "......o...o.....",
    "................",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SwarmHiveMode {
    #[default]
    Hive,
    Mentions,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SwarmHivePacing {
    #[default]
    Even,
    Real,
}

impl SwarmHivePacing {
    fn toggle(self) -> Self {
        match self {
            Self::Even => Self::Real,
            Self::Real => Self::Even,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Even => tr!("swarm.pacing_even"),
            Self::Real => tr!("swarm.pacing_real"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HiveSceneCacheKey {
    session_id: Uuid,
    snapshot_fp: String,
    width_bucket: i32,
    mode: SwarmHiveMode,
    show_killed: bool,
    selected_member: Option<String>,
    replay_cursor_seq: Option<u64>,
}

pub(super) struct SwarmHiveState {
    pub(super) mode: SwarmHiveMode,
    pub(super) pacing: SwarmHivePacing,
    pub(super) selected_member: Option<String>,
    pub(super) replay_cursor_seq: Option<u64>,
    pub(super) replay_playing: bool,
    pub(super) replay_generation: u64,
    pub(super) scroll: ScrollHandle,
    pub(super) scrollbar: Rc<ScrollbarState>,
    scrub_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    scrub_dragging: bool,
    cached_scene: Option<(HiveSceneCacheKey, Arc<HiveSceneLayout>)>,
    requested_scene: Option<HiveSceneCacheKey>,
    scene_generation: u64,
    scene_in_flight: bool,
    mode_hive_focus: FocusHandle,
    mode_mentions_focus: FocusHandle,
    pacing_focus: FocusHandle,
    replay_prev_focus: FocusHandle,
    replay_play_focus: FocusHandle,
    replay_next_focus: FocusHandle,
    replay_live_focus: FocusHandle,
    scrub_bar_focus: FocusHandle,
    view_in_board_focus: FocusHandle,
    bee_focuses: RefCell<HashMap<String, FocusHandle>>,
}

impl SwarmHiveState {
    pub(super) fn new(cx: &mut Context<Waku>) -> Self {
        Self {
            mode: SwarmHiveMode::Hive,
            pacing: SwarmHivePacing::Even,
            selected_member: None,
            replay_cursor_seq: None,
            replay_playing: false,
            replay_generation: 0,
            scroll: ScrollHandle::new(),
            scrollbar: ScrollbarState::new(),
            scrub_bounds: Rc::new(Cell::new(None)),
            scrub_dragging: false,
            cached_scene: None,
            requested_scene: None,
            scene_generation: 0,
            scene_in_flight: false,
            mode_hive_focus: cx.focus_handle(),
            mode_mentions_focus: cx.focus_handle(),
            pacing_focus: cx.focus_handle(),
            replay_prev_focus: cx.focus_handle(),
            replay_play_focus: cx.focus_handle(),
            replay_next_focus: cx.focus_handle(),
            replay_live_focus: cx.focus_handle(),
            scrub_bar_focus: cx.focus_handle(),
            view_in_board_focus: cx.focus_handle(),
            bee_focuses: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn stop_replay(&mut self) {
        self.replay_playing = false;
        self.replay_generation = self.replay_generation.wrapping_add(1);
    }

    pub(super) fn reset_for_session_change(&mut self) {
        self.selected_member = None;
        self.replay_cursor_seq = None;
        self.scrub_dragging = false;
        self.cached_scene = None;
        self.requested_scene = None;
        self.scene_generation = self.scene_generation.wrapping_add(1);
        self.scroll.set_offset(gpui::Point::default());
        self.bee_focuses.borrow_mut().clear();
        self.stop_replay();
    }

    fn bee_focus(&self, name: &str, cx: &mut Context<Waku>) -> FocusHandle {
        let mut focuses = self.bee_focuses.borrow_mut();
        if let Some(handle) = focuses.get(name) {
            return handle.clone();
        }
        let handle = cx.focus_handle();
        focuses.insert(name.to_owned(), handle.clone());
        handle
    }
}

#[derive(Clone, Debug)]
struct HiveChamberLayout {
    title: String,
    subtitle: String,
    cx: f32,
    cy: f32,
    radius: f32,
    status_accent: Option<AntigravitySwarmMemberStatus>,
    label_at_bottom: bool,
}

#[derive(Clone, Debug)]
struct HiveBeeNode {
    name: String,
    is_coordinator: bool,
    status: AntigravitySwarmMemberStatus,
    killed: bool,
    cx: f32,
    cy: f32,
    pixel_size: f32,
    label_width: f32,
    phase_seed: f32,
    highlighted: bool,
    selected: bool,
}

#[derive(Clone, Debug)]
struct HiveArcEdge {
    from: (f32, f32),
    to: (f32, f32),
    bow: f32,
    opacity: f32,
    stroke_width: f32,
    dashed: bool,
    active_pulse: bool,
    highlighted: bool,
    phase_offset: f32,
}

#[derive(Default)]
struct HiveSceneLayout {
    stage_width: f32,
    stage_height: f32,
    chambers: Vec<HiveChamberLayout>,
    bees: Vec<HiveBeeNode>,
    edges: Vec<HiveArcEdge>,
    any_working: bool,
}

#[derive(Clone, Copy)]
struct BeePalette {
    body: Hsla,
    highlight: Hsla,
    shadow: Hsla,
    stripe: Hsla,
    outline: Hsla,
}

fn bee_palette(name: &str, is_coordinator: bool, is_dark: bool, alpha: f32) -> BeePalette {
    if is_coordinator {
        return BeePalette {
            body: gpui::hsla(40.0 / 360.0, 0.90, if is_dark { 0.56 } else { 0.48 }, alpha),
            highlight: gpui::hsla(48.0 / 360.0, 0.96, 0.74, alpha),
            shadow: gpui::hsla(28.0 / 360.0, 0.85, 0.34, alpha),
            stripe: gpui::hsla(18.0 / 360.0, 0.72, 0.18, alpha),
            outline: gpui::hsla(24.0 / 360.0, 0.45, 0.11, alpha),
        };
    }

    const HUES: [f32; 8] = [
        262.0, // violet
        8.0,   // coral red
        196.0, // sky cyan
        308.0, // magenta orchid
        148.0, // emerald
        32.0,  // amber orange
        172.0, // teal
        228.0, // cobalt blue
    ];
    let hash = name_hash(name);
    let hue = HUES[hash as usize % HUES.len()] / 360.0;
    BeePalette {
        body: gpui::hsla(hue, 0.78, if is_dark { 0.58 } else { 0.46 }, alpha),
        highlight: gpui::hsla(hue, 0.88, if is_dark { 0.76 } else { 0.64 }, alpha),
        shadow: gpui::hsla(hue, 0.72, if is_dark { 0.34 } else { 0.28 }, alpha),
        stripe: gpui::hsla(hue, 0.35, 0.16, alpha),
        outline: gpui::hsla(hue, 0.28, 0.10, alpha),
    }
}

fn name_hash(name: &str) -> u32 {
    name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

fn is_coordinator_member(member: &AntigravitySwarmMember) -> bool {
    member.role == "coordinator" || member.name == "coordinator"
}

fn flat_top_hex_vertices(cx: f32, cy: f32, radius: f32) -> [(f32, f32); 6] {
    let mut pts = [(0.0, 0.0); 6];
    for (idx, pt) in pts.iter_mut().enumerate() {
        let angle = (idx as f32) * (PI / 3.0);
        *pt = (cx + radius * angle.cos(), cy + radius * angle.sin());
    }
    pts
}

/// Computes balanced `(x, y)` bee centers strictly inside a flat-top hexagon,
/// leaving top or bottom space for the chamber header label.
fn pack_bees_in_hex_with_spacing(
    count: usize,
    cx: f32,
    cy: f32,
    hex_r: f32,
    label_at_bottom: bool,
) -> (Vec<(f32, f32)>, f32) {
    if count == 0 {
        return (Vec::new(), 78.0);
    }
    if count == 1 {
        let y_shift = if label_at_bottom {
            -hex_r * 0.10
        } else {
            hex_r * 0.06
        };
        return (vec![(cx, cy + y_shift)], (hex_r * 0.95).clamp(52.0, 84.0));
    }

    let row_counts: Vec<usize> = match count {
        2 => vec![2],
        3 => vec![3],
        4 => vec![2, 2],
        5 => vec![3, 2],
        6 => vec![3, 3],
        7 => vec![2, 3, 2],
        8 => vec![3, 3, 2],
        9 => vec![3, 3, 3],
        n => {
            let mut rows = Vec::new();
            let mut rem = n;
            while rem > 0 {
                let take = if rem > 4 { 4 } else { rem };
                rows.push(take);
                rem -= take;
            }
            rows
        }
    };

    let half_h = hex_r * HEX_HEIGHT_RATIO * 0.5;
    let band_top = if label_at_bottom {
        cy - half_h * 0.62
    } else if row_counts.len() >= 3 {
        cy - half_h * 0.34
    } else {
        cy - half_h * 0.30
    };
    let band_bottom = if label_at_bottom {
        cy + half_h * 0.30
    } else if row_counts.len() >= 3 {
        cy + half_h * 0.74
    } else {
        cy + half_h * 0.70
    };
    let rows_len = row_counts.len();
    let max_cols = row_counts.iter().copied().max().unwrap_or(1);
    let col_step = if max_cols <= 2 {
        hex_r * 0.54
    } else if max_cols == 3 {
        hex_r * 0.46
    } else {
        hex_r * 0.38
    };

    let mut positions = Vec::with_capacity(count);
    let mut min_step = f32::MAX;
    for (row_idx, cols) in row_counts.into_iter().enumerate() {
        let y = if rows_len == 1 {
            (band_top + band_bottom) * 0.5
        } else {
            let t = row_idx as f32 / (rows_len - 1) as f32;
            band_top + t * (band_bottom - band_top)
        };

        // Clamp horizontal spread to stay safely inside the sloped hex sides at `y`.
        let dy_frac = ((y - cy).abs() / half_h.max(1.0)).clamp(0.0, 0.95);
        let hex_half_w_at_y = hex_r * (1.0 - 0.5 * dy_frac) * 0.80;
        let step = if cols <= 1 {
            0.0
        } else {
            let s = col_step.min((hex_half_w_at_y * 2.0) / (cols - 1) as f32);
            min_step = min_step.min(s);
            s
        };
        let actual_span = (cols.saturating_sub(1) as f32) * step;
        let start_x = cx - actual_span * 0.5;
        for col in 0..cols {
            positions.push((start_x + col as f32 * step, y));
        }
    }
    let label_width = if min_step.is_finite() {
        (min_step - 4.0).clamp(44.0, 78.0)
    } else {
        78.0
    };
    (positions, label_width)
}

#[cfg_attr(not(test), allow(dead_code))]
fn pack_bees_in_hex(
    count: usize,
    cx: f32,
    cy: f32,
    hex_r: f32,
    label_at_bottom: bool,
) -> Vec<(f32, f32)> {
    pack_bees_in_hex_with_spacing(count, cx, cy, hex_r, label_at_bottom).0
}

fn compute_worker_done_counts(members: &[AntigravitySwarmMember]) -> (usize, usize) {
    let mut done = 0usize;
    let mut total = 0usize;
    for member in members {
        if is_coordinator_member(member) || member.is_role_worker {
            continue;
        }
        total += 1;
        if member.killed || member.has_posted_done {
            done += 1;
        }
    }
    (done, total)
}

fn resolve_replay_index(
    messages: &[AntigravitySwarmMessage],
    cursor_seq: Option<u64>,
) -> Option<usize> {
    let seq = cursor_seq?;
    if messages.is_empty() {
        return None;
    }
    messages
        .iter()
        .position(|msg| msg.seq == seq)
        .or_else(|| messages.iter().position(|msg| msg.seq > seq))
        .or_else(|| Some(messages.len() - 1))
}

fn scrub_x_to_fraction(x: f32, bounds_left: f32, bounds_width: f32) -> f32 {
    if bounds_width <= 1.0 {
        return 0.0;
    }
    ((x - bounds_left) / bounds_width).clamp(0.0, 1.0)
}

fn scrub_fraction_to_index(fraction: f32, count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    if count == 1 {
        return Some(0);
    }
    Some(((fraction.clamp(0.0, 1.0)) * (count - 1) as f32).round() as usize)
}

fn visible_hive_members(
    snapshot: &AntigravitySwarmSnapshot,
    show_killed: bool,
) -> Vec<&AntigravitySwarmMember> {
    snapshot
        .members
        .iter()
        .filter(|member| show_killed || !member.killed || is_coordinator_member(member))
        .collect()
}

fn message_recipients<'a>(
    message: &'a AntigravitySwarmMessage,
    all_names: &[&'a str],
) -> Vec<&'a str> {
    let mut recipients = Vec::new();
    for mention in &message.mentions {
        if mention != &message.agent && !recipients.contains(&mention.as_str()) {
            recipients.push(mention.as_str());
        }
    }
    if message.mentions_all {
        for &name in all_names {
            if name != message.agent && !recipients.contains(&name) {
                recipients.push(name);
            }
        }
    }
    recipients
}

fn cubic_bezier_point(
    p0: (f32, f32),
    c1: (f32, f32),
    c2: (f32, f32),
    p3: (f32, f32),
    t: f32,
) -> (f32, f32) {
    let u = 1.0 - t;
    let tt = t * t;
    let uu = u * u;
    let uuu = uu * u;
    let ttt = tt * t;
    (
        uuu * p0.0 + 3.0 * uu * t * c1.0 + 3.0 * u * tt * c2.0 + ttt * p3.0,
        uuu * p0.1 + 3.0 * uu * t * c1.1 + 3.0 * u * tt * c2.1 + ttt * p3.1,
    )
}

fn edge_control_points(from: (f32, f32), to: (f32, f32), bow: f32) -> ((f32, f32), (f32, f32)) {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let nx = -dy / len;
    let ny = dx / len;
    let c1 = (from.0 + dx * 0.30 + nx * bow, from.1 + dy * 0.30 + ny * bow);
    let c2 = (from.0 + dx * 0.70 + nx * bow, from.1 + dy * 0.70 + ny * bow);
    (c1, c2)
}

fn parse_board_timestamp_seconds(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.timestamp());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            return Some(naive.and_utc().timestamp());
        }
    }
    for fmt in ["%H:%M:%S", "%H:%M"] {
        if let Ok(time) = NaiveTime::parse_from_str(trimmed, fmt) {
            return Some(time.num_seconds_from_midnight_positive_fallback());
        }
    }
    None
}

trait NaiveTimeSecs {
    fn num_seconds_from_midnight_positive_fallback(&self) -> i64;
}

impl NaiveTimeSecs for NaiveTime {
    fn num_seconds_from_midnight_positive_fallback(&self) -> i64 {
        use chrono::Timelike;
        i64::from(self.hour()) * 3600 + i64::from(self.minute()) * 60 + i64::from(self.second())
    }
}

fn format_delta_suffix(delta_secs: i64) -> String {
    if delta_secs <= 0 {
        return String::new();
    }
    if delta_secs < 60 {
        format!(" (+{delta_secs}s)")
    } else if delta_secs < 3600 {
        let mins = delta_secs / 60;
        let secs = delta_secs % 60;
        if secs == 0 {
            format!(" (+{mins}m)")
        } else {
            format!(" (+{mins}m{secs}s)")
        }
    } else {
        let hours = delta_secs / 3600;
        let mins = (delta_secs % 3600) / 60;
        format!(" (+{hours}h{mins}m)")
    }
}

fn format_replay_telemetry(messages: &[AntigravitySwarmMessage], cursor: usize) -> String {
    let Some(msg) = messages.get(cursor) else {
        return String::new();
    };
    let step = cursor + 1;
    let time_part = if msg.time.trim().is_empty() {
        String::new()
    } else {
        format!(" · {}", short_board_time(&msg.time))
    };
    let delta_part = if cursor > 0 {
        match (
            parse_board_timestamp_seconds(&messages[cursor - 1].time),
            parse_board_timestamp_seconds(&msg.time),
        ) {
            (Some(prev), Some(curr)) if curr >= prev => format_delta_suffix(curr - prev),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    format!("{step} · #{}{}{}", msg.seq, time_part, delta_part)
}

fn replay_step_delay(
    messages: &[AntigravitySwarmMessage],
    cursor: usize,
    pacing: SwarmHivePacing,
) -> Duration {
    const EVEN_DELAY: Duration = Duration::from_millis(1200);
    if pacing == SwarmHivePacing::Even {
        return EVEN_DELAY;
    }
    let Some(curr) = messages.get(cursor) else {
        return EVEN_DELAY;
    };
    let Some(next) = messages.get(cursor + 1) else {
        return EVEN_DELAY;
    };
    match (
        parse_board_timestamp_seconds(&curr.time),
        parse_board_timestamp_seconds(&next.time),
    ) {
        (Some(t0), Some(t1)) if t1 >= t0 => {
            let ms = ((t1 - t0) as u64).saturating_mul(120).clamp(350, 2500);
            Duration::from_millis(ms)
        }
        _ => EVEN_DELAY,
    }
}

fn build_hive_scene_layout(
    snapshot: &AntigravitySwarmSnapshot,
    panel_width: f32,
    mode: SwarmHiveMode,
    show_killed: bool,
    selected_member: Option<&str>,
    replay_cursor_seq: Option<u64>,
) -> HiveSceneLayout {
    let stage_width = (panel_width - 16.0).max(220.0);
    let visible = visible_hive_members(snapshot, show_killed);
    let visible_names: Vec<&str> = visible.iter().map(|m| m.name.as_str()).collect();
    let replay_cursor = resolve_replay_index(&snapshot.messages, replay_cursor_seq);

    let (highlighted_names, replay_active_pair): (Vec<&str>, Option<(Vec<&str>, usize)>) =
        if let Some(idx) = replay_cursor.filter(|&i| i < snapshot.messages.len()) {
            let msg = &snapshot.messages[idx];
            let recips = message_recipients(msg, &visible_names);
            let mut hi = vec![msg.agent.as_str()];
            for r in &recips {
                hi.push(*r);
            }
            (hi, Some((recips, idx)))
        } else {
            (Vec::new(), None)
        };

    let coordinator = visible.iter().copied().find(|m| is_coordinator_member(m));
    let mut chambers = Vec::new();
    let mut bees = Vec::new();
    let mut bee_pos: HashMap<&str, (f32, f32)> = HashMap::new();
    let mut any_working = false;

    match mode {
        SwarmHiveMode::Hive => {
            const GROUP_ORDER: [AntigravitySwarmMemberStatus; 5] = [
                AntigravitySwarmMemberStatus::Failed,
                AntigravitySwarmMemberStatus::Working,
                AntigravitySwarmMemberStatus::Waiting,
                AntigravitySwarmMemberStatus::Stopped,
                AntigravitySwarmMemberStatus::Killed,
            ];
            let mut groups: Vec<(AntigravitySwarmMemberStatus, Vec<&AntigravitySwarmMember>)> =
                Vec::new();
            for status in GROUP_ORDER {
                let members: Vec<&AntigravitySwarmMember> = visible
                    .iter()
                    .copied()
                    .filter(|m| {
                        !is_coordinator_member(m)
                            && !m.is_role_worker
                            && member_display_status(m) == status
                    })
                    .collect();
                if !members.is_empty() {
                    groups.push((status, members));
                }
            }

            let pad_y = 14.0;
            let gap_y = 12.0;

            let (done_count, total_workers) = compute_worker_done_counts(&snapshot.members);
            let has_dense_group = groups.iter().any(|(_, members)| members.len() > 6);
            let use_triad = stage_width >= 460.0
                && groups.len() == 2
                && !has_dense_group
                && coordinator.is_some();

            if use_triad {
                let r = ((stage_width - 32.0) / 4.8).clamp(86.0, 118.0);
                let hex_h = r * HEX_HEIGHT_RATIO;
                let half_h = hex_h * 0.5;
                let left_cx = stage_width * 0.5 - 1.5 * r * 0.72 - 10.0;
                let right_cx = stage_width * 0.5 + 1.5 * r * 0.72 + 10.0;
                let top_cy = pad_y + half_h;
                let coord_cy = top_cy + hex_h * 0.86 + 10.0;

                for (_group_idx, (status, members)) in groups.iter().enumerate() {
                    let cx = if _group_idx == 0 { left_cx } else { right_cx };
                    chambers.push(HiveChamberLayout {
                        title: member_status_label(*status),
                        subtitle: members.len().to_string(),
                        cx,
                        cy: top_cy,
                        radius: r,
                        status_accent: Some(*status),
                        label_at_bottom: false,
                    });
                    let pixel_size = if members.len() > 6 { 1.65 } else { 2.0 };
                    let (pts, label_width) =
                        pack_bees_in_hex_with_spacing(members.len(), cx, top_cy, r, false);
                    for (member, (bx, by)) in members.iter().zip(pts) {
                        let status = member_display_status(member);
                        if status == AntigravitySwarmMemberStatus::Working {
                            any_working = true;
                        }
                        bee_pos.insert(member.name.as_str(), (bx, by));
                        bees.push(HiveBeeNode {
                            name: member.name.clone(),
                            is_coordinator: false,
                            status,
                            killed: member.killed,
                            cx: bx,
                            cy: by,
                            pixel_size,
                            label_width,
                            phase_seed: (name_hash(&member.name) % 100) as f32 / 100.0,
                            highlighted: highlighted_names.contains(&member.name.as_str()),
                            selected: selected_member == Some(member.name.as_str()),
                        });
                    }
                }

                if let Some(coord) = coordinator {
                    let coord_cx = stage_width * 0.5;
                    chambers.push(HiveChamberLayout {
                        title: tr!("swarm.coordinator"),
                        subtitle: tr!(
                            "swarm.done_progress",
                            done = done_count,
                            total = total_workers
                        ),
                        cx: coord_cx,
                        cy: coord_cy,
                        radius: r,
                        status_accent: None,
                        label_at_bottom: true,
                    });
                    let (bx, by) = (coord_cx, coord_cy - r * 0.12);
                    let status = member_display_status(coord);
                    if status == AntigravitySwarmMemberStatus::Working {
                        any_working = true;
                    }
                    bee_pos.insert(coord.name.as_str(), (bx, by));
                    bees.push(HiveBeeNode {
                        name: coord.name.clone(),
                        is_coordinator: true,
                        status,
                        killed: coord.killed,
                        cx: bx,
                        cy: by,
                        pixel_size: 2.35,
                        label_width: 84.0,
                        phase_seed: 0.0,
                        highlighted: highlighted_names.contains(&coord.name.as_str()),
                        selected: selected_member == Some(coord.name.as_str()),
                    });
                }

                let stage_height = coord_cy + half_h + pad_y;
                let edges = build_hive_edges(
                    snapshot,
                    &visible_names,
                    &bee_pos,
                    coordinator.map(|c| c.name.as_str()),
                    visible.as_slice(),
                    selected_member,
                    replay_active_pair.as_ref().map(|(_, i)| *i),
                    false,
                );
                return HiveSceneLayout {
                    stage_width,
                    stage_height,
                    chambers,
                    bees,
                    edges,
                    any_working,
                };
            }

            // Centered honeycomb column; scale chamber radius up when a status holds >6 workers.
            let max_group_len = groups.iter().map(|(_, m)| m.len()).max().unwrap_or(0);
            let r = if max_group_len > 9 {
                (stage_width * 0.44).clamp(112.0, 176.0)
            } else if max_group_len > 6 {
                (stage_width * 0.42).clamp(102.0, 160.0)
            } else if groups.len() <= 1 {
                (stage_width * 0.37).clamp(88.0, 142.0)
            } else {
                (stage_width * 0.34).clamp(82.0, 126.0)
            };
            let hex_h = r * HEX_HEIGHT_RATIO;
            let half_h = hex_h * 0.5;
            let center_x = stage_width * 0.5;
            let mut cursor_cy = pad_y + half_h;

            for (_group_idx, (status, members)) in groups.iter().enumerate() {
                chambers.push(HiveChamberLayout {
                    title: member_status_label(*status),
                    subtitle: members.len().to_string(),
                    cx: center_x,
                    cy: cursor_cy,
                    radius: r,
                    status_accent: Some(*status),
                    label_at_bottom: false,
                });
                let pixel_size = if members.len() > 9 {
                    1.45
                } else if members.len() > 6 {
                    1.65
                } else {
                    2.0
                };
                let (pts, label_width) =
                    pack_bees_in_hex_with_spacing(members.len(), center_x, cursor_cy, r, false);
                for (member, (bx, by)) in members.iter().zip(pts) {
                    let status = member_display_status(member);
                    if status == AntigravitySwarmMemberStatus::Working {
                        any_working = true;
                    }
                    bee_pos.insert(member.name.as_str(), (bx, by));
                    bees.push(HiveBeeNode {
                        name: member.name.clone(),
                        is_coordinator: false,
                        status,
                        killed: member.killed,
                        cx: bx,
                        cy: by,
                        pixel_size,
                        label_width,
                        phase_seed: (name_hash(&member.name) % 100) as f32 / 100.0,
                        highlighted: highlighted_names.contains(&member.name.as_str()),
                        selected: selected_member == Some(member.name.as_str()),
                    });
                }
                cursor_cy += hex_h + gap_y;
            }

            if let Some(coord) = coordinator {
                let coord_r = r.min(138.0);
                chambers.push(HiveChamberLayout {
                    title: tr!("swarm.coordinator"),
                    subtitle: tr!(
                        "swarm.done_progress",
                        done = done_count,
                        total = total_workers
                    ),
                    cx: center_x,
                    cy: cursor_cy,
                    radius: coord_r,
                    status_accent: None,
                    label_at_bottom: true,
                });
                let (bx, by) = (center_x, cursor_cy - coord_r * 0.12);
                let status = member_display_status(coord);
                if status == AntigravitySwarmMemberStatus::Working {
                    any_working = true;
                }
                bee_pos.insert(coord.name.as_str(), (bx, by));
                bees.push(HiveBeeNode {
                    name: coord.name.clone(),
                    is_coordinator: true,
                    status,
                    killed: coord.killed,
                    cx: bx,
                    cy: by,
                    pixel_size: 2.5,
                    label_width: 84.0,
                    phase_seed: 0.0,
                    highlighted: highlighted_names.contains(&coord.name.as_str()),
                    selected: selected_member == Some(coord.name.as_str()),
                });
                cursor_cy += hex_h + gap_y;
            }

            let stage_height = if chambers.is_empty() {
                220.0
            } else {
                (cursor_cy - gap_y - half_h + pad_y).max(220.0)
            };
            let edges = build_hive_edges(
                snapshot,
                &visible_names,
                &bee_pos,
                coordinator.map(|c| c.name.as_str()),
                visible.as_slice(),
                selected_member,
                replay_active_pair.as_ref().map(|(_, i)| *i),
                false,
            );
            HiveSceneLayout {
                stage_width,
                stage_height,
                chambers,
                bees,
                edges,
                any_working,
            }
        }
        SwarmHiveMode::Mentions => {
            let orbit_r = (stage_width * 0.34).clamp(76.0, 140.0);
            let center_hex_r = (stage_width * 0.15).clamp(36.0, 56.0);
            let pad_y = 42.0;
            let cx = stage_width * 0.5;
            let cy = orbit_r + pad_y;
            let stage_height = (orbit_r * 2.0 + pad_y * 2.0 + 18.0).max(260.0);

            if let Some(coord) = coordinator {
                chambers.push(HiveChamberLayout {
                    title: String::new(),
                    subtitle: String::new(),
                    cx,
                    cy,
                    radius: center_hex_r,
                    status_accent: None,
                    label_at_bottom: true,
                });
                let status = member_display_status(coord);
                if status == AntigravitySwarmMemberStatus::Working {
                    any_working = true;
                }
                bee_pos.insert(coord.name.as_str(), (cx, cy));
                bees.push(HiveBeeNode {
                    name: coord.name.clone(),
                    is_coordinator: true,
                    status,
                    killed: coord.killed,
                    cx,
                    cy,
                    pixel_size: 2.1,
                    label_width: (center_hex_r * 1.65).clamp(58.0, 84.0),
                    phase_seed: 0.0,
                    highlighted: highlighted_names.contains(&coord.name.as_str()),
                    selected: selected_member == Some(coord.name.as_str()),
                });
            }

            let orbit_members: Vec<&AntigravitySwarmMember> = visible
                .iter()
                .copied()
                .filter(|m| !is_coordinator_member(m))
                .collect();
            let count = orbit_members.len().max(1);
            let chord = 2.0 * orbit_r * (PI / count as f32).sin();
            let orbit_label_w = (chord - 4.0).clamp(44.0, 78.0);
            let orbit_px = if count > 9 {
                1.5
            } else if count > 6 {
                1.7
            } else {
                1.9
            };
            for (idx, member) in orbit_members.iter().enumerate() {
                let angle = -PI * 0.5 + (idx as f32) * (TAU / count as f32);
                let bx = cx + orbit_r * angle.cos();
                let by = cy + orbit_r * angle.sin();
                let status = member_display_status(member);
                if status == AntigravitySwarmMemberStatus::Working {
                    any_working = true;
                }
                bee_pos.insert(member.name.as_str(), (bx, by));
                bees.push(HiveBeeNode {
                    name: member.name.clone(),
                    is_coordinator: false,
                    status,
                    killed: member.killed,
                    cx: bx,
                    cy: by,
                    pixel_size: orbit_px,
                    label_width: orbit_label_w,
                    phase_seed: (name_hash(&member.name) % 100) as f32 / 100.0,
                    highlighted: highlighted_names.contains(&member.name.as_str()),
                    selected: selected_member == Some(member.name.as_str()),
                });
            }

            let edges = build_hive_edges(
                snapshot,
                &visible_names,
                &bee_pos,
                coordinator.map(|c| c.name.as_str()),
                visible.as_slice(),
                selected_member,
                replay_active_pair.as_ref().map(|(_, i)| *i),
                true,
            );
            HiveSceneLayout {
                stage_width,
                stage_height,
                chambers,
                bees,
                edges,
                any_working,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_hive_edges(
    snapshot: &AntigravitySwarmSnapshot,
    visible_names: &[&str],
    bee_pos: &HashMap<&str, (f32, f32)>,
    coordinator_name: Option<&str>,
    visible_members: &[&AntigravitySwarmMember],
    selected_member: Option<&str>,
    replay_cursor: Option<usize>,
    mentions_mode: bool,
) -> Vec<HiveArcEdge> {
    let mut edges = Vec::new();

    if let Some(cursor) = replay_cursor.filter(|&i| i < snapshot.messages.len()) {
        // Draw recent trailing messages faintly, then the current replayed message brightly.
        let start = cursor.saturating_sub(3);
        for idx in start..=cursor {
            let msg = &snapshot.messages[idx];
            let Some(&from_pt) = bee_pos.get(msg.agent.as_str()) else {
                continue;
            };
            let is_current = idx == cursor;
            let age = cursor - idx;
            let base_opacity = match age {
                0 => 0.88,
                1 => 0.42,
                2 => 0.25,
                _ => 0.14,
            };
            let recips = message_recipients(msg, visible_names);
            for (recip_idx, recip) in recips.iter().enumerate() {
                let Some(&to_pt) = bee_pos.get(*recip) else {
                    continue;
                };
                let touches_selected =
                    selected_member.is_none_or(|sel| sel == msg.agent || sel == *recip);
                let opacity = if touches_selected {
                    base_opacity
                } else {
                    base_opacity * 0.25
                };
                let lane_sign = if (from_pt.0 + to_pt.0) * 0.5
                    < bee_pos.values().map(|p| p.0).sum::<f32>() / bee_pos.len().max(1) as f32
                {
                    -1.0
                } else {
                    1.0
                };
                let bow = lane_sign * (18.0 + (recip_idx as f32 % 3.0) * 8.0);
                edges.push(HiveArcEdge {
                    from: from_pt,
                    to: to_pt,
                    bow,
                    opacity,
                    stroke_width: if is_current { 2.0 } else { 1.3 },
                    dashed: false,
                    active_pulse: is_current,
                    highlighted: is_current,
                    phase_offset: (recip_idx as f32 * 0.17) % 1.0,
                });
            }
        }
        return bounded_hive_edges(edges);
    }

    if mentions_mode {
        // Aggregate directed pair counts across all board messages.
        let mut counts: HashMap<(&str, &str), usize> = HashMap::new();
        for msg in &snapshot.messages {
            for recip in message_recipients(msg, visible_names) {
                if bee_pos.contains_key(msg.agent.as_str()) && bee_pos.contains_key(recip) {
                    *counts.entry((msg.agent.as_str(), recip)).or_insert(0) += 1;
                }
            }
        }
        for (&(from_name, to_name), &count) in &counts {
            let from_pt = bee_pos[from_name];
            let to_pt = bee_pos[to_name];
            let has_reverse = counts.contains_key(&(to_name, from_name));
            let touches_selected = selected_member.map(|sel| sel == from_name || sel == to_name);
            let opacity = match touches_selected {
                Some(true) => 0.85,
                Some(false) => 0.16,
                None => (0.34 + (count as f32).min(5.0) * 0.09).min(0.82),
            };
            let bow = if has_reverse { 26.0 } else { 14.0 };
            edges.push(HiveArcEdge {
                from: from_pt,
                to: to_pt,
                bow,
                opacity,
                stroke_width: (1.25 + (count as f32 - 1.0).min(3.0) * 0.35),
                dashed: false,
                active_pulse: touches_selected.unwrap_or(false),
                highlighted: touches_selected == Some(true),
                phase_offset: (name_hash(from_name).wrapping_add(name_hash(to_name)) % 100) as f32
                    / 100.0,
            });
        }
        return bounded_hive_edges(edges);
    }

    // Live Hive mode: scan newest 16 messages for recent direct links, plus subtle
    // coordinator -> working worker filaments so active work stays connected.
    let mut seen_pairs: HashMap<(&str, &str), usize> = HashMap::new();
    let tail_start = snapshot.messages.len().saturating_sub(16);
    for (rank, msg) in snapshot.messages[tail_start..].iter().rev().enumerate() {
        let Some(&from_pt) = bee_pos.get(msg.agent.as_str()) else {
            continue;
        };
        for recip in message_recipients(msg, visible_names) {
            let Some(&to_pt) = bee_pos.get(recip) else {
                continue;
            };
            if seen_pairs.contains_key(&(msg.agent.as_str(), recip)) {
                continue;
            }
            seen_pairs.insert((msg.agent.as_str(), recip), rank);
            let touches_selected = selected_member.map(|sel| sel == msg.agent || sel == recip);
            let recency = (1.0 - (rank as f32 / 14.0)).clamp(0.18, 1.0);
            let opacity = match touches_selected {
                Some(true) => 0.86,
                Some(false) => 0.14,
                None => 0.28 + 0.44 * recency,
            };
            let bow = ((to_pt.0 - from_pt.0) * 0.18).clamp(-26.0, 26.0)
                + if rank % 2 == 0 { 10.0 } else { -10.0 };
            edges.push(HiveArcEdge {
                from: from_pt,
                to: to_pt,
                bow,
                opacity,
                stroke_width: if rank < 3 { 1.65 } else { 1.25 },
                dashed: false,
                active_pulse: rank < 5,
                highlighted: touches_selected == Some(true),
                phase_offset: ((rank as f32) * 0.21) % 1.0,
            });
        }
    }

    if let Some(coord_name) = coordinator_name
        && let Some(&coord_pt) = bee_pos.get(coord_name)
    {
        for (idx, member) in visible_members.iter().enumerate() {
            if member.name == coord_name
                || member_display_status(member) != AntigravitySwarmMemberStatus::Working
            {
                continue;
            }
            let has_edge = seen_pairs
                .keys()
                .any(|(a, b)| *a == member.name || *b == member.name);
            if has_edge {
                continue;
            }
            if let Some(&worker_pt) = bee_pos.get(member.name.as_str()) {
                let touches_selected =
                    selected_member.map(|sel| sel == coord_name || sel == member.name);
                let opacity = match touches_selected {
                    Some(true) => 0.72,
                    Some(false) => 0.14,
                    None => 0.34,
                };
                let bow = ((worker_pt.0 - coord_pt.0) * 0.22).clamp(-24.0, 24.0)
                    + if idx % 2 == 0 { 8.0 } else { -8.0 };
                edges.push(HiveArcEdge {
                    from: coord_pt,
                    to: worker_pt,
                    bow,
                    opacity,
                    stroke_width: 1.3,
                    dashed: true,
                    active_pulse: true,
                    highlighted: touches_selected == Some(true),
                    phase_offset: (idx as f32 * 0.19) % 1.0,
                });
            }
        }
    }

    bounded_hive_edges(edges)
}

// Arcs are a visual sample, not a count of messages or member states. Keep
// every member and all aggregation inputs, but bound expensive path painting.
const MAX_HIVE_PAINT_EDGES: usize = 64;

fn bounded_hive_edges(mut edges: Vec<HiveArcEdge>) -> Vec<HiveArcEdge> {
    edges.sort_by(|a, b| {
        b.highlighted
            .cmp(&a.highlighted)
            .then_with(|| b.opacity.total_cmp(&a.opacity))
            .then_with(|| a.from.0.total_cmp(&b.from.0))
            .then_with(|| a.from.1.total_cmp(&b.from.1))
            .then_with(|| a.to.0.total_cmp(&b.to.0))
            .then_with(|| a.to.1.total_cmp(&b.to.1))
    });
    edges.truncate(MAX_HIVE_PAINT_EDGES);
    edges
}

fn paint_hex_chamber(
    window: &mut Window,
    origin: Point<Pixels>,
    chamber: &HiveChamberLayout,
    theme: &Theme,
) {
    let vertices = flat_top_hex_vertices(
        f32::from(origin.x) + chamber.cx,
        f32::from(origin.y) + chamber.cy,
        chamber.radius,
    );
    let fill_color = if theme.is_dark {
        gpui::hsla(240.0 / 360.0, 0.10, 0.085, 0.96)
    } else {
        theme.composer.opacity(0.96)
    };
    let border_color = match chamber.status_accent {
        Some(AntigravitySwarmMemberStatus::Failed) => theme.danger.opacity(0.60),
        Some(AntigravitySwarmMemberStatus::Working) => theme.success.opacity(0.55),
        Some(AntigravitySwarmMemberStatus::Stopped) => theme.text_tertiary.opacity(0.55),
        _ => theme.border_strong,
    };

    let mut fill_path = PathBuilder::fill();
    fill_path.move_to(point(px(vertices[0].0), px(vertices[0].1)));
    for &(vx, vy) in &vertices[1..] {
        fill_path.line_to(point(px(vx), px(vy)));
    }
    fill_path.close();
    if let Ok(built) = fill_path.build() {
        window.paint_path(built, fill_color);
    }

    let mut stroke_path = PathBuilder::stroke(px(1.25));
    stroke_path.move_to(point(px(vertices[0].0), px(vertices[0].1)));
    for &(vx, vy) in &vertices[1..] {
        stroke_path.line_to(point(px(vx), px(vy)));
    }
    stroke_path.line_to(point(px(vertices[0].0), px(vertices[0].1)));
    if let Ok(built) = stroke_path.build() {
        window.paint_path(built, border_color);
    }
}

fn cubic_bezier_tangent(
    p0: (f32, f32),
    c1: (f32, f32),
    c2: (f32, f32),
    p3: (f32, f32),
    t: f32,
) -> (f32, f32) {
    let u = 1.0 - t;
    let dx =
        3.0 * u * u * (c1.0 - p0.0) + 6.0 * u * t * (c2.0 - c1.0) + 3.0 * t * t * (p3.0 - c2.0);
    let dy =
        3.0 * u * u * (c1.1 - p0.1) + 6.0 * u * t * (c2.1 - c1.1) + 3.0 * t * t * (p3.1 - c2.1);
    (dx, dy)
}

fn paint_arc_edge(
    window: &mut Window,
    origin: Point<Pixels>,
    edge: &HiveArcEdge,
    phase: f32,
    animate: bool,
    theme: &Theme,
) {
    let ox = f32::from(origin.x);
    let oy = f32::from(origin.y);
    let p0 = (ox + edge.from.0, oy + edge.from.1);
    let p3 = (ox + edge.to.0, oy + edge.to.1);
    let (c1, c2) = edge_control_points(p0, p3, edge.bow);

    let line_color = if edge.highlighted {
        theme.accent.opacity(edge.opacity)
    } else if theme.is_dark {
        gpui::hsla(214.0 / 360.0, 0.78, 0.62, edge.opacity)
    } else {
        gpui::hsla(214.0 / 360.0, 0.72, 0.44, edge.opacity)
    };

    let mut builder = PathBuilder::stroke(px(edge.stroke_width));
    if edge.dashed {
        builder = builder.dash_array(&[px(3.0), px(4.0)]);
    }
    builder.move_to(point(px(p0.0), px(p0.1)));
    builder.cubic_bezier_to(
        point(px(p3.0), px(p3.1)),
        point(px(c1.0), px(c1.1)),
        point(px(c2.0), px(c2.1)),
    );
    if let Ok(path) = builder.build() {
        window.paint_path(path, line_color);
    }

    let chord = ((p3.0 - p0.0).powi(2) + (p3.1 - p0.1).powi(2)).sqrt();
    if chord >= 22.0 && edge.opacity >= 0.05 {
        let tip_t = 0.84;
        let tip = cubic_bezier_point(p0, c1, c2, p3, tip_t);
        let tangent = cubic_bezier_tangent(p0, c1, c2, p3, tip_t);
        let t_len = (tangent.0 * tangent.0 + tangent.1 * tangent.1).sqrt();
        if t_len > 0.5 {
            let ux = tangent.0 / t_len;
            let uy = tangent.1 / t_len;
            let nx = -uy;
            let ny = ux;
            let head_len = 5.2 + edge.stroke_width * 0.75;
            let half_w = 2.6 + edge.stroke_width * 0.4;
            let base_x = tip.0 - ux * head_len;
            let base_y = tip.1 - uy * head_len;
            let mut arrow = PathBuilder::fill();
            arrow.move_to(point(px(tip.0), px(tip.1)));
            arrow.line_to(point(px(base_x + nx * half_w), px(base_y + ny * half_w)));
            arrow.line_to(point(px(base_x - nx * half_w), px(base_y - ny * half_w)));
            arrow.close();
            if let Ok(built) = arrow.build() {
                window.paint_path(built, line_color);
            }
        }
    }

    if edge.active_pulse {
        let t = if animate {
            (phase + edge.phase_offset).fract()
        } else {
            0.66
        };
        let (dx, dy) = cubic_bezier_point(p0, c1, c2, p3, t);
        let dot_color = if edge.highlighted {
            theme.accent
        } else if theme.is_dark {
            gpui::hsla(212.0 / 360.0, 0.90, 0.72, 1.0)
        } else {
            gpui::hsla(212.0 / 360.0, 0.85, 0.48, 1.0)
        };
        paint_filled_circle(
            window,
            point(px(dx), px(dy)),
            px(4.2),
            dot_color.opacity(0.26),
        );
        paint_filled_circle(
            window,
            point(px(dx), px(dy)),
            px(2.0),
            dot_color.opacity(0.95),
        );
    }
}

fn paint_filled_circle(window: &mut Window, center: Point<Pixels>, radius: Pixels, color: Hsla) {
    let mut path = PathBuilder::fill();
    stroke_circle_arc(&mut path, center, radius);
    if let Ok(built) = path.build() {
        window.paint_path(built, color);
    }
}

fn paint_stroked_circle(
    window: &mut Window,
    center: Point<Pixels>,
    radius: Pixels,
    stroke: Pixels,
    color: Hsla,
) {
    let mut path = PathBuilder::stroke(stroke);
    stroke_circle_arc(&mut path, center, radius);
    if let Ok(built) = path.build() {
        window.paint_path(built, color);
    }
}

fn stroke_circle_arc(path: &mut PathBuilder, center: Point<Pixels>, radius: Pixels) {
    path.move_to(point(center.x + radius, center.y));
    path.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x - radius, center.y),
    );
    path.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x + radius, center.y),
    );
    path.close();
}

fn bee_sprite_pixel_color(
    ch: u8,
    palette: &BeePalette,
    eye_color: Hsla,
    glint_color: Hsla,
    wing_bright: Hsla,
    wing_edge: Hsla,
    crown_gold: Hsla,
    crown_tip: Hsla,
) -> Option<Hsla> {
    match ch {
        b'.' => None,
        b'o' | b't' => Some(palette.outline),
        b'b' => Some(palette.body),
        b'h' => Some(palette.highlight),
        b's' => Some(palette.shadow),
        b'k' => Some(palette.stripe),
        b'e' => Some(eye_color),
        b'g' => Some(glint_color),
        b'w' => Some(wing_bright),
        b'W' => Some(wing_edge),
        b'c' => Some(crown_gold),
        b'C' => Some(crown_tip),
        _ => None,
    }
}

fn paint_pixel_bee(
    window: &mut Window,
    origin: Point<Pixels>,
    bee: &HiveBeeNode,
    phase: f32,
    animate: bool,
    theme: &Theme,
) {
    let is_working = bee.status == AntigravitySwarmMemberStatus::Working && !bee.killed;
    let is_stopped = bee.status == AntigravitySwarmMemberStatus::Stopped && !bee.killed;
    let bob_y = if is_working && animate {
        ((phase * TAU * 2.0) + bee.phase_seed * TAU).sin() * 1.6
    } else {
        0.0
    };
    let wings_up = if is_working && animate {
        ((phase * 10.0 + bee.phase_seed * 4.0) as u32).is_multiple_of(2)
    } else {
        true
    };

    let center_x = f32::from(origin.x) + bee.cx;
    let center_y = f32::from(origin.y) + bee.cy + bob_y;

    if bee.selected || bee.highlighted {
        paint_stroked_circle(
            window,
            point(px(center_x), px(center_y)),
            px(bee.pixel_size * 9.5),
            px(1.5),
            theme.accent.opacity(if bee.selected { 0.92 } else { 0.65 }),
        );
    } else if bee.status == AntigravitySwarmMemberStatus::Failed && !bee.killed {
        paint_stroked_circle(
            window,
            point(px(center_x), px(center_y)),
            px(bee.pixel_size * 9.5),
            px(1.5),
            theme.danger.opacity(0.85),
        );
    } else if is_stopped {
        paint_stroked_circle(
            window,
            point(px(center_x), px(center_y)),
            px(bee.pixel_size * 9.2),
            px(1.1),
            theme.text_tertiary.opacity(0.55),
        );
    } else if is_working && !animate {
        paint_stroked_circle(
            window,
            point(px(center_x), px(center_y)),
            px(bee.pixel_size * 9.2),
            px(1.2),
            theme.success.opacity(0.65),
        );
    }

    let alpha = if bee.killed {
        0.42
    } else if is_stopped {
        0.78
    } else {
        1.0
    };
    let palette = bee_palette(&bee.name, bee.is_coordinator, theme.is_dark, alpha);
    let eye_color = gpui::hsla(0.0, 0.0, 0.08, alpha);
    let glint_color = gpui::hsla(0.0, 0.0, 0.98, alpha);
    let (wing_bright, wing_edge, crown_gold, crown_tip) = if theme.is_dark {
        (
            gpui::hsla(205.0 / 360.0, 0.55, 0.88, 0.88 * alpha),
            gpui::hsla(212.0 / 360.0, 0.48, 0.70, 0.68 * alpha),
            gpui::hsla(45.0 / 360.0, 0.96, 0.56, alpha),
            gpui::hsla(52.0 / 360.0, 1.0, 0.78, alpha),
        )
    } else {
        (
            gpui::hsla(210.0 / 360.0, 0.58, 0.72, 0.92 * alpha),
            gpui::hsla(214.0 / 360.0, 0.50, 0.48, 0.82 * alpha),
            gpui::hsla(40.0 / 360.0, 0.92, 0.45, alpha),
            gpui::hsla(45.0 / 360.0, 0.96, 0.60, alpha),
        )
    };

    let (cols, rows, sprite): (usize, usize, &[&str]) = if bee.is_coordinator {
        (
            QUEEN_SPRITE_COLS,
            QUEEN_SPRITE_ROWS,
            if wings_up {
                &QUEEN_BEE_UP
            } else {
                &QUEEN_BEE_DOWN
            },
        )
    } else {
        (
            WORKER_SPRITE_COLS,
            WORKER_SPRITE_ROWS,
            if wings_up {
                &WORKER_BEE_UP
            } else {
                &WORKER_BEE_DOWN
            },
        )
    };

    let cell = bee.pixel_size;
    let total_w = cols as f32 * cell;
    let total_h = rows as f32 * cell;
    let x0 = (center_x - total_w * 0.5).round();
    let y0 = (center_y - total_h * 0.5).round();

    // Merge consecutive identical sprite pixels per row (horizontal RLE) to reduce draw quads.
    for (row_idx, line) in sprite.iter().enumerate() {
        let py0 = (y0 + row_idx as f32 * cell).round();
        let py1 = (y0 + (row_idx + 1) as f32 * cell).round();
        let cell_h = (py1 - py0).max(1.0);
        let bytes = line.as_bytes();
        let mut col_idx = 0usize;
        while col_idx < bytes.len() {
            let Some(color) = bee_sprite_pixel_color(
                bytes[col_idx],
                &palette,
                eye_color,
                glint_color,
                wing_bright,
                wing_edge,
                crown_gold,
                crown_tip,
            ) else {
                col_idx += 1;
                continue;
            };
            let run_start = col_idx;
            let key_ch = bytes[col_idx];
            col_idx += 1;
            while col_idx < bytes.len() && bytes[col_idx] == key_ch {
                col_idx += 1;
            }
            let px0 = (x0 + run_start as f32 * cell).round();
            let px1 = (x0 + col_idx as f32 * cell).round();
            let run_w = (px1 - px0).max(1.0);
            window.paint_quad(gpui::fill(
                Bounds::new(point(px(px0), px(py0)), gpui::size(px(run_w), px(cell_h))),
                color,
            ));
        }
    }

    // Accessible non-color status badges at top-right of bee sprite.
    if bee.status == AntigravitySwarmMemberStatus::Failed && !bee.killed {
        let bx = x0 + total_w - 1.5;
        let by = y0 + 2.5;
        paint_filled_circle(window, point(px(bx), px(by)), px(4.5), theme.danger);
        let fg = gpui::white();
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(px(bx - 0.75), px(by - 2.6)),
                gpui::size(px(1.5), px(3.0)),
            ),
            fg,
        ));
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(px(bx - 0.75), px(by + 1.2)),
                gpui::size(px(1.5), px(1.5)),
            ),
            fg,
        ));
    } else if is_stopped {
        let bx = x0 + total_w - 2.5;
        let by = y0 + 2.5;
        paint_filled_circle(window, point(px(bx), px(by)), px(4.2), theme.text_tertiary);
        let fg = if theme.is_dark {
            theme.inset
        } else {
            gpui::white()
        };
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(px(bx - 1.8), px(by - 1.8)),
                gpui::size(px(1.2), px(3.6)),
            ),
            fg,
        ));
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(px(bx + 0.6), px(by - 1.8)),
                gpui::size(px(1.2), px(3.6)),
            ),
            fg,
        ));
    } else if is_working && !animate {
        paint_filled_circle(
            window,
            point(px(x0 + total_w - 2.0), px(y0 + 3.0)),
            px(2.8),
            theme.success,
        );
    }
}

fn bee_tooltip_text(member: &AntigravitySwarmMember) -> String {
    let status = member_status_label(member_display_status(member));
    let mut parts = vec![
        format!("{} · {}", member.name, status),
        format!(
            "{} · {}",
            tr!("swarm.steps_count", count = member.steps),
            tr!("swarm.posts_count", count = member.posts)
        ),
    ];
    if let Some(activity) = member.activity.as_ref().filter(|s| !s.is_empty()) {
        parts.push(localized_member_activity(activity));
    }
    if let Some(failure) = member.failure.as_ref() {
        parts.push(failure.message.clone());
    }
    parts.join("\n")
}

fn replay_caption_text(msg: &AntigravitySwarmMessage) -> String {
    let mut recipients = msg
        .mentions
        .iter()
        .map(|name| format!("@{name}"))
        .collect::<Vec<_>>();
    if msg.mentions_all {
        recipients.push("@all".to_owned());
    }
    let first_line = msg
        .text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let prefix = if recipients.is_empty() {
        format!("#{} {}:", msg.seq, msg.agent)
    } else {
        format!("#{} {} → {}:", msg.seq, msg.agent, recipients.join(" "))
    };
    if first_line.is_empty() {
        prefix
    } else {
        format!("{prefix} {first_line}")
    }
}

fn render_play_pause_glyph(playing: bool, color: Hsla) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let cx = f32::from(bounds.origin.x + bounds.size.width * 0.5);
            let cy = f32::from(bounds.origin.y + bounds.size.height * 0.5);
            if playing {
                window.paint_quad(gpui::fill(
                    Bounds::new(
                        point(px(cx - 3.5), px(cy - 4.0)),
                        gpui::size(px(2.5), px(8.0)),
                    ),
                    color,
                ));
                window.paint_quad(gpui::fill(
                    Bounds::new(
                        point(px(cx + 1.0), px(cy - 4.0)),
                        gpui::size(px(2.5), px(8.0)),
                    ),
                    color,
                ));
            } else {
                let mut tri = PathBuilder::fill();
                tri.move_to(point(px(cx - 3.0), px(cy - 4.5)));
                tri.line_to(point(px(cx + 4.5), px(cy)));
                tri.line_to(point(px(cx - 3.0), px(cy + 4.5)));
                tri.close();
                if let Ok(path) = tri.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .size(px(12.0))
    .into_any_element()
}

impl Waku {
    pub(super) fn start_swarm_hive_replay(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        let Some(snapshot) = self
            .swarm_panel
            .snapshots
            .get(&session_id)
            .and_then(|entry| entry.as_ref())
        else {
            return;
        };
        if snapshot.messages.is_empty() {
            return;
        }
        let current_idx =
            resolve_replay_index(&snapshot.messages, self.swarm_panel.hive.replay_cursor_seq);
        if current_idx.is_none() || current_idx == Some(snapshot.messages.len() - 1) {
            self.swarm_panel.hive.replay_cursor_seq = Some(snapshot.messages[0].seq);
        }
        self.swarm_panel.hive.replay_playing = true;
        self.swarm_panel.hive.replay_generation =
            self.swarm_panel.hive.replay_generation.wrapping_add(1);
        let generation = self.swarm_panel.hive.replay_generation;
        self.schedule_swarm_hive_replay_step(session_id, generation, cx);
        cx.notify();
    }

    fn schedule_swarm_hive_replay_step(
        &self,
        session_id: Uuid,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self
            .swarm_panel
            .snapshots
            .get(&session_id)
            .and_then(|entry| entry.as_ref())
        else {
            return;
        };
        let Some(cursor) =
            resolve_replay_index(&snapshot.messages, self.swarm_panel.hive.replay_cursor_seq)
        else {
            return;
        };
        let delay = replay_step_delay(&snapshot.messages, cursor, self.swarm_panel.hive.pacing);
        cx.spawn(async move |waku, cx| {
            cx.background_executor().timer(delay).await;
            let _ = waku.update(cx, |this, cx| {
                if this.swarm_panel.hive.replay_generation != generation
                    || !this.swarm_panel.hive.replay_playing
                    || this.state.selected_session != Some(session_id)
                    || !this.right_panel_visible
                    || this.active_right_panel_surface() != Some(&RightPanelSurface::Swarm)
                    || this.swarm_panel.active_tab != SwarmPanelTab::Hive
                {
                    if this.swarm_panel.hive.replay_generation == generation {
                        this.swarm_panel.hive.replay_playing = false;
                    }
                    return;
                }
                let Some(snapshot) = this
                    .swarm_panel
                    .snapshots
                    .get(&session_id)
                    .and_then(|entry| entry.as_ref())
                else {
                    this.swarm_panel.hive.stop_replay();
                    return;
                };
                let next_idx = resolve_replay_index(
                    &snapshot.messages,
                    this.swarm_panel.hive.replay_cursor_seq,
                )
                .map_or(0, |c| c + 1);
                if let Some(next_msg) = snapshot.messages.get(next_idx) {
                    this.swarm_panel.hive.replay_cursor_seq = Some(next_msg.seq);
                    this.schedule_swarm_hive_replay_step(session_id, generation, cx);
                } else {
                    this.swarm_panel.hive.stop_replay();
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_swarm_hive_tab(
        &mut self,
        session_id: Uuid,
        snapshot: &Arc<AntigravitySwarmSnapshot>,
        panel_width: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let msg_count = snapshot.messages.len();
        let replay_idx =
            resolve_replay_index(&snapshot.messages, self.swarm_panel.hive.replay_cursor_seq);
        if replay_idx.is_none() && self.swarm_panel.hive.replay_cursor_seq.is_some() {
            self.swarm_panel.hive.replay_cursor_seq = None;
            self.swarm_panel.hive.stop_replay();
        } else if let Some(idx) = replay_idx {
            self.swarm_panel.hive.replay_cursor_seq = Some(snapshot.messages[idx].seq);
        }
        if let Some(selected) = self.swarm_panel.hive.selected_member.as_deref()
            && !snapshot.members.iter().any(|m| m.name == selected)
        {
            self.swarm_panel.hive.selected_member = None;
        }

        let mode = self.swarm_panel.hive.mode;
        let pacing = self.swarm_panel.hive.pacing;
        let replay_cursor_seq = self.swarm_panel.hive.replay_cursor_seq;
        let replay_playing = self.swarm_panel.hive.replay_playing;
        let show_killed = self.swarm_panel.show_killed_members;

        let telemetry = if let Some(cursor) = replay_idx {
            format_replay_telemetry(&snapshot.messages, cursor)
        } else if snapshot.head_seq > 0 {
            format!("{} · #{}", tr!("swarm.live"), snapshot.head_seq)
        } else {
            tr!("swarm.live")
        };

        let hive_focus = self.swarm_panel.hive.mode_hive_focus.clone();
        let mentions_focus = self.swarm_panel.hive.mode_mentions_focus.clone();
        let pacing_focus = self.swarm_panel.hive.pacing_focus.clone();

        let top_bar = div()
            .flex_none()
            .px(px(12.0))
            .py(px(7.0))
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().size(px(6.0)).flex_none().rounded_full().bg(
                        if replay_idx.is_some() {
                            theme.accent
                        } else {
                            theme.success
                        },
                    ))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(sp(11.5))
                            .text_color(theme.text_secondary)
                            .child(telemetry),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .p(px(2.0))
                            .rounded(px(7.0))
                            .bg(theme.inset)
                            .border_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .id("swarm-hive-mode-hive")
                                    .track_focus(&hive_focus)
                                    .tab_index(0)
                                    .tab_stop(true)
                                    .h(px(20.0))
                                    .px(px(8.0))
                                    .rounded(px(5.0))
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                                    .flex()
                                    .items_center()
                                    .cursor_default()
                                    .focus_visible(|style| style.border_color(theme.accent))
                                    .when(mode == SwarmHiveMode::Hive, |el| {
                                        el.bg(theme.overlay_strong)
                                    })
                                    .when(mode != SwarmHiveMode::Hive, |el| {
                                        el.hover(|style| style.bg(theme.overlay))
                                    })
                                    .text_size(sp(11.5))
                                    .font_weight(if mode == SwarmHiveMode::Hive {
                                        FontWeight::MEDIUM
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(if mode == SwarmHiveMode::Hive {
                                        theme.text
                                    } else {
                                        theme.text_tertiary
                                    })
                                    .child(tr!("swarm.view_hive"))
                                    .on_activation(cx, |this, _, cx| {
                                        this.swarm_panel.hive.mode = SwarmHiveMode::Hive;
                                        cx.notify();
                                    }),
                            )
                            .child(
                                div()
                                    .id("swarm-hive-mode-mentions")
                                    .track_focus(&mentions_focus)
                                    .tab_index(0)
                                    .tab_stop(true)
                                    .h(px(20.0))
                                    .px(px(8.0))
                                    .rounded(px(5.0))
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                                    .flex()
                                    .items_center()
                                    .cursor_default()
                                    .focus_visible(|style| style.border_color(theme.accent))
                                    .when(mode == SwarmHiveMode::Mentions, |el| {
                                        el.bg(theme.overlay_strong)
                                    })
                                    .when(mode != SwarmHiveMode::Mentions, |el| {
                                        el.hover(|style| style.bg(theme.overlay))
                                    })
                                    .text_size(sp(11.5))
                                    .font_weight(if mode == SwarmHiveMode::Mentions {
                                        FontWeight::MEDIUM
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(if mode == SwarmHiveMode::Mentions {
                                        theme.text
                                    } else {
                                        theme.text_tertiary
                                    })
                                    .child(tr!("swarm.view_mentions"))
                                    .on_activation(cx, |this, _, cx| {
                                        this.swarm_panel.hive.mode = SwarmHiveMode::Mentions;
                                        cx.notify();
                                    }),
                            ),
                    )
                    .when(msg_count > 0, |row| {
                        row.child(
                            div()
                                .id("swarm-hive-pacing")
                                .track_focus(&pacing_focus)
                                .tab_index(0)
                                .tab_stop(true)
                                .h(px(24.0))
                                .px(px(8.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(theme.border_strong)
                                .bg(theme.surface)
                                .flex()
                                .items_center()
                                .cursor_default()
                                .focus_visible(|style| style.border_color(theme.accent))
                                .hover(|style| style.bg(theme.overlay))
                                .text_size(sp(11.0))
                                .text_color(theme.text_secondary)
                                .child(pacing.label())
                                .on_activation(cx, |this, _, cx| {
                                    this.swarm_panel.hive.pacing =
                                        this.swarm_panel.hive.pacing.toggle();
                                    cx.notify();
                                }),
                        )
                    }),
            );

        let cache_key = HiveSceneCacheKey {
            session_id,
            snapshot_fp: snapshot.fingerprint.clone(),
            width_bucket: (panel_width.round() as i32),
            mode,
            show_killed,
            selected_member: self.swarm_panel.hive.selected_member.clone(),
            replay_cursor_seq,
        };
        let hive = &mut self.swarm_panel.hive;
        if hive.requested_scene.as_ref() != Some(&cache_key) {
            hive.requested_scene = Some(cache_key.clone());
            hive.scene_generation = hive.scene_generation.wrapping_add(1);
        }
        let is_cached = hive
            .cached_scene
            .as_ref()
            .is_some_and(|(key, _)| key == &cache_key);
        if !is_cached && !hive.scene_in_flight {
            // Single-flight: rapid resizing/replay coalesces to the latest key,
            // rather than queuing one expensive layout for every frame.
            hive.scene_in_flight = true;
            let generation = hive.scene_generation;
            let snapshot = Arc::clone(snapshot);
            let selected_member = hive.selected_member.clone();
            cx.spawn(async move |waku, cx| {
                let scene = cx
                    .background_executor()
                    .spawn(async move {
                        Arc::new(build_hive_scene_layout(
                            &snapshot,
                            panel_width,
                            mode,
                            show_killed,
                            selected_member.as_deref(),
                            replay_cursor_seq,
                        ))
                    })
                    .await;
                let _ = waku.update(cx, |this, cx| {
                    let hive = &mut this.swarm_panel.hive;
                    hive.scene_in_flight = false;
                    if hive.scene_generation == generation
                        && hive.requested_scene.as_ref() == Some(&cache_key)
                    {
                        hive.cached_scene = Some((cache_key, scene));
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        // A same-session prior scene stays visible while the replacement is
        // built. Session/account changes clear it and invalidate the generation.
        let scene = hive
            .cached_scene
            .as_ref()
            .filter(|(key, _)| key.session_id == session_id)
            .map(|(_, scene)| Arc::clone(scene))
            .unwrap_or_else(|| Arc::new(HiveSceneLayout::default()));

        let stage_body: AnyElement = if scene.bees.is_empty() {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(20.0))
                .text_size(sp(12.5))
                .text_color(theme.text_tertiary)
                .child(if self.swarm_panel.hive.scene_in_flight {
                    "…".to_owned()
                } else {
                    tr!("swarm.hive_empty")
                })
                .into_any_element()
        } else {
            let reduce_motion = cx.reduce_motion();
            let has_pulse_edge = scene.edges.iter().any(|edge| edge.active_pulse);
            let needs_animation =
                !reduce_motion && (replay_playing || (replay_idx.is_none() && scene.any_working));
            let animate_motion =
                needs_animation && (scene.any_working || has_pulse_edge || replay_playing);

            let scene_for_paint = Arc::clone(&scene);
            let theme_copy = *theme;
            let stage_w = scene.stage_width;
            let stage_h = scene.stage_height;

            let paint_scene = move |phase: f32| {
                let scene = Arc::clone(&scene_for_paint);
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let origin = bounds.origin;
                        for chamber in &scene.chambers {
                            paint_hex_chamber(window, origin, chamber, &theme_copy);
                        }
                        for edge in &scene.edges {
                            paint_arc_edge(
                                window,
                                origin,
                                edge,
                                phase,
                                animate_motion,
                                &theme_copy,
                            );
                        }
                        for bee in &scene.bees {
                            paint_pixel_bee(
                                window,
                                origin,
                                bee,
                                phase,
                                animate_motion,
                                &theme_copy,
                            );
                        }
                    },
                )
                .size_full()
                .into_any_element()
            };

            let backdrop: AnyElement = if needs_animation {
                motion::pulse(Duration::from_millis(2400), paint_scene)
                    .every(2)
                    .into_any_element()
            } else {
                paint_scene(0.0)
            };

            let mut stage_layers = div()
                .relative()
                .mx_auto()
                .w(px(stage_w))
                .h(px(stage_h))
                .child(div().absolute().inset_0().child(backdrop));

            // Chamber labels rendered via native typography so CJK & scaling stay razor-sharp.
            for chamber in &scene.chambers {
                if chamber.title.is_empty() && chamber.subtitle.is_empty() {
                    continue;
                }
                let half_h = chamber.radius * HEX_HEIGHT_RATIO * 0.5;
                let label_w = chamber.radius * 1.15;
                let label_left = chamber.cx - label_w * 0.5;
                let label_top = if chamber.label_at_bottom {
                    chamber.cy + half_h - 46.0
                } else {
                    chamber.cy - half_h + 12.0
                };
                let title_color = match chamber.status_accent {
                    Some(AntigravitySwarmMemberStatus::Failed) => theme.danger,
                    _ => theme.text,
                };
                stage_layers = stage_layers.child(
                    div()
                        .absolute()
                        .left(px(label_left))
                        .top(px(label_top))
                        .w(px(label_w))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(1.0))
                        .when(!chamber.title.is_empty(), |col| {
                            col.child(
                                div()
                                    .max_w_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .gap(px(4.0))
                                    .when_some(chamber.status_accent, |row, status| {
                                        row.child(icon(
                                            member_status_icon(status),
                                            10.5,
                                            member_status_color(status, theme),
                                        ))
                                    })
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_center()
                                            .text_size(sp(12.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(title_color)
                                            .child(chamber.title.clone()),
                                    ),
                            )
                        })
                        .when(!chamber.subtitle.is_empty(), |col| {
                            col.child(
                                div()
                                    .max_w_full()
                                    .truncate()
                                    .text_center()
                                    .text_size(sp(11.0))
                                    .text_color(theme.text_tertiary)
                                    .child(chamber.subtitle.clone()),
                            )
                        }),
                );
            }

            // Interactive bee hit targets & labels.
            let member_lookup: HashMap<&str, &AntigravitySwarmMember> = snapshot
                .members
                .iter()
                .map(|m| (m.name.as_str(), m))
                .collect();

            for bee in &scene.bees {
                let Some(member) = member_lookup.get(bee.name.as_str()).copied() else {
                    continue;
                };
                let focus = self.swarm_panel.hive.bee_focus(&bee.name, cx);
                let hit_w = (bee.pixel_size * 22.0).max(38.0).min(bee.label_width);
                let hit_h = (bee.pixel_size * 18.0).max(32.0);
                let label_w = bee.label_width;
                let tooltip = bee_tooltip_text(member);
                let toggle_name_act = bee.name.clone();
                let show_name_label = mode == SwarmHiveMode::Mentions || !bee.is_coordinator;

                stage_layers = stage_layers.child(
                    div()
                        .id(SharedString::from(format!("swarm-hive-bee-{}", bee.name)))
                        .track_focus(&focus)
                        .tab_index(0)
                        .tab_stop(true)
                        .absolute()
                        .left(px(bee.cx - label_w * 0.5))
                        .top(px(bee.cy - hit_h * 0.5))
                        .w(px(label_w))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(2.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(gpui::transparent_black())
                        .cursor_default()
                        .focus_visible(|style| style.border_color(theme.accent))
                        .hover(|style| style.bg(theme.overlay.opacity(0.6)))
                        .tooltip(Tooltip::text(tooltip))
                        .on_activation(cx, move |this, _, cx| {
                            if this.swarm_panel.hive.selected_member.as_deref()
                                == Some(toggle_name_act.as_str())
                            {
                                this.swarm_panel.hive.selected_member = None;
                            } else {
                                this.swarm_panel.hive.selected_member =
                                    Some(toggle_name_act.clone());
                            }
                            cx.notify();
                        })
                        .child(div().w(px(hit_w)).h(px(hit_h)))
                        .when(show_name_label, |col| {
                            col.child(
                                div()
                                    .max_w(px(label_w))
                                    .px(px(3.0))
                                    .truncate()
                                    .text_center()
                                    .text_size(sp(10.5))
                                    .font_weight(if bee.selected || bee.highlighted {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(if bee.selected || bee.highlighted {
                                        theme.text
                                    } else if bee.killed {
                                        theme.text_tertiary
                                    } else {
                                        theme.text_secondary
                                    })
                                    .child(bee.name.clone()),
                            )
                        }),
                );
            }

            if mode == SwarmHiveMode::Mentions && scene.edges.is_empty() {
                stage_layers = stage_layers.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(6.0))
                        .text_center()
                        .text_size(sp(11.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("swarm.hive_mentions_empty")),
                );
            }

            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .relative()
                .child(
                    div()
                        .id("swarm-hive-scroll")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.swarm_panel.hive.scroll)
                        .py(px(6.0))
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .overflow_hidden()
                                .child(stage_layers),
                        ),
                )
                .child(scrollbar::vertical(
                    &self.swarm_panel.hive.scroll,
                    &self.swarm_panel.hive.scrollbar,
                ))
                .into_any_element()
        };

        let selected_card = self
            .swarm_panel
            .hive
            .selected_member
            .as_deref()
            .and_then(|name| snapshot.members.iter().find(|m| m.name == name))
            .map(|member| {
                let status = member_display_status(member);
                let color = member_status_color(status, theme);
                let board_focus = self.swarm_panel.hive.view_in_board_focus.clone();
                let filter_name = member.name.clone();
                let detail = member
                    .failure
                    .as_ref()
                    .map(|f| f.message.clone())
                    .or_else(|| member.activity.as_deref().map(localized_member_activity))
                    .unwrap_or_else(|| {
                        format!(
                            "{} · {}",
                            tr!("swarm.steps_count", count = member.steps),
                            tr!("swarm.posts_count", count = member.posts)
                        )
                    });
                div()
                    .flex_none()
                    .px(px(12.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.composer)
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .min_w_0()
                                    .child(icon(member_status_icon(status), 11.0, color))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(sp(12.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child(member.name.clone()),
                                    )
                                    .child(swarm_badge(member_status_label(status), color, theme)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(sp(11.0))
                                    .text_color(theme.text_tertiary)
                                    .child(detail),
                            ),
                    )
                    .child(
                        div()
                            .id("swarm-hive-view-board")
                            .track_focus(&board_focus)
                            .tab_index(0)
                            .tab_stop(true)
                            .h(px(24.0))
                            .px(px(8.0))
                            .flex_none()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border_strong)
                            .bg(theme.raised)
                            .flex()
                            .items_center()
                            .cursor_default()
                            .focus_visible(|style| style.border_color(theme.accent))
                            .hover(|style| style.bg(theme.overlay_strong))
                            .text_size(sp(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(tr!("swarm.view_in_board"))
                            .on_activation(cx, move |this, window, cx| {
                                this.swarm_panel.hive.stop_replay();
                                this.swarm_panel.member_filter = Some(filter_name.clone());
                                this.swarm_panel.clear_board_selection();
                                this.swarm_panel.active_tab = SwarmPanelTab::Board;
                                this.swarm_panel.follow_tail.set(true);
                                window.focus(&this.swarm_panel.tab_board_focus, cx);
                                cx.notify();
                            }),
                    )
            });

        let replay_bar = (msg_count > 0).then(|| {
            let prev_focus = self.swarm_panel.hive.replay_prev_focus.clone();
            let play_focus = self.swarm_panel.hive.replay_play_focus.clone();
            let next_focus = self.swarm_panel.hive.replay_next_focus.clone();
            let live_focus = self.swarm_panel.hive.replay_live_focus.clone();
            let scrub_focus = self.swarm_panel.hive.scrub_bar_focus.clone();
            let is_live = replay_idx.is_none();
            let fraction = match replay_idx {
                None => 1.0,
                Some(_) if msg_count <= 1 => 1.0,
                Some(idx) => (idx as f32 / (msg_count - 1) as f32).clamp(0.0, 1.0),
            };
            let caption = replay_idx
                .and_then(|idx| snapshot.messages.get(idx))
                .map(replay_caption_text);
            let msg_seqs: Rc<Vec<u64>> = Rc::new(snapshot.messages.iter().map(|m| m.seq).collect());
            let bounds_for_canvas = self.swarm_panel.hive.scrub_bounds.clone();
            let seqs_for_down = Rc::clone(&msg_seqs);
            let seqs_for_move = Rc::clone(&msg_seqs);
            let seqs_for_prev = Rc::clone(&msg_seqs);
            let seqs_for_next = Rc::clone(&msg_seqs);
            let seqs_for_key = Rc::clone(&msg_seqs);

            div()
                .flex_none()
                .px(px(12.0))
                .py(px(7.0))
                .border_t_1()
                .border_color(theme.border)
                .bg(theme.inset)
                .flex()
                .flex_col()
                .gap(px(5.0))
                .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                    if !this.swarm_panel.hive.scrub_dragging {
                        return;
                    }
                    if !event.dragging() {
                        this.swarm_panel.hive.scrub_dragging = false;
                        return;
                    }
                    if let Some(bounds) = this.swarm_panel.hive.scrub_bounds.get() {
                        let frac = scrub_x_to_fraction(
                            f32::from(event.position.x),
                            f32::from(bounds.origin.x),
                            f32::from(bounds.size.width),
                        );
                        if let Some(idx) = scrub_fraction_to_index(frac, seqs_for_move.len()) {
                            this.swarm_panel.hive.replay_cursor_seq = Some(seqs_for_move[idx]);
                            cx.notify();
                        }
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, _| {
                        this.swarm_panel.hive.scrub_dragging = false;
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, _| {
                        this.swarm_panel.hive.scrub_dragging = false;
                    }),
                )
                .when_some(caption, |col, text| {
                    col.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(sp(11.0))
                            .text_color(theme.text_secondary)
                            .child(text),
                    )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .child(
                            div()
                                .id("swarm-hive-prev")
                                .track_focus(&prev_focus)
                                .tab_index(0)
                                .tab_stop(true)
                                .size(px(22.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(gpui::transparent_black())
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_default()
                                .focus_visible(|style| style.border_color(theme.accent))
                                .hover(|style| style.bg(theme.overlay))
                                .active(|style| style.bg(theme.overlay_strong))
                                .child(icon("icons/arrow-left.svg", 13.0, theme.text_tertiary))
                                .tooltip(Tooltip::text(tr!("swarm.replay_prev")))
                                .on_activation(cx, move |this, _, cx| {
                                    this.swarm_panel.hive.stop_replay();
                                    let prev_idx = match replay_idx {
                                        None => seqs_for_prev.len().saturating_sub(1),
                                        Some(c) => c.saturating_sub(1),
                                    };
                                    this.swarm_panel.hive.replay_cursor_seq =
                                        seqs_for_prev.get(prev_idx).copied();
                                    cx.notify();
                                }),
                        )
                        .child(
                            div()
                                .id("swarm-hive-play-pause")
                                .track_focus(&play_focus)
                                .tab_index(0)
                                .tab_stop(true)
                                .size(px(24.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(if replay_playing {
                                    theme.accent.opacity(0.65)
                                } else {
                                    theme.border_strong
                                })
                                .bg(if replay_playing {
                                    theme.overlay_strong
                                } else {
                                    theme.surface
                                })
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_default()
                                .focus_visible(|style| style.border_color(theme.accent))
                                .hover(|style| style.bg(theme.overlay))
                                .tooltip(Tooltip::text(if replay_playing {
                                    tr!("swarm.replay_pause")
                                } else {
                                    tr!("swarm.replay_play")
                                }))
                                .child(render_play_pause_glyph(
                                    replay_playing,
                                    if replay_playing {
                                        theme.accent
                                    } else {
                                        theme.text_secondary
                                    },
                                ))
                                .on_activation(cx, move |this, _, cx| {
                                    if this.swarm_panel.hive.replay_playing {
                                        this.swarm_panel.hive.stop_replay();
                                        cx.notify();
                                    } else {
                                        this.start_swarm_hive_replay(session_id, cx);
                                    }
                                }),
                        )
                        .child(
                            div()
                                .id("swarm-hive-next")
                                .track_focus(&next_focus)
                                .tab_index(0)
                                .tab_stop(true)
                                .size(px(22.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(gpui::transparent_black())
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_default()
                                .focus_visible(|style| style.border_color(theme.accent))
                                .hover(|style| style.bg(theme.overlay))
                                .active(|style| style.bg(theme.overlay_strong))
                                .child(icon("icons/arrow-right.svg", 13.0, theme.text_tertiary))
                                .tooltip(Tooltip::text(tr!("swarm.replay_next")))
                                .on_activation(cx, move |this, _, cx| {
                                    this.swarm_panel.hive.stop_replay();
                                    let next_idx = match replay_idx {
                                        None => seqs_for_next.len().saturating_sub(1),
                                        Some(c) if c + 1 < seqs_for_next.len() => c + 1,
                                        Some(_) => {
                                            this.swarm_panel.hive.replay_cursor_seq = None;
                                            cx.notify();
                                            return;
                                        }
                                    };
                                    this.swarm_panel.hive.replay_cursor_seq =
                                        seqs_for_next.get(next_idx).copied();
                                    cx.notify();
                                }),
                        )
                        .child(
                            div()
                                .id("swarm-hive-scrub-bar")
                                .track_focus(&scrub_focus)
                                .tab_index(0)
                                .tab_stop(true)
                                .relative()
                                .flex_1()
                                .h(px(16.0))
                                .px(px(2.0))
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(gpui::transparent_black())
                                .flex()
                                .items_center()
                                .cursor_default()
                                .tooltip(Tooltip::text(tr!("swarm.hive_scrubber")))
                                .focus_visible(|style| style.border_color(theme.accent))
                                .child(
                                    canvas(
                                        move |bounds, _, _| {
                                            bounds_for_canvas.set(Some(bounds));
                                        },
                                        |_, _, _, _| (),
                                    )
                                    .absolute()
                                    .inset_0(),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .h(px(4.0))
                                        .rounded_full()
                                        .bg(theme.overlay_strong)
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .h_full()
                                                .w(gpui::relative(fraction))
                                                .rounded_full()
                                                .bg(if is_live {
                                                    theme.success.opacity(0.85)
                                                } else {
                                                    theme.accent
                                                }),
                                        ),
                                )
                                .on_key_down(cx.listener(
                                    move |this, event: &KeyDownEvent, _, cx| {
                                        if event.keystroke.modifiers.modified()
                                            || seqs_for_key.is_empty()
                                        {
                                            return;
                                        }
                                        let current = resolve_replay_index_from_seqs(
                                            &seqs_for_key,
                                            this.swarm_panel.hive.replay_cursor_seq,
                                        );
                                        let next_seq = match event.keystroke.key.as_str() {
                                            "left" => {
                                                let idx = current.map_or(
                                                    seqs_for_key.len().saturating_sub(1),
                                                    |c| c.saturating_sub(1),
                                                );
                                                Some(seqs_for_key[idx])
                                            }
                                            "right" => match current {
                                                None => None,
                                                Some(c) if c + 1 < seqs_for_key.len() => {
                                                    Some(seqs_for_key[c + 1])
                                                }
                                                Some(_) => None,
                                            },
                                            "home" => Some(seqs_for_key[0]),
                                            "end" => None,
                                            _ => return,
                                        };
                                        this.swarm_panel.hive.stop_replay();
                                        this.swarm_panel.hive.replay_cursor_seq = next_seq;
                                        cx.notify();
                                        cx.stop_propagation();
                                    },
                                ))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                        this.swarm_panel.hive.stop_replay();
                                        this.swarm_panel.hive.scrub_dragging = true;
                                        if let Some(bounds) =
                                            this.swarm_panel.hive.scrub_bounds.get()
                                        {
                                            let frac = scrub_x_to_fraction(
                                                f32::from(event.position.x),
                                                f32::from(bounds.origin.x),
                                                f32::from(bounds.size.width),
                                            );
                                            if let Some(idx) =
                                                scrub_fraction_to_index(frac, seqs_for_down.len())
                                            {
                                                this.swarm_panel.hive.replay_cursor_seq =
                                                    Some(seqs_for_down[idx]);
                                            }
                                        }
                                        cx.notify();
                                        cx.stop_propagation();
                                    }),
                                )
                                .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                })),
                        )
                        .child(
                            div()
                                .id("swarm-hive-live")
                                .track_focus(&live_focus)
                                .tab_index(0)
                                .tab_stop(true)
                                .h(px(22.0))
                                .px(px(8.0))
                                .flex_none()
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(if is_live {
                                    theme.success.opacity(0.6)
                                } else {
                                    theme.border_strong
                                })
                                .bg(if is_live {
                                    theme.overlay_strong
                                } else {
                                    theme.surface
                                })
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .cursor_default()
                                .focus_visible(|style| style.border_color(theme.accent))
                                .hover(|style| style.bg(theme.overlay))
                                .child(div().size(px(5.0)).rounded_full().bg(if is_live {
                                    theme.success
                                } else {
                                    theme.text_ghost
                                }))
                                .child(
                                    div()
                                        .text_size(sp(11.0))
                                        .text_color(if is_live {
                                            theme.text
                                        } else {
                                            theme.text_secondary
                                        })
                                        .child(tr!("swarm.live")),
                                )
                                .on_activation(cx, |this, _, cx| {
                                    this.swarm_panel.hive.stop_replay();
                                    this.swarm_panel.hive.replay_cursor_seq = None;
                                    cx.notify();
                                }),
                        ),
                )
        });

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(top_bar)
            .child(stage_body)
            .children(selected_card)
            .children(replay_bar)
    }
}

fn resolve_replay_index_from_seqs(seqs: &[u64], cursor_seq: Option<u64>) -> Option<usize> {
    let seq = cursor_seq?;
    if seqs.is_empty() {
        return None;
    }
    seqs.iter()
        .position(|&s| s == seq)
        .or_else(|| seqs.iter().position(|&s| s > seq))
        .or_else(|| Some(seqs.len() - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use waku_client::antigravity_swarm::AntigravitySwarmMemberStatus;

    fn make_member(
        name: &str,
        role: &str,
        status: AntigravitySwarmMemberStatus,
        killed: bool,
    ) -> AntigravitySwarmMember {
        AntigravitySwarmMember {
            name: name.to_owned(),
            conversation_id: name.to_owned(),
            model: "gemini".to_owned(),
            role: role.to_owned(),
            is_role_worker: false,
            wake_mode: "mentions".to_owned(),
            status,
            killed,
            has_posted_done: false,
            task: None,
            last_read_seq: 0,
            unread: 0,
            posts: 1,
            steps: 3,
            activity: None,
            activity_kind: None,
            failure: None,
            transcript_mtime_ms: None,
        }
    }

    fn make_message(
        seq: u64,
        agent: &str,
        mentions: &[&str],
        time: &str,
    ) -> AntigravitySwarmMessage {
        AntigravitySwarmMessage {
            seq,
            agent: agent.to_owned(),
            text: format!("post #{seq}"),
            time: time.to_owned(),
            mentions: mentions.iter().map(|s| (*s).to_owned()).collect(),
            mentions_all: false,
            kind: None,
            channel: None,
            channel_mentions: vec![],
            screenshots: vec![],
        }
    }

    #[test]
    fn flat_top_hex_vertices_keep_points_symmetric_and_horizontal_top() {
        let pts = flat_top_hex_vertices(100.0, 200.0, 80.0);
        // Right & left points lie on horizontal centerline
        assert!((pts[0].0 - 180.0).abs() < 0.01);
        assert!((pts[0].1 - 200.0).abs() < 0.01);
        assert!((pts[3].0 - 20.0).abs() < 0.01);
        assert!((pts[3].1 - 200.0).abs() < 0.01);
        // Bottom-flat and top-flat edges share matching y values
        assert!((pts[1].1 - pts[2].1).abs() < 0.01);
        assert!((pts[4].1 - pts[5].1).abs() < 0.01);
    }

    #[test]
    fn pack_bees_in_hex_places_five_workers_as_three_over_two_inside_bounds() {
        let cx = 160.0;
        let cy = 140.0;
        let r = 110.0;
        let pts = pack_bees_in_hex(5, cx, cy, r, false);
        assert_eq!(pts.len(), 5);
        // First 3 share row 0 (same y), next 2 share row 1 (larger y)
        assert!((pts[0].1 - pts[1].1).abs() < 0.01);
        assert!((pts[1].1 - pts[2].1).abs() < 0.01);
        assert!((pts[3].1 - pts[4].1).abs() < 0.01);
        assert!(pts[3].1 > pts[0].1);

        let half_h = r * HEX_HEIGHT_RATIO * 0.5;
        for (px, py) in pts {
            assert!((py - cy).abs() < half_h);
            let dy_frac = (py - cy).abs() / half_h;
            let hex_half_w = r * (1.0 - 0.5 * dy_frac);
            assert!((px - cx).abs() < hex_half_w);
        }
    }

    #[test]
    fn worker_done_counts_use_has_posted_done_and_exclude_role_workers() {
        use AntigravitySwarmMemberStatus::{Failed, Stopped, Waiting, Working};
        let mut monitor = make_member("monitor", "monitor", Working, false);
        monitor.is_role_worker = true;
        let mut done_waiting = make_member("c", "worker", Waiting, false);
        done_waiting.has_posted_done = true;
        let idle_waiting = make_member("f", "worker", Waiting, false);
        let stopped_member = make_member("g", "worker", Stopped, false);
        let members = vec![
            make_member("coordinator", "coordinator", Waiting, false),
            make_member("a", "worker", Working, false),
            make_member("b", "worker", Working, false),
            done_waiting,
            idle_waiting,
            stopped_member,
            make_member("d", "worker", Failed, false),
            make_member("e", "worker", Working, true),
            monitor,
        ];
        // Done: done_waiting + killed e (2 out of 7 regular workers). idle_waiting & stopped_member are not done.
        assert_eq!(compute_worker_done_counts(&members), (2, 7));
    }

    #[test]
    fn build_hive_scene_layout_produces_working_and_coordinator_chambers() {
        use AntigravitySwarmMemberStatus::{Stopped, Waiting, Working};
        let mut role_narrator = make_member("narrator", "narrator", Working, false);
        role_narrator.is_role_worker = true;
        let snapshot = AntigravitySwarmSnapshot {
            swarm_id: "swarm-1".to_owned(),
            current_member: Some("coordinator".to_owned()),
            title: "Demo".to_owned(),
            head_seq: 5,
            total_messages: 2,
            messages_truncated: false,
            channels: vec!["main".to_owned()],
            members: vec![
                make_member("coordinator", "coordinator", Waiting, false),
                make_member("animator", "worker", Working, false),
                make_member("audio-renderer", "worker", Working, false),
                make_member("brand-assets", "worker", Working, false),
                make_member("reviewer", "worker", Working, false),
                make_member("storyboard", "worker", Working, false),
                make_member("paused-one", "worker", Stopped, false),
                role_narrator,
            ],
            messages: vec![
                make_message(1, "coordinator", &["animator"], "2026-11-24 15:27:00"),
                make_message(5, "reviewer", &["coordinator"], "2026-11-24 15:27:10"),
            ],
            overview: None,
            system_attention: vec![],
            fingerprint: "fp-1".to_owned(),
            board_fingerprint: None,
            roster_fingerprint: None,
        };

        let live =
            build_hive_scene_layout(&snapshot, 360.0, SwarmHiveMode::Hive, false, None, None);
        // Working chamber (5, narrator excluded), Stopped chamber (1), Coordinator chamber (1).
        assert_eq!(live.chambers.len(), 3);
        assert_eq!(live.bees.len(), 7);
        assert!(live.any_working);
        assert!(!live.chambers[0].label_at_bottom);
        assert!(live.chambers[2].label_at_bottom);
        assert!(!live.edges.is_empty());

        let replay =
            build_hive_scene_layout(&snapshot, 360.0, SwarmHiveMode::Hive, false, None, Some(5));
        assert!(replay.edges.iter().any(|e| e.highlighted && e.active_pulse));
        assert!(
            replay
                .bees
                .iter()
                .any(|b| b.name == "reviewer" && b.highlighted)
        );
        assert!(
            replay
                .bees
                .iter()
                .any(|b| b.name == "coordinator" && b.highlighted)
        );

        let mentions =
            build_hive_scene_layout(&snapshot, 360.0, SwarmHiveMode::Mentions, false, None, None);
        assert_eq!(mentions.chambers.len(), 1);
        assert_eq!(mentions.bees.len(), 8);
        assert_eq!(mentions.edges.len(), 2);
    }

    #[test]
    fn replay_cursor_seq_and_scrub_coordinate_mapping() {
        let messages = vec![
            make_message(10, "coordinator", &["alpha"], "2026-11-24 15:27:00"),
            make_message(14, "alpha", &["coordinator"], "2026-11-24 15:27:10"),
            make_message(20, "beta", &["coordinator"], "2026-11-24 15:27:20"),
        ];
        assert_eq!(resolve_replay_index(&messages, None), None);
        assert_eq!(resolve_replay_index(&messages, Some(14)), Some(1));
        assert_eq!(resolve_replay_index(&messages, Some(3)), Some(0));
        assert_eq!(resolve_replay_index(&messages, Some(99)), Some(2));
        assert_eq!(resolve_replay_index(&[], Some(10)), None);

        assert!((scrub_x_to_fraction(50.0, 50.0, 200.0) - 0.0).abs() < 0.001);
        assert!((scrub_x_to_fraction(150.0, 50.0, 200.0) - 0.5).abs() < 0.001);
        assert!((scrub_x_to_fraction(300.0, 50.0, 200.0) - 1.0).abs() < 0.001);
        assert_eq!(scrub_fraction_to_index(0.0, 3), Some(0));
        assert_eq!(scrub_fraction_to_index(0.5, 3), Some(1));
        assert_eq!(scrub_fraction_to_index(1.0, 3), Some(2));
        assert_eq!(scrub_fraction_to_index(0.5, 0), None);
    }

    #[test]
    fn replay_telemetry_and_pacing_compute_time_deltas() {
        let messages = vec![
            make_message(3, "coordinator", &["alpha"], "2026-11-24 15:27:00"),
            make_message(5, "alpha", &["coordinator"], "2026-11-24 15:27:10"),
        ];
        assert_eq!(format_replay_telemetry(&messages, 0), "1 · #3 · 15:27");
        assert_eq!(
            format_replay_telemetry(&messages, 1),
            "2 · #5 · 15:27 (+10s)"
        );
        assert_eq!(
            replay_step_delay(&messages, 0, SwarmHivePacing::Even),
            Duration::from_millis(1200)
        );
        assert_eq!(
            replay_step_delay(&messages, 0, SwarmHivePacing::Real),
            Duration::from_millis(1200)
        );
    }

    #[test]
    fn cubic_bezier_point_matches_endpoints() {
        let p0 = (10.0, 20.0);
        let c1 = (30.0, 80.0);
        let c2 = (70.0, 80.0);
        let p3 = (90.0, 20.0);
        assert_eq!(cubic_bezier_point(p0, c1, c2, p3, 0.0), p0);
        assert_eq!(cubic_bezier_point(p0, c1, c2, p3, 1.0), p3);
        let mid = cubic_bezier_point(p0, c1, c2, p3, 0.5);
        assert!((mid.0 - 50.0).abs() < 0.01);
        assert!(mid.1 > 20.0);
    }
}
