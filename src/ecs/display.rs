use bevy::app::{App, Plugin, PostUpdate, PreUpdate};
use bevy::ecs::change_detection::DetectChangesMut;
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::Add;
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Commands, Local, NonSend, Query, Res, RunSystemOnce};
use bevy::ecs::world::World;
use bevy::math::IRect;
use bevy::platform::collections::HashSet;
use bevy::time::{Time, Timer, TimerMode};
use objc2_app_kit::NSScreen;
use objc2_core_graphics::CGDirectDisplayID;
use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;
use tracing::{Level, debug, error, info, instrument, warn};

use crate::commands::Command;
use crate::config::Config;
use crate::ecs::layout::{LayoutStrip, PARKED_STRIP_SLIVER};
use crate::ecs::workspace::PreviousStripPosition;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, Bounds, EnsureVisibleMarker, FocusedMarker,
    LayoutPosition, ManualStripOffset, Position, ReadDisplayProperties, RepositionMarker,
    ResizeMarker, SelectedVirtualMarker, SendMessageTrigger, SpawnCommandsExt, Timeout, WidthRatio,
};
use crate::events::Event;
use crate::manager::{Display, WindowManager, irect_from};
use crate::platform::{PlatformCallbacks, WorkspaceId};
use crate::util::{read_screen_property, round_px};

const ORPHANED_SPACES_TIMEOUT_SEC: u64 = 30;
const DISPLAY_SETTLE_DELAY: Duration = Duration::from_millis(350);
const DISPLAY_VERIFY_DELAY: Duration = Duration::from_secs(1);
const DISPLAY_RETRY_DELAY: Duration = Duration::from_secs(1);
const DISPLAY_RETRIES: u8 = 3;

#[derive(Default)]
pub(crate) struct DisplayReconcileState {
    pending: Option<Timer>,
    retries_left: u8,
    verify_again: bool,
    reload: bool,
}

/// Suppress transient AX geometry and Space ownership while monitors settle.
#[derive(bevy::prelude::Resource)]
pub(crate) struct DisplayReconfiguring {
    until: Duration,
}

/// Original lane identity while its physical monitor is disconnected.
#[derive(Component, Clone, Copy)]
pub(crate) struct DisplacedWorkspace {
    pub display_id: u32,
    pub workspace_id: WorkspaceId,
    pub virtual_index: u32,
}

fn finish_reconfiguration(
    recovery: Res<DisplayReconfiguring>,
    time: Res<Time>,
    mut commands: Commands,
) {
    if time.elapsed() >= recovery.until {
        commands.remove_resource::<DisplayReconfiguring>();
    }
}

pub struct DisplayEventsPlugin;

impl Plugin for DisplayEventsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            (
                display_change_handler.in_set(super::sleep::LayoutActivity),
                reconcile_displays,
            )
                .chain()
                .after(super::systems::pump_events),
        )
        .add_systems(
            PostUpdate,
            finish_reconfiguration.run_if(
                bevy::ecs::schedule::common_conditions::resource_exists::<DisplayReconfiguring>,
            ),
        )
        .add_observer(read_display_properties_trigger)
        .add_observer(cleanup_active_display_marker);
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn cleanup_active_display_marker(
    trigger: On<Add, ActiveDisplayMarker>,
    displays: Query<(Entity, Has<ActiveDisplayMarker>), With<Display>>,
    mut commands: Commands,
) {
    for (entity, active) in displays {
        if active
            && entity != trigger.entity
            && let Ok(mut cmd) = commands.get_entity(entity)
        {
            debug!("Display id {entity} lost active marker.");
            cmd.try_remove::<ActiveDisplayMarker>();
        }
    }
}

/// Handles display change events.
#[instrument(level = Level::DEBUG, skip_all, fields(trigger))]
fn display_change_handler(
    mut messages: MessageReader<Event>,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if !messages
        .read()
        .any(|event| matches!(event, Event::DisplayChanged))
    {
        return;
    }

    let Ok(active_id) = window_manager.active_display_id() else {
        error!("Unable to get active display id!");
        return;
    };

    for (display, entity, focused) in displays {
        let display_id = display.id();
        if !focused
            && display_id == active_id
            && let Ok(mut cmd) = commands.get_entity(entity)
        {
            debug!("Display id {display_id} is active");
            cmd.try_insert(ActiveDisplayMarker);
        }
    }
    commands.trigger(SendMessageTrigger(Event::SpaceChanged));
}

