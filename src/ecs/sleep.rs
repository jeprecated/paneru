//! Preserve layout intent while macOS tears down and rebuilds displays at wake.
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use bevy::ecs::schedule::ScheduleLabel as _;
use bevy::prelude::*;
use tracing::{info, warn};

use crate::{
    ecs::{
        FocusedMarker, NativeFullscreenMarker, Position, PreviousManagedStrip, RepositionMarker,
        Scrolling, SpawnCommandsExt, layout::LayoutStrip, window_geometry::RecentDisplayTransfer,
    },
    events::Event,
    manager::{Display, Window, WindowManager},
    platform::WorkspaceId,
};

#[derive(Resource)]
pub(crate) struct Sleeping;

#[derive(Resource)]
struct SleepLayout {
    spaces: HashMap<u32, HashSet<WorkspaceId>>,
    focused: Option<Entity>,
}

#[derive(Resource)]
pub(crate) struct WakeRecovery {
    poll: Timer,
    elapsed: Duration,
    candidate: Vec<(u32, IRect, Vec<WorkspaceId>)>,
    stable: u8,
}

#[derive(Component)]
struct FocusAfterWake;

/// Systems that can adopt transient OS geometry or change layout ownership.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct LayoutActivity;

pub(crate) fn register(app: &mut App) {
    for schedule in [PreUpdate.intern(), Update.intern(), PostUpdate.intern()] {
        app.configure_sets(schedule, LayoutActivity.run_if(layout_available));
    }
    app.add_systems(
        PreUpdate,
        (begin, recover.run_if(resource_exists::<WakeRecovery>))
            .chain()
            .after(super::systems::pump_events)
            .before(super::display::reconcile_displays)
            .before(LayoutActivity),
    );
    app.add_systems(
        PostUpdate,
        restore_focus
            .after(super::systems::commit_window_position)
            .after(super::systems::commit_window_size),
    );
}

