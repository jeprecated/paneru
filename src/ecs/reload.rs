//! Recovery scans preserve the user's layout while reconciling OS ownership.
use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
};

use bevy::{
    prelude::*,
    tasks::{AsyncComputeTaskPool, futures_lite::future},
};
use tracing::{info, warn};

use crate::{
    config::Config,
    ecs::{
        Bounds, BruteforceWindows, LayoutPosition, Position, RepositionMarker, ResizeMarker,
        SelectedVirtualMarker, SpawnWindowTrigger, Unmanaged, layout::LayoutStrip,
    },
    manager::{Application, Display, Window, WindowManager, bruteforce_windows},
    platform::PlatformCallbacks,
};

#[derive(Clone, Copy)]
enum ReloadReason {
    Automatic,
    Manual,
    Wake,
}

#[derive(Resource)]
pub(crate) struct Reloading {
    reason: ReloadReason,
    scanned: bool,
    finished: bool,
}

pub(crate) fn request_reload(commands: &mut Commands, manual: bool) {
    commands.insert_resource(Reloading {
        reason: if manual {
            ReloadReason::Manual
        } else {
            ReloadReason::Automatic
        },
        scanned: false,
        finished: false,
    });
}

pub(crate) fn request_wake_reload(commands: &mut Commands) {
    commands.insert_resource(Reloading {
        reason: ReloadReason::Wake,
        scanned: false,
        finished: false,
    });
}

pub(crate) fn register(app: &mut App) {
    app.add_systems(
        PreUpdate,
        scan.after(super::display::reconcile_displays)
            .run_if(resource_exists::<Reloading>)
            .in_set(super::sleep::LayoutActivity),
    );
    app.add_systems(
        Update,
        realign
            .after(super::triggers::apply_window_defaults)
            .before(super::triggers::apply_window_positions)
            .run_if(resource_exists::<Reloading>)
            .in_set(super::sleep::LayoutActivity),
    );
    app.add_systems(PostUpdate, finish.run_if(resource_exists::<Reloading>));
}

