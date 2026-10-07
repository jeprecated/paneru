//! A scrolling main display with a single-window supplementary display.
use bevy::prelude::*;

use crate::{
    config::Config,
    ecs::{
        Bounds, DockPosition, FocusedMarker, Initializing, LayoutPosition, Position,
        RepositionMarker, ResizeMarker, SelectedVirtualMarker, SpawnCommandsExt, Unmanaged,
        layout::LayoutStrip, params::ActiveDisplay,
    },
    events::Event,
    manager::{Display, Size, Window, WindowManager},
    types::commands::Command,
};

#[derive(Component)]
pub(crate) struct SupplementaryDisplay;

#[derive(Component)]
struct FocusAfterTransfer;

#[derive(Component, Clone, Copy)]
pub(crate) struct SupplementaryWindow {
    pub size: Size,
    pub width_ratio: f64,
}

pub(crate) fn scrolling_display(
    active: ActiveDisplay,
    displays: Query<(), With<SupplementaryDisplay>>,
) -> bool {
    !displays.contains(active.entity())
}

pub(crate) fn register(app: &mut App) {
    app.add_systems(
        Update,
        reconcile
            .after(super::systems::finish_setup)
            .before(super::layout::layout_sizes_changed)
            .run_if(needs_reconcile)
            .run_if(bevy::ecs::schedule::common_conditions::not(
                bevy::ecs::schedule::common_conditions::resource_exists::<Initializing>,
            ))
            .in_set(super::sleep::LayoutActivity),
    );
    app.add_systems(PreUpdate, send_window.in_set(super::sleep::LayoutActivity));
    app.add_systems(
        PostUpdate,
        focus_after_transfer
            .after(super::systems::commit_window_position)
            .after(super::systems::commit_window_size)
            .in_set(super::sleep::LayoutActivity),
    );
    app.add_systems(
        PostUpdate,
        fit_window
            .after(super::systems::animate_entities)
            .after(super::systems::animate_resize_entities)
            .before(super::systems::commit_window_position)
            .before(super::systems::commit_window_size)
            .in_set(super::sleep::LayoutActivity),
    );
}