/// Full reconciliation of the ECS display set against the OS truth.
///
/// Runs after a brief quiet period following wake or a display notification.
/// macOS may report an empty or incomplete display list during reconfiguration;
/// applying that transient list would orphan every workspace. A successful
/// scan refreshes both display ownership and the positions of its strips.
#[allow(clippy::too_many_lines)]
pub(crate) fn reconcile_displays(
    mut messages: MessageReader<Event>,
    window_manager: Res<WindowManager>,
    time: Res<Time>,
    mut state: Local<DisplayReconcileState>,
    mut commands: Commands,
    sleeping: Option<Res<super::sleep::Sleeping>>,
    waking: Option<Res<super::sleep::WakeRecovery>>,
) {
    if sleeping.is_some() {
        messages.clear();
        state.pending = None;
        return;
    }
    if waking.is_some() {
        messages.clear();
        // Wake recovery's stable snapshot can still precede WindowServer's
        // final topology. Keep a bounded verification armed even when every
        // configuration callback arrived while recovery was blocking layouts.
        state.pending = Some(Timer::new(DISPLAY_SETTLE_DELAY, TimerMode::Once));
        state.retries_left = DISPLAY_RETRIES;
        state.verify_again = true;
        state.reload = false;
        return;
    }
    let needs_reconcile = messages.read().fold(false, |changed, event| {
        let reload = matches!(
            event,
            Event::Command {
                command: Command::Reload
            }
        );
        state.reload |= reload;
        changed
            | reload
            | matches!(
                event,
                Event::SystemWoke { .. }
                    | Event::DisplayAdded { .. }
                    | Event::DisplayRemoved { .. }
                    | Event::DisplayMoved { .. }
                    | Event::DisplayResized { .. }
                    | Event::DisplayConfigured { .. }
            )
    });
    if needs_reconcile {
        commands.insert_resource(DisplayReconfiguring {
            until: time.elapsed() + Duration::from_secs(2),
        });
        state.pending = Some(Timer::new(DISPLAY_SETTLE_DELAY, TimerMode::Once));
        state.retries_left = DISPLAY_RETRIES;
        state.verify_again = true;
        return;
    }

    let Some(timer) = state.pending.as_mut() else {
        return;
    };
    timer.tick(time.delta());
    if !timer.is_finished() {
        return;
    }
    state.pending = None;

    debug!("Reconciling displays against OS after wake / resize / configure");
    commands.insert_resource(DisplayReconfiguring {
        until: time.elapsed() + Duration::from_secs(2),
    });

    let present_displays: HashMap<CGDirectDisplayID, _> = window_manager
        .0
        .present_displays()
        .into_iter()
        .map(|(display, workspaces)| (display.id(), (display, workspaces)))
        .collect();
    if present_displays.is_empty()
        || present_displays
            .values()
            .any(|(_, spaces)| spaces.is_empty())
    {
        if state.retries_left > 0 {
            state.retries_left -= 1;
            warn!("Display list is incomplete; retrying in {DISPLAY_RETRY_DELAY:?}");
            state.pending = Some(Timer::new(DISPLAY_RETRY_DELAY, TimerMode::Once));
            return;
        }
        warn!("Display list is still incomplete; retaining the existing layouts");
        return;
    }

    info!(
        displays = ?present_displays.iter().map(|(id, (display, spaces))| (*id, display.bounds(), spaces)).collect::<Vec<_>>(),
        "Reconciled display topology"
    );
    commands.run_system_cached_with(
        apply_display_snapshot,
        present_displays.into_values().collect(),
    );
    // Rescan after ownership has settled, including newly discovered windows.
    crate::ecs::reload::request_reload(&mut commands, state.reload);
    state.reload = false;
    commands.trigger(SendMessageTrigger(Event::DisplayChanged));
    if state.verify_again {
        // A begin-configuration callback can precede the final macOS space
        // assignment even after a quiet period. Verify once more without
        // requiring a second notification from Core Graphics.
        state.verify_again = false;
        state.pending = Some(Timer::new(DISPLAY_VERIFY_DELAY, TimerMode::Once));
    }
}