#[allow(clippy::too_many_arguments)]
fn scan(
    mut reload: ResMut<Reloading>,
    mut apps: Query<&mut Application>,
    windows: Query<&Window>,
    strips: Query<&LayoutStrip>,
    wm: Res<WindowManager>,
    config: Res<Config>,
    mut jobs: Query<(Entity, &mut BruteforceWindows)>,
    mut platform: Option<NonSendMut<Pin<Box<PlatformCallbacks>>>>,
    mut commands: Commands,
) {
    if reload.scanned {
        for (entity, mut job) in &mut jobs {
            if let Some(found) = future::block_on(future::poll_once(&mut job.0)) {
                commands.trigger(SpawnWindowTrigger(found));
                commands.entity(entity).despawn();
            }
        }
        return;
    }
    reload.scanned = true;
    if matches!(reload.reason, ReloadReason::Manual | ReloadReason::Wake)
        && let Some(platform) = platform.as_mut()
    {
        platform.rescan_processes();
        platform.ensure_input_tap_alive();
    }
    let spaces = strips
        .iter()
        .map(LayoutStrip::id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let known = windows
        .iter()
        .map(|window| window.id())
        .collect::<HashSet<_>>();
    for mut app in &mut apps {
        let _ = app
            .observe()
            .inspect_err(|error| warn!(%error, "re-observing application"));
        let Ok((mut found, mut missing)) = wm
            .find_existing_application_windows(&mut app, &spaces, &config)
            .inspect_err(|error| warn!(%error, "rescanning application windows"))
        else {
            continue;
        };
        found.retain(|window| !known.contains(&window.id()));
        missing.retain(|id| !known.contains(id));
        commands.trigger(SpawnWindowTrigger(found));
        if !missing.is_empty() {
            let pid = app.pid();
            let bundle = app.bundle_id();
            let config = config.clone();
            commands.spawn(BruteforceWindows(AsyncComputeTaskPool::get().spawn(
                async move { bruteforce_windows(pid, bundle.as_deref(), missing, &config) },
            )));
        }
    }
}

#[allow(clippy::too_many_lines)]
fn realign(world: &mut World) {
    if !world.resource::<Reloading>().scanned
        || world.resource::<Reloading>().finished
        || world
            .query::<&BruteforceWindows>()
            .iter(world)
            .next()
            .is_some()
    {
        return;
    }
    let preserve_layout = matches!(world.resource::<Reloading>().reason, ReloadReason::Wake);
    let spaces = world
        .query::<&LayoutStrip>()
        .iter(world)
        .map(LayoutStrip::id)
        .collect::<HashSet<_>>();
    let wm = world.resource::<WindowManager>();
    // A failed Space query is not proof that its windows disappeared.
    let ownership = spaces
        .into_iter()
        .filter_map(|space| {
            wm.windows_in_workspace(space)
                .inspect_err(|error| warn!(%error, space, "rescanning Space"))
                .ok()
                .map(|ids| (space, ids))
        })
        .collect::<HashMap<_, _>>();
    let windows = world
        .query::<(Entity, &mut Window, Option<&Unmanaged>)>()
        .iter_mut(world)
        .map(|(entity, mut window, unmanaged)| {
            // Refresh the cached AX frame so committing the desired ECS frame
            // cannot be skipped just because macOS moved it behind our back.
            let valid = window.update_frame().is_ok();
            (entity, (window.id(), unmanaged.is_none(), valid))
        })
        .collect::<HashMap<_, _>>();
    let strips = world
        .query::<(Entity, &LayoutStrip, Has<SelectedVirtualMarker>)>()
        .iter(world)
        .map(|(entity, strip, selected)| (entity, strip.id(), strip.virtual_index, selected))
        .collect::<Vec<_>>();
    for (entity, space, _, _) in &strips {
        let members = world
            .get::<LayoutStrip>(*entity)
            .map(LayoutStrip::all_windows)
            .unwrap_or_default();
        for member in members {
            let keep = windows.get(&member).is_some_and(|(id, managed, _)| {
                *managed
                    && (preserve_layout || ownership.get(space).is_none_or(|ids| ids.contains(id)))
            });
            if !keep && let Some(mut strip) = world.get_mut::<LayoutStrip>(*entity) {
                strip.remove(member);
            }
        }
    }
    for (entity, (id, managed, valid)) in &windows {
        if !valid {
            // AX errors can be transient; remove only confirmed dead windows.
            if !preserve_layout && world.resource::<WindowManager>().window_is_unordered(*id) {
                world.entity_mut(*entity).despawn();
            }
            continue;
        }
        if !managed
            || preserve_layout
                && strips.iter().any(|(strip, _, _, _)| {
                    world
                        .get::<LayoutStrip>(*strip)
                        .is_some_and(|strip| strip.contains(*entity))
                })
        {
            continue;
        }
        let Some(space) = ownership
            .iter()
            .find_map(|(space, ids)| ids.contains(id).then_some(*space))
        else {
            continue;
        };
        if strips.iter().any(|(strip, strip_space, _, _)| {
            *strip_space == space
                && world
                    .get::<LayoutStrip>(*strip)
                    .is_some_and(|strip| strip.contains(*entity))
        }) {
            continue;
        }
        if let Some((target, _, _, _)) = strips
            .iter()
            .filter(|(_, id, _, _)| *id == space)
            .min_by_key(|(_, _, index, selected)| (!selected, *index))
            && let Some(mut strip) = world.get_mut::<LayoutStrip>(*target)
        {
            strip.append(*entity);
        }
    }
    let bounds = world
        .query::<(Entity, &Display)>()
        .iter(world)
        .map(|(entity, display)| (entity, display.bounds()))
        .collect::<HashMap<_, _>>();
    let parents = world
        .query::<(Entity, &ChildOf)>()
        .iter(world)
        .filter_map(|(entity, child)| bounds.get(&child.parent()).map(|bounds| (entity, *bounds)))
        .collect::<HashMap<_, _>>();
    for entity in windows.keys() {
        if let Ok(mut entity) = world.get_entity_mut(*entity) {
            entity.remove::<(RepositionMarker, ResizeMarker)>();
            if let Some(mut position) = entity.get_mut::<Position>() {
                position.set_changed();
            }
            if let Some(mut bounds) = entity.get_mut::<Bounds>() {
                bounds.set_changed();
            }
            if let Some(mut layout) = entity.get_mut::<LayoutPosition>() {
                layout.set_changed();
            }
        }
    }
    super::display::refresh_display_layout(world, &parents);
    world.resource_mut::<Reloading>().finished = true;
    info!("Reloaded windows and displays, preserving existing layouts");
}

fn finish(mut commands: Commands, reload: Res<Reloading>) {
    if reload.finished {
        commands.remove_resource::<Reloading>();
    }
}