#[allow(clippy::type_complexity)]
fn needs_reconcile(
    config: Res<Config>,
    changed: Query<(), Or<(Changed<Display>, Changed<LayoutStrip>)>>,
    mut removed: RemovedComponents<Display>,
) -> bool {
    removed.read().next().is_some() || config.is_changed() || !changed.is_empty()
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::type_complexity
)]
fn reconcile(
    config: Res<Config>,
    displays: Query<(
        Entity,
        &Display,
        Option<&DockPosition>,
        Has<SupplementaryDisplay>,
    )>,
    mut strips: Query<(
        Entity,
        &mut LayoutStrip,
        &ChildOf,
        Has<SelectedVirtualMarker>,
    )>,
    mut windows: Query<(Entity, &mut Bounds, Option<&SupplementaryWindow>), With<Window>>,
    wm: Res<WindowManager>,
    focused: Query<Entity, With<FocusedMarker>>,
    mut commands: Commands,
) {
    let chosen = (displays.iter().count() > 1)
        .then(|| {
            let configured = config.options().supplementary_display;
            displays
                .iter()
                .find(|(_, display, _, _)| {
                    configured.map_or_else(|| display.is_built_in(), |id| display.id() == id)
                })
                .map(|(entity, _, _, _)| entity)
        })
        .flatten();
    for (entity, _, _, marked) in &displays {
        if Some(entity) == chosen && !marked {
            commands.entity(entity).insert(SupplementaryDisplay);
        }
        if Some(entity) != chosen && marked {
            commands.entity(entity).remove::<SupplementaryDisplay>();
        }
    }

    // Restore original scrolling dimensions when display roles or topology change.
    for (entity, mut bounds, saved) in &mut windows {
        let Some(saved) = saved else {
            continue;
        };
        let stays = strips
            .iter()
            .any(|(_, strip, child, _)| Some(child.parent()) == chosen && strip.contains(entity));
        if !stays {
            let owner = strips
                .iter()
                .find_map(|(_, strip, child, _)| strip.contains(entity).then_some(child.parent()));
            bounds.0 = owner.and_then(|owner| displays.get(owner).ok()).map_or(
                saved.size,
                |(_, display, dock, _)| {
                    let viewport = display.actual_display_bounds(dock, &config);
                    Size::new(
                        crate::util::round_px(saved.width_ratio * f64::from(viewport.width())),
                        viewport.height(),
                    )
                },
            );
            commands.entity(entity).remove::<SupplementaryWindow>();
        }
    }
    let Some(supplementary) = chosen else {
        return;
    };
    let Ok((_, display, dock, _)) = displays.get(supplementary) else {
        return;
    };
    let Ok(space) = wm.active_display_space(display.id()) else {
        return;
    };
    let Some((target, _, _, _)) = strips
        .iter()
        .filter(|(_, strip, child, _)| child.parent() == supplementary && strip.id() == space)
        .min_by_key(|(_, strip, _, selected)| (!selected, strip.virtual_index))
    else {
        return;
    };
    let Some((main_entity, main, _, _)) = displays
        .iter()
        .filter(|(entity, _, _, _)| *entity != supplementary)
        .max_by_key(|(_, display, _, _)| {
            i64::from(display.bounds().width()) * i64::from(display.bounds().height())
        })
    else {
        return;
    };
    let Ok(main_space) = wm.active_display_space(main.id()) else {
        return;
    };
    let Some((destination, _, _, _)) = strips
        .iter()
        .filter(|(_, strip, child, _)| child.parent() == main_entity && strip.id() == main_space)
        .min_by_key(|(_, strip, _, selected)| (!selected, strip.virtual_index))
    else {
        return;
    };
    let members = strips
        .iter()
        .filter(|(_, _, child, _)| child.parent() == supplementary)
        .flat_map(|(_, strip, _, _)| strip.all_windows())
        .collect::<Vec<_>>();
    let occupant = focused
        .iter()
        .find(|entity| members.contains(entity))
        .or_else(|| {
            strips
                .get(target)
                .ok()
                .and_then(|(_, strip, _, _)| strip.all_windows().first().copied())
        })
        .or_else(|| members.first().copied());
    for entity in members {
        if Some(entity) == occupant {
            continue;
        }
        for (_, mut strip, child, _) in &mut strips {
            if child.parent() == supplementary && strip.contains(entity) {
                strip.remove(entity);
            }
        }
        if let Ok((_, mut strip, _, _)) = strips.get_mut(destination) {
            strip.append(entity);
        }
        if let Ok((_, mut bounds, saved)) = windows.get_mut(entity) {
            let width = saved.map_or_else(
                || {
                    crate::util::round_px(
                        f64::from(bounds.0.x) / f64::from(display.width().max(1))
                            * f64::from(main.width()),
                    )
                },
                |saved| saved.size.x,
            );
            bounds.0 = Size::new(width, main.bounds().height());
        }
        commands.entity(entity).remove::<SupplementaryWindow>();
        super::workspace::spawn_snap_strip_guard(destination, &mut commands);
    }
    if let Some(entity) = occupant {
        for (strip_entity, mut strip, child, _) in &mut strips {
            if child.parent() == supplementary && strip_entity != target && strip.contains(entity) {
                strip.remove(entity);
            }
        }
        if let Ok((_, mut strip, _, _)) = strips.get_mut(target)
            && !strip.contains(entity)
        {
            strip.append(entity);
        }
        if let Ok((_, mut bounds, saved)) = windows.get_mut(entity) {
            if saved.is_none() {
                commands.entity(entity).insert(SupplementaryWindow {
                    size: bounds.0,
                    width_ratio: f64::from(bounds.0.x)
                        / f64::from(display.actual_display_bounds(dock, &config).width().max(1)),
                });
            }
            let size = display.actual_display_bounds(dock, &config).size();
            if bounds.0 != size {
                bounds.0 = size;
            }
        }
        super::workspace::spawn_snap_strip_guard(target, &mut commands);
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn send_window(
    mut messages: MessageReader<Event>,
    displays: Query<(Entity, &Display), With<SupplementaryDisplay>>,
    mut params: ParamSet<(
        ActiveDisplay,
        Query<(
            Entity,
            &mut LayoutStrip,
            &ChildOf,
            Has<SelectedVirtualMarker>,
        )>,
    )>,
    windows: Query<(&Bounds, Option<&Unmanaged>), With<Window>>,
    focused: Query<Entity, With<FocusedMarker>>,
    wm: Res<WindowManager>,
    time: Res<Time>,
    mut commands: Commands,
) {
    if !messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::DisplaySupplementary
            }
        )
    }) {
        return;
    }
    let Some((display_entity, display)) = displays.iter().next() else {
        return;
    };
    let Some(entity) = focused.iter().next() else {
        return;
    };
    let Ok((bounds, None)) = windows.get(entity) else {
        return;
    };
    let (source, neighbor, source_width) = {
        let active = params.p0();
        if active.entity() == display_entity || !active.active_strip().contains(entity) {
            return;
        }
        (
            active.active_strip_entity(),
            active
                .active_strip()
                .left_neighbour(entity)
                .or_else(|| active.active_strip().right_neighbour(entity)),
            active.bounds().width(),
        )
    };
    let mut strips = params.p1();
    let Ok(space) = wm.active_display_space(display.id()) else {
        return;
    };
    let Some(target) = strips
        .iter()
        .filter(|(_, strip, child, _)| child.parent() == display_entity && strip.id() == space)
        .min_by_key(|(_, strip, _, selected)| (!selected, strip.virtual_index))
        .map(|(entity, _, _, _)| entity)
    else {
        return;
    };
    let occupant = strips
        .get(target)
        .ok()
        .and_then(|(_, strip, _, _)| strip.all_windows().first().copied());
    if let Ok((_, mut strip, _, _)) = strips.get_mut(source) {
        if let Some(occupant) = occupant {
            strip.replace_window(entity, occupant);
        } else {
            strip.remove(entity);
        }
    }
    if let Ok((_, mut strip, _, _)) = strips.get_mut(target) {
        if let Some(occupant) = occupant {
            strip.remove(occupant);
        }
        strip.append(entity);
    }
    let until = time.elapsed() + std::time::Duration::from_millis(500);
    commands
        .entity(entity)
        .insert(super::window_geometry::RecentDisplayTransfer { until });
    if let Some(occupant) = occupant {
        commands
            .entity(occupant)
            .insert(super::window_geometry::RecentDisplayTransfer { until });
    }
    commands.entity(entity).insert(SupplementaryWindow {
        size: bounds.0,
        width_ratio: f64::from(bounds.0.x) / f64::from(source_width.max(1)),
    });
    if let Some(occupant) = occupant {
        commands.entity(occupant).remove::<SupplementaryWindow>();
        // Preserve the source slot's size so the rest of the main strip stays put.
        commands
            .entity(occupant)
            .insert((Bounds(bounds.0), ResizeMarker(bounds.0)));
    }
    super::workspace::spawn_snap_strip_guard(source, &mut commands);
    super::workspace::spawn_snap_strip_guard(target, &mut commands);
    let focus = occupant.or(neighbor).unwrap_or(entity);
    // Focus only after committing both physical frames. Raising the occupant
    // while it still lived on the laptop let macOS activate the old screen.
    commands.entity(focus).insert(FocusAfterTransfer);
}

