//! Selectable native-window presentation experiments at shared display edges.
use bevy::prelude::*;
use tracing::info;

use crate::{
    config::Config,
    ecs::{FocusedMarker, LayoutPosition, RepositionMarker, layout::LayoutStrip},
    events::Event,
    types::commands::{Command, DisplayEdgeMode},
};

/// A menu/command choice for this daemon session; config supplies the default.
#[derive(Resource)]
pub(crate) struct DisplayEdges(pub DisplayEdgeMode);

/// The physical frame is constrained while its logical slot keeps scrolling.
#[derive(Component)]
pub(crate) struct DisplayConstrainedMarker;

/// A recently requested physical origin, so delayed AX echoes cannot rewrite
/// the scrolling layout once its animation marker has been removed.
#[derive(Component)]
pub(crate) struct RecentWindowMove {
    pub origin: crate::manager::Origin,
    pub until: std::time::Duration,
}

pub(crate) fn mode(config: &Config, selection: Option<&DisplayEdges>) -> DisplayEdgeMode {
    selection.map_or_else(|| config.display_edge_mode(), |selection| selection.0)
}

pub(crate) fn register(app: &mut App) {
    app.add_systems(PreUpdate, select_mode);
    app.add_systems(
        Update,
        refresh_on_focus.before(super::layout::position_layout_windows),
    );
    app.add_systems(PostUpdate, expire_moves);
}

fn expire_moves(
    moves: Populated<(Entity, &RecentWindowMove)>,
    time: Res<Time>,
    mut commands: Commands,
) {
    for (entity, requested) in &moves {
        if time.elapsed() >= requested.until {
            commands.entity(entity).remove::<RecentWindowMove>();
        }
    }
}

fn select_mode(
    mut messages: MessageReader<Event>,
    strips: Query<(Entity, &LayoutStrip)>,
    mut windows: Query<&mut LayoutPosition>,
    mut commands: Commands,
) {
    let Some(selected) = messages
        .read()
        .filter_map(|event| match event {
            Event::Command {
                command: Command::DisplayEdges(mode),
            } => Some(*mode),
            _ => None,
        })
        .last()
    else {
        return;
    };
    commands.insert_resource(DisplayEdges(selected));
    for (strip_entity, strip) in &strips {
        super::workspace::spawn_snap_strip_guard(strip_entity, &mut commands);
        for entity in strip.all_windows() {
            if let Ok(mut layout) = windows.get_mut(entity) {
                layout.set_changed();
            }
            commands.entity(entity).remove::<RepositionMarker>();
        }
    }
    info!(mode = selected.token(), "Selected display-edge experiment");
}

fn refresh_on_focus(
    focused: Populated<Entity, Added<FocusedMarker>>,
    strips: Query<&LayoutStrip>,
    mut windows: Query<&mut LayoutPosition>,
) {
    // Parking depends on the last focused column on each display. Re-evaluate
    // even if focusing it did not move the strip or change any logical slot.
    for entity in &focused {
        if let Some(strip) = strips.iter().find(|strip| strip.contains(entity)) {
            for member in strip.all_windows() {
                if let Ok(mut layout) = windows.get_mut(member) {
                    layout.set_changed();
                }
            }
        }
    }
}