/// Apply one complete display snapshot and rebase existing strip offsets.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn apply_display_snapshot(
    bevy::prelude::In(snapshot): bevy::prelude::In<Vec<(Display, Vec<WorkspaceId>)>>,
    world: &mut World,
) {
    rehome_disconnected_workspaces(world, &snapshot);
    remap_replaced_spaces(world, &snapshot);
    if let Err(error) = world.run_system_once_with(apply_display_ownership, snapshot) {
        error!(%error, "Unable to reconcile display ownership");
    }
}

/// Keep populated lanes usable on a sole remaining screen, even when macOS
/// removes their native Space. Retain their home so reconnecting restores the
/// original lane numbers instead of treating them as new laptop windows.
#[allow(clippy::too_many_lines)]
fn rehome_disconnected_workspaces(world: &mut World, snapshot: &[(Display, Vec<WorkspaceId>)]) {
    let mut strips = world
        .query::<(
            Entity,
            &LayoutStrip,
            Option<&ChildOf>,
            Option<&DisplacedWorkspace>,
        )>()
        .iter(world)
        .filter_map(|(entity, strip, child, home)| {
            let display_id = child
                .and_then(|child| world.get::<Display>(child.parent()))
                .map(Display::id)
                .or_else(|| home.map(|home| home.display_id))?;
            Some((
                entity,
                strip.id(),
                strip.virtual_index,
                strip.len(),
                display_id,
                home.copied(),
            ))
        })
        .collect::<Vec<_>>();
    strips.sort_by_key(|(_, space, index, _, display, _)| (*display, *space, *index));
    // Restore homes before matching replacements on the laptop: both groups
    // may temporarily share its native Space while the external is absent.
    for &(entity, _, _, _, _, home) in &strips {
        let Some(home) = home else {
            continue;
        };
        let Some((_, spaces)) = snapshot
            .iter()
            .find(|(display, _)| display.id() == home.display_id)
        else {
            continue;
        };
        let target = spaces
            .iter()
            .find(|space| **space == home.workspace_id)
            .copied()
            .or_else(|| (spaces.len() == 1).then(|| spaces[0]));
        let Some(target) = target else {
            continue;
        };
        if let Some(mut strip) = world.get_mut::<LayoutStrip>(entity) {
            strip.set_workspace_id(target);
            strip.virtual_index = home.virtual_index;
        }
        world.entity_mut(entity).remove::<DisplacedWorkspace>();
    }
    let present_spaces = snapshot
        .iter()
        .flat_map(|(_, spaces)| spaces)
        .collect::<HashSet<_>>();
    let fallback = match snapshot {
        [(_, spaces)] if spaces.len() == 1 => Some(spaces[0]),
        _ => None,
    };
    let mut offset = fallback.map_or(0, |space| {
        strips
            .iter()
            .filter(|(_, id, _, _, _, _)| *id == space)
            .map(|(_, _, index, _, _, _)| index + 1)
            .max()
            .unwrap_or(0)
    });
    let mut next_offset = offset;
    let mut group = None;
    let mut rehomed = false;
    for (entity, space, index, len, display_id, home) in strips {
        if home.is_some()
            || snapshot
                .iter()
                .any(|(display, _)| display.id() == display_id)
        {
            continue;
        }
        world.entity_mut(entity).insert(DisplacedWorkspace {
            display_id,
            workspace_id: space,
            virtual_index: index,
        });
        if len > 0
            && !present_spaces.contains(&space)
            && let Some(target) = fallback
            && let Some(mut strip) = world.get_mut::<LayoutStrip>(entity)
        {
            if group != Some((display_id, space)) {
                group = Some((display_id, space));
                offset = next_offset;
            }
            strip.set_workspace_id(target);
            strip.virtual_index += offset;
            next_offset = next_offset.max(strip.virtual_index + 1);
            rehomed = true;
        }
    }
    if rehomed && let Some(space) = fallback {
        select_rehomed_workspace(world, space);
    }
}