fn focus_after_transfer(
    pending: Populated<Entity, With<FocusAfterTransfer>>,
    mut commands: Commands,
) {
    for entity in &pending {
        commands.entity(entity).remove::<FocusAfterTransfer>();
        super::workspace::spawn_restore_focus_guard(entity, &mut commands);
        commands.focus_entity(entity, true);
    }
}

#[allow(clippy::type_complexity)]
fn fit_window(
    windows: Populated<
        (
            Entity,
            &SupplementaryWindow,
            &mut Bounds,
            &mut Position,
            &mut LayoutPosition,
        ),
        Or<(
            Changed<Bounds>,
            Changed<Position>,
            Changed<LayoutPosition>,
            Added<SupplementaryWindow>,
        )>,
    >,
    strips: Query<&LayoutStrip>,
    displays: Query<(&Display, Option<&DockPosition>), With<SupplementaryDisplay>>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let Some((display, dock)) = displays.iter().next() else {
        return;
    };
    let viewport = display.actual_display_bounds(dock, &config);
    for (entity, _, mut bounds, mut position, mut layout) in windows {
        if !strips.iter().any(|strip| strip.contains(entity)) {
            continue;
        }
        if bounds.0 != viewport.size() {
            bounds.0 = viewport.size();
        }
        if position.0 != viewport.min {
            position.0 = viewport.min;
        }
        if layout.0 != Size::ZERO {
            layout.0 = Size::ZERO;
        }
        commands
            .entity(entity)
            .remove::<(RepositionMarker, ResizeMarker)>();
    }
}