pub(crate) fn layout_available(
    sleeping: Option<Res<Sleeping>>,
    waking: Option<Res<WakeRecovery>>,
) -> bool {
    sleeping.is_none() && waking.is_none()
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn begin(
    mut events: MessageReader<Event>,
    strips: Query<(&LayoutStrip, &ChildOf)>,
    displays: Query<&Display>,
    focused: Query<Entity, With<FocusedMarker>>,
    saved: Option<Res<SleepLayout>>,
    wm: Res<WindowManager>,
    mut commands: Commands,
) {
    for event in events.read() {
        if !matches!(event, Event::SystemWillSleep | Event::SystemWoke { .. }) {
            continue;
        }
        if matches!(event, Event::SystemWillSleep) || saved.is_none() {
            let mut spaces: HashMap<u32, HashSet<WorkspaceId>> = HashMap::new();
            for (strip, parent) in &strips {
                if let Ok(display) = displays.get(parent.parent()) {
                    spaces.entry(display.id()).or_default().insert(strip.id());
                }
            }
            if matches!(event, Event::SystemWillSleep) {
                // Include empty and fullscreen native Spaces too, so an
                // already-existing Space cannot look like a replacement.
                for (display, native_spaces) in wm.0.present_displays() {
                    if !native_spaces.is_empty() {
                        spaces.insert(display.id(), native_spaces.into_iter().collect());
                    }
                }
            }
            commands.insert_resource(SleepLayout {
                spaces,
                focused: focused.iter().next(),
            });
        }
        if matches!(event, Event::SystemWillSleep) {
            commands.queue(|world: &mut World| {
                let strips = world
                    .query_filtered::<Entity, With<LayoutStrip>>()
                    .iter(world)
                    .collect::<Vec<_>>();
                for entity in strips {
                    let target = world.get::<RepositionMarker>(entity).map(|target| target.0);
                    if let Some(target) = target
                        && let Some(mut position) = world.get_mut::<Position>(entity)
                    {
                        position.0 = target;
                    }
                    world
                        .entity_mut(entity)
                        .remove::<(Scrolling, RepositionMarker)>();
                }
            });
            commands.insert_resource(Sleeping);
            commands.remove_resource::<WakeRecovery>();
            info!("Preserving layouts while the system sleeps");
        } else {
            commands.remove_resource::<Sleeping>();
            commands.insert_resource(WakeRecovery {
                poll: Timer::new(Duration::from_millis(750), TimerMode::Once),
                elapsed: Duration::ZERO,
                candidate: Vec::new(),
                stable: 0,
            });
            info!("Waiting for displays and window lists to settle after wake");
        }
    }
}

fn recover(
    mut recovery: ResMut<WakeRecovery>,
    saved: Res<SleepLayout>,
    wm: Res<WindowManager>,
    time: Res<Time>,
    mut commands: Commands,
) {
    // The first wake frame includes the entire sleep in its elapsed time.
    // Start measuring readiness on the next frame, never skip the settle gate.
    if recovery.is_added() {
        return;
    }
    recovery.elapsed += time.delta();
    recovery.poll.tick(time.delta());
    if !recovery.poll.is_finished() {
        return;
    }
    recovery.poll = Timer::new(Duration::from_millis(250), TimerMode::Once);
    let snapshot = wm.0.present_displays();
    let mut signature = snapshot
        .iter()
        .map(|(display, spaces)| {
            let mut spaces = spaces.clone();
            spaces.sort_unstable();
            (display.id(), display.bounds(), spaces)
        })
        .collect::<Vec<_>>();
    signature.sort_by_key(|entry| entry.0);
    let complete = !snapshot.is_empty() && snapshot.iter().all(|(_, spaces)| !spaces.is_empty());
    let windows_ready = complete
        && snapshot.iter().all(|(_, spaces)| {
            spaces
                .iter()
                .all(|space| wm.windows_in_workspace(*space).is_ok())
        });
    if windows_ready && signature == recovery.candidate {
        recovery.stable += 1;
    } else {
        recovery.stable = 0;
    }
    recovery.candidate = signature;
    let missing_display = saved
        .spaces
        .keys()
        .any(|id| !snapshot.iter().any(|(d, _)| d.id() == *id));
    let timed_out = recovery.elapsed >= Duration::from_secs(8);
    if !timed_out
        && (recovery.stable < 2 || missing_display && recovery.elapsed < Duration::from_secs(4))
    {
        return;
    }
    if !windows_ready {
        if !timed_out {
            return;
        }
        warn!("Wake window lists are still unavailable; retaining existing layout ownership");
    }
    let mut remaps = Vec::new();
    for (display, current) in &snapshot {
        if let Some(previous) = saved.spaces.get(&display.id()) {
            let missing = previous
                .iter()
                .filter(|id| !current.contains(id))
                .copied()
                .collect::<Vec<_>>();
            let added = current
                .iter()
                .filter(|id| !previous.contains(id))
                .copied()
                .collect::<Vec<_>>();
            // Only a unique replacement is safe; real Space switches leave the
            // old Space in the display list and do not remap any rows.
            if missing.len() == 1 && added.len() == 1 {
                remaps.push((missing[0], added[0]));
            }
        }
    }
    commands.queue(move |world: &mut World| remap_spaces(world, &remaps));
    if complete {
        commands.run_system_cached_with(super::display::apply_display_snapshot, snapshot);
    }
    let focused = saved.focused;
    let until = time.elapsed() + Duration::from_secs(2);
    commands.queue(move |world: &mut World| {
        let windows = world
            .query_filtered::<Entity, With<Window>>()
            .iter(world)
            .collect::<Vec<_>>();
        for entity in windows {
            world
                .entity_mut(entity)
                .insert(RecentDisplayTransfer { until });
        }
        if let Some(entity) = focused
            && let Ok(mut entity) = world.get_entity_mut(entity)
        {
            entity.insert(FocusAfterWake);
        }
    });
    super::reload::request_wake_reload(&mut commands);
    commands.remove_resource::<WakeRecovery>();
    commands.remove_resource::<SleepLayout>();
    info!("Restoring preserved layouts after wake");
}

fn remap_spaces(world: &mut World, remaps: &[(WorkspaceId, WorkspaceId)]) {
    for &(old, new) in remaps {
        for mut strip in world.query::<&mut LayoutStrip>().iter_mut(world) {
            if strip.id() == old {
                strip.set_workspace_id(new);
            }
        }
        for mut previous in world.query::<&mut PreviousManagedStrip>().iter_mut(world) {
            if previous.workspace_id == old {
                previous.workspace_id = new;
            }
        }
        for mut fullscreen in world.query::<&mut NativeFullscreenMarker>().iter_mut(world) {
            if fullscreen.workspace_id == old {
                fullscreen.workspace_id = new;
            }
        }
        world
            .resource_mut::<super::focus::FocusHistory>()
            .remap_workspace(old, new);
        info!(
            old,
            new, "Reattached existing virtual rows to replacement native Space"
        );
    }
}

fn restore_focus(pending: Populated<Entity, With<FocusAfterWake>>, mut commands: Commands) {
    for entity in &pending {
        commands.entity(entity).remove::<FocusAfterWake>();
        super::workspace::spawn_restore_focus_guard(entity, &mut commands);
        commands.focus_entity(entity, true);
    }
}