/// A fallback combines physical Space groups, but still has only one visible
/// lane. Keep the active lane and park the other group's formerly visible row.
fn select_rehomed_workspace(world: &mut World, space: WorkspaceId) {
    let rows = world
        .query::<(
            Entity,
            &LayoutStrip,
            Has<ActiveWorkspaceMarker>,
            Has<SelectedVirtualMarker>,
        )>()
        .iter(world)
        .filter(|(_, strip, _, _)| strip.id() == space)
        .map(|(entity, strip, active, selected)| (entity, strip.virtual_index, active, selected))
        .collect::<Vec<_>>();
    let Some(&(selected, _, _, _)) = rows
        .iter()
        .min_by_key(|(_, index, active, selected)| (!active, !selected, *index))
    else {
        return;
    };
    for (entity, _, _, _) in rows {
        if entity == selected {
            world.entity_mut(entity).insert(SelectedVirtualMarker);
        } else {
            let origin = world.get::<Position>(entity).map(|position| position.0);
            world.entity_mut(entity).remove::<SelectedVirtualMarker>();
            if world.get::<PreviousStripPosition>(entity).is_none()
                && let Some(origin) = origin
            {
                world.entity_mut(entity).insert(PreviousStripPosition {
                    origin,
                    focus: None,
                });
            }
        }
    }
}

/// A hotplug can replace a native Space without a sleep/wake notification.
/// Match only a unique replacement on the same display; a Space that is still
/// present on another monitor is being moved, not replaced.
fn remap_replaced_spaces(world: &mut World, snapshot: &[(Display, Vec<WorkspaceId>)]) {
    let all_spaces = snapshot
        .iter()
        .flat_map(|(_, spaces)| spaces)
        .collect::<HashSet<_>>();
    let previous = world
        .query::<(&LayoutStrip, &ChildOf)>()
        .iter(world)
        .filter_map(|(strip, child)| {
            world
                .get::<Display>(child.parent())
                .map(|display| (display.id(), strip.id()))
        })
        .fold(
            HashMap::<u32, HashSet<WorkspaceId>>::new(),
            |mut groups, (display, space)| {
                groups.entry(display).or_default().insert(space);
                groups
            },
        );
    let mut remaps = Vec::new();
    for (display, spaces) in snapshot {
        let Some(previous) = previous.get(&display.id()) else {
            continue;
        };
        let missing = previous
            .iter()
            .filter(|space| !all_spaces.contains(space))
            .collect::<Vec<_>>();
        let added = spaces
            .iter()
            .filter(|space| !previous.contains(*space))
            .collect::<Vec<_>>();
        if let ([old], [new]) = (missing.as_slice(), added.as_slice()) {
            remaps.push((**old, **new));
        }
    }
    super::sleep::remap_spaces(world, &remaps);
}

#[allow(clippy::needless_pass_by_value)]
fn apply_display_ownership(
    bevy::prelude::In(snapshot): bevy::prelude::In<Vec<(Display, Vec<WorkspaceId>)>>,
    workspaces: Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    mut displays: Query<(&mut Display, Entity)>,
    mut commands: Commands,
) {
    let mut present_displays: HashMap<CGDirectDisplayID, _> = snapshot
        .into_iter()
        .map(|(display, spaces)| (display.id(), (display, spaces)))
        .collect();
    let previous_bounds: HashMap<Entity, IRect> = displays
        .iter()
        .map(|(display, entity)| (entity, display.bounds()))
        .collect();
    let previous_parents: HashMap<Entity, IRect> = workspaces
        .iter()
        .filter_map(|(_, entity, child)| {
            let bounds = previous_bounds.get(&child?.parent())?;
            Some((entity, *bounds))
        })
        .collect();

    let existing_displays: HashMap<CGDirectDisplayID, _> = displays
        .iter()
        .map(|(display, workspaces)| (display.id(), (display, workspaces)))
        .collect();

    let present_ids = present_displays.keys().copied().collect::<HashSet<_>>();
    let existing_ids = existing_displays.keys().copied().collect::<HashSet<_>>();

    // Displays that vanished while we were away (e.g. unplugged during sleep).
    for display_id in existing_ids.difference(&present_ids) {
        let Some((display, _)) = existing_displays.get(display_id) else {
            error!("Unable to find removed display: {display_id}");
            continue;
        };
        remove_display(display, &workspaces, &displays, &mut commands);
    }

    // Displays that appeared while we were away.
    for display_id in present_ids.difference(&existing_ids) {
        let Some((display, workspace_ids)) = present_displays.remove(display_id) else {
            error!("Unable to find added display: {display_id}");
            continue;
        };
        add_display(display, &workspace_ids, &workspaces, &mut commands);
    }

    // Displays that are still present: refresh their bounds (resolution or
    // menubar may have changed) and re-home any workspaces that drifted.
    for display_id in present_ids.intersection(&existing_ids) {
        let Some((display, workspace_ids)) = present_displays.remove(display_id) else {
            continue;
        };
        move_display(
            display,
            &workspace_ids,
            &mut displays,
            &workspaces,
            &mut commands,
        );
    }

    commands.queue(move |world: &mut World| refresh_display_layout(world, &previous_parents));
}

