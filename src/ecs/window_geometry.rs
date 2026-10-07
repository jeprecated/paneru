//! Physical window constraints and delayed Accessibility move feedback.
use bevy::prelude::*;

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

pub(crate) fn register(app: &mut App) {
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