/// Rebase each strip onto its current display and ask the layout and OS writers
/// to replay window frames. This also covers wakeups where macOS moved windows
/// without changing the reported display geometry.
pub(crate) fn refresh_display_layout(world: &mut World, previous_parents: &HashMap<Entity, IRect>) {
    let strips = world
        .query_filtered::<Entity, With<LayoutStrip>>()
        .iter(world)
        .collect::<Vec<_>>();
    let focused = world
        .query_filtered::<Entity, With<FocusedMarker>>()
        .iter(world)
        .next();

    for strip_entity in strips {
        crate::ecs::workspace::spawn_snap_strip_guard(strip_entity, &mut world.commands());
        refresh_strip_layout(
            world,
            strip_entity,
            previous_parents.get(&strip_entity).copied(),
            focused,
        );
    }
}

/// Replays a strip that acquired a display after the initial reconciliation.
/// macOS may publish the space ownership later than the display itself.
pub(crate) fn refresh_reparented_strip(world: &mut World, strip_entity: Entity) {
    let focused = world
        .query_filtered::<Entity, With<FocusedMarker>>()
        .iter(world)
        .next();
    refresh_strip_layout(world, strip_entity, None, focused);
}

fn refresh_strip_layout(
    world: &mut World,
    strip_entity: Entity,
    previous_bounds: Option<IRect>,
    focused: Option<Entity>,
) {
    let Some(display_entity) = world.get::<ChildOf>(strip_entity).map(ChildOf::parent) else {
        return;
    };
    let Some(display_bounds) = world.get::<Display>(display_entity).map(Display::bounds) else {
        return;
    };
    if previous_bounds.is_none() {
        // A rescued orphan has no usable display-relative scroll offset.
        world.entity_mut(strip_entity).remove::<ManualStripOffset>();
    }
    let hidden = world.get::<PreviousStripPosition>(strip_entity).is_some();
    if let Some(mut position) = world.get_mut::<Position>(strip_entity) {
        let origin = if hidden {
            display_bounds.max - PARKED_STRIP_SLIVER
        } else if let Some(previous_bounds) = previous_bounds {
            position.0 + display_bounds.min - previous_bounds.min
        } else {
            display_bounds.min
        };
        if position.0 != origin {
            position.0 = origin;
        }
    }
    if let Some(mut previous) = world.get_mut::<PreviousStripPosition>(strip_entity) {
        previous.origin = previous_bounds.map_or(display_bounds.min, |bounds| {
            previous.origin + display_bounds.min - bounds.min
        });
    }
    if let Some(mut target) = world.get_mut::<RepositionMarker>(strip_entity) {
        target.0 = if hidden {
            display_bounds.max - PARKED_STRIP_SLIVER
        } else {
            previous_bounds.map_or(display_bounds.min, |bounds| {
                target.0 + display_bounds.min - bounds.min
            })
        };
    }

    let Some(mut strip) = world.get_mut::<LayoutStrip>(strip_entity) else {
        return;
    };
    let windows = strip.all_windows();
    strip.set_changed();
    if let Some(focused) = focused.filter(|focused| windows.contains(focused)) {
        world
            .entity_mut(focused)
            .insert(EnsureVisibleMarker { snap: true });
    }
    for window in windows {
        // Carry the user's width fraction to the new screen before the size
        // writer derives a new ratio from it. Replaying the old pixel width
        // first permanently reinterpreted it against the smaller display.
        if previous_bounds.is_none_or(|bounds| bounds.width() != display_bounds.width())
            && world
                .get::<super::supplementary::SupplementaryWindow>(window)
                .is_none()
            && let Some(ratio) = world.get::<WidthRatio>(window).map(|ratio| ratio.0)
            && let Some(mut bounds) = world.get_mut::<Bounds>(window)
        {
            bounds.0.x = round_px(ratio * f64::from(display_bounds.width()));
            world.entity_mut(window).remove::<ResizeMarker>();
        }
        if let Some(mut layout) = world.get_mut::<LayoutPosition>(window) {
            layout.set_changed();
        }
        if let Some(mut position) = world.get_mut::<Position>(window) {
            position.set_changed();
        }
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn add_display(
    display: Display,
    workspace_ids: &[WorkspaceId],
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    let display_id = display.id();
    debug!("Display Added: {display_id}");

    let display_bounds = display.bounds();
    let display_entity = commands.spawn(display).id();
    commands.trigger(ReadDisplayProperties(display_entity));

    reparent_existing_workspaces(
        workspace_ids,
        display_entity,
        &display_bounds,
        existing_strips,
        commands,
    );
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn remove_display(
    display: &Display,
    workspaces: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    displays: &Query<(&mut Display, Entity)>,
    commands: &mut Commands,
) {
    let display_id = display.id();
    debug!("Display Removed: {display_id:?}");
    let Some((display, display_entity)) = displays
        .into_iter()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find removed display!");
        return;
    };

    for (strip, entity, _) in workspaces
        .into_iter()
        .filter(|(_, _, child)| child.is_some_and(|child| child.parent() == display_entity))
    {
        let display_id = display.id();
        debug!(
            "orphaning strip {} after removal of display {display_id}.",
            strip.id(),
        );
        let timeout = Timeout::new(
            Duration::from_secs(ORPHANED_SPACES_TIMEOUT_SEC),
            Some(format!(
                "Orphaned strip {} ({strip}) could not be re-inserted after {ORPHANED_SPACES_TIMEOUT_SEC}s.",
                strip.id()
            )),
            commands,
        );
        if let Ok(mut commands) = commands.get_entity(entity) {
            commands.try_insert(timeout);
        }
        if let Ok(mut commands) = commands.get_entity(display_entity) {
            commands.detach_child(entity);
        }
    }

    if let Ok(mut commands) = commands.get_entity(display_entity) {
        commands.try_despawn();
    }
}

#[instrument(level = Level::DEBUG, skip_all, fields(display_id))]
fn move_display(
    moved_display: Display,
    workspace_ids: &[WorkspaceId],
    displays: &mut Query<(&mut Display, Entity)>,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    let display_id = moved_display.id();
    debug!("Display Moved: {display_id:?}");
    let Some((mut display, display_entity)) = displays
        .iter_mut()
        .find(|(display, _)| display.id() == display_id)
    else {
        error!("Unable to find moved display!");
        return;
    };
    *display = moved_display;
    commands.trigger(ReadDisplayProperties(display_entity));

    reparent_existing_workspaces(
        workspace_ids,
        display_entity,
        &display.bounds(),
        existing_strips,
        commands,
    );
}

fn reparent_existing_workspaces(
    workspace_ids: &[WorkspaceId],
    display_entity: Entity,
    display_bounds: &IRect,
    existing_strips: &Query<(&LayoutStrip, Entity, Option<&ChildOf>)>,
    commands: &mut Commands,
) {
    // Verifies that a moved display has all the workspaces which it owns.
    for &id in workspace_ids {
        let mut found = false;
        for (strip, entity, child) in existing_strips {
            if strip.id() == id {
                found = true;
                if child.is_none_or(|child| child.parent() != display_entity) {
                    // Re-parent this workspace
                    if let Ok(mut cmd) = commands.get_entity(entity) {
                        debug!("reparenting workspace {id} to display {display_entity}");
                        cmd.try_remove::<Timeout>()
                            .try_remove::<ChildOf>()
                            .try_insert(ChildOf(display_entity));
                    }
                }
            }
        }

        if !found {
            // New workspace.
            let origin = display_bounds.min;
            debug!("new workspace {id} on display {display_entity}");
            commands.spawn_layout_strip(LayoutStrip::new(id, 0), origin, display_entity, false);
        }
    }
}

/// Tracks whether floating windows on a workspace sit above or behind tiled
/// ones in the OS z-order. Default is `Front` (floats above tiles).
#[derive(Clone, Component, Copy)]
pub struct FloatingLayer {
    pub workspace_id: WorkspaceId,
    pub front: bool,
}

impl FloatingLayer {
    pub fn new(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            front: false,
        }
    }

    pub fn flip(&mut self) {
        self.front = !self.front;
    }
}

fn read_display_properties_trigger(
    trigger: On<ReadDisplayProperties>,
    mut displays: Query<(&mut Display, Entity)>,
    platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    config: Option<Res<Config>>,
    mut commands: Commands,
) {
    let Ok((mut display, entity)) = displays.get_mut(trigger.event().0) else {
        return;
    };
    let display_id = display.id();

    // NSScreen::screen needs to run in the main thread, thus we run it in a NonSend trigger.
    let Some(screens) = platform.map(|platform| NSScreen::screens(platform.main_thread_marker))
    else {
        return;
    };

    let notch = read_screen_property(&screens, display_id, |screen| {
        let insets = screen.safeAreaInsets();
        debug!("notch on display {display_id}: {insets:?}");
        round_px(insets.top)
    });
    if let Some(height) = notch {
        display.set_notch_height(height);
    }

    let dock = read_screen_property(&screens, display_id, |screen| {
        let visible_frame = cocoa_visible_frame_to_cg(
            display.bounds().min - crate::manager::Origin::new(0, display.menubar_height()),
            irect_from(screen.frame()),
            irect_from(screen.visibleFrame()),
        );
        display.locate_dock(&visible_frame)
    });
    if let Some(dock) = dock {
        debug!("dock on display {display_id}: {:?}", dock);
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(dock);
        }
    }

    if let Some(config) = config {
        let height = config.menubar_height();
        display.set_menubar_height_override(height);
    }
}

/// Cocoa is Y-up; derive CG coordinates relative to this screen's raw origin.
/// Comparing Cocoa's absolute Y with CG's absolute Y misidentifies the Dock on
/// vertically arranged displays and reserves a large, spurious bottom margin.
fn cocoa_visible_frame_to_cg(
    origin: crate::manager::Origin,
    screen: IRect,
    visible: IRect,
) -> IRect {
    IRect::from_corners(
        origin
            + crate::manager::Origin::new(
                visible.min.x - screen.min.x,
                screen.max.y - visible.max.y,
            ),
        origin
            + crate::manager::Origin::new(
                visible.max.x - screen.min.x,
                screen.max.y - visible.min.y,
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ecs::DockPosition, manager::Origin};

    #[test]
    fn dock_insets_follow_each_screens_coordinate_system() {
        for y in [-1200, 0, 768] {
            let display = Display::new(1, IRect::new(1024, y, 2944, y + 1200), 20);
            let screen = IRect::new(1024, 768 - y - 1200, 2944, 768 - y);
            let visible = IRect::new(
                screen.min.x,
                screen.min.y + 60,
                screen.max.x,
                screen.max.y - 20,
            );
            let cg = cocoa_visible_frame_to_cg(Origin::new(1024, y), screen, visible);
            assert!(matches!(display.locate_dock(&cg), DockPosition::Bottom(60)));
            let visible = IRect::new(screen.min.x, screen.min.y, screen.max.x, screen.max.y - 20);
            let cg = cocoa_visible_frame_to_cg(Origin::new(1024, y), screen, visible);
            assert!(matches!(display.locate_dock(&cg), DockPosition::Hidden));
        }
    }
}
