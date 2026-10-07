use super::*;
use crate::{
    commands::{Command, Direction, Operation},
    config::{Config, MainOptions},
    ecs::{
        Bounds, FocusedMarker, LayoutPosition, Position, SelectedVirtualMarker, layout::LayoutStrip,
    },
    events::Event,
    manager::Window,
};
use bevy::prelude::*;
use std::{collections::HashMap, time::Duration};

fn two_displays() -> TestHarness {
    let config: Config = (
        MainOptions {
            supplementary_display: Some(EXT_DISPLAY_ID),
            ..Default::default()
        },
        vec![],
    )
        .into();
    TestHarness::new()
        .with_config(config)
        .with_windows(4)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(-800, 0, 0, 600),
            vec![EXT_WORKSPACE_ID],
        )
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(-750, 20, -350, 600);
        })
}

#[test]
fn wake_preserves_virtual_rows_and_column_order_when_native_space_is_replaced() {
    let mut harness = two_displays();
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::VirtualMove(
            Direction::South,
            crate::commands::MoveFocus::Follow,
        )),
    });
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    let rows = world
        .query::<(Entity, &LayoutStrip, &Position, Has<SelectedVirtualMarker>)>()
        .iter(world)
        .filter(|(_, strip, _, _)| strip.id() == TEST_WORKSPACE_ID)
        .map(|(entity, strip, position, selected)| {
            (entity, strip.all_windows(), position.0, selected)
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    let sizes = world
        .query::<(Entity, &Bounds)>()
        .iter(world)
        .map(|(entity, bounds)| (entity, bounds.0))
        .collect::<HashMap<_, _>>();
    let focused = world
        .query_filtered::<Entity, With<FocusedMarker>>()
        .single(world)
        .unwrap();
    harness.world().write_message(Event::SystemWillSleep);
    harness.advance(Duration::from_millis(50));
    // WindowServer publishes a replacement Space and moves/resizes every main window.
    harness.mock_state.add_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![99],
    );
    for id in 0..4 {
        harness.mock_state.update_window(id, |window| {
            window.workspace_id = 99;
            window.frame = IRect::new(-600, 30, -100, 450);
        });
        harness
            .world()
            .write_message(Event::WindowResized { window_id: id });
        harness
            .world()
            .write_message(Event::WindowMoved { window_id: id });
    }
    harness.world().write_message(Event::SpaceChanged);
    harness
        .world()
        .write_message(Event::WindowFocused { window_id: 100 });
    harness.advance(Duration::from_secs(10));
    for (entity, members, position, _) in &rows {
        assert_eq!(
            harness
                .world()
                .get::<LayoutStrip>(*entity)
                .unwrap()
                .all_windows(),
            *members
        );
        assert_eq!(
            harness.world().get::<Position>(*entity).unwrap().0,
            *position
        );
    }
    harness
        .world()
        .write_message(Event::SystemWoke { msg: String::new() });
    harness.advance(Duration::from_secs(3));
    let world = harness.world();
    for (entity, members, position, selected) in rows {
        let strip = world.get::<LayoutStrip>(entity).unwrap();
        assert_eq!(strip.id(), 99);
        assert_eq!(strip.all_windows(), members);
        assert_eq!(world.get::<Position>(entity).unwrap().0, position);
        assert_eq!(
            world.get::<SelectedVirtualMarker>(entity).is_some(),
            selected
        );
    }
    for (entity, size) in sizes {
        assert_eq!(world.get::<Bounds>(entity).unwrap().0, size);
    }
    assert!(world.get::<FocusedMarker>(focused).is_some());
    crate::assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
    assert_eq!(
        world
            .query::<&LayoutStrip>()
            .iter(world)
            .filter(|strip| strip.id() == 99)
            .count(),
        2
    );
}

#[test]
fn wake_waits_for_temporarily_missing_monitor_before_releasing_supplementary_role() {
    let mut harness = two_displays();
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    let laptop = find_window_entity(100, world);
    let frame = world.get::<Window>(laptop).unwrap().frame();
    harness.world().write_message(Event::SystemWillSleep);
    harness.advance(Duration::from_millis(50));
    harness.mock_state.remove_display(TEST_DISPLAY_ID);
    harness
        .world()
        .write_message(Event::SystemWoke { msg: String::new() });
    harness.world().write_message(Event::DisplayRemoved {
        display_id: TEST_DISPLAY_ID,
    });
    harness.advance(Duration::from_secs(2));
    assert!(
        harness
            .world()
            .contains_resource::<crate::ecs::sleep::WakeRecovery>()
    );
    assert!(
        harness
            .world()
            .get::<crate::ecs::supplementary::SupplementaryWindow>(laptop)
            .is_some()
    );
    harness.mock_state.add_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![TEST_WORKSPACE_ID],
    );
    harness.advance(Duration::from_secs(2));
    assert!(
        !harness
            .world()
            .contains_resource::<crate::ecs::sleep::WakeRecovery>()
    );
    assert_eq!(
        harness.world().get::<Window>(laptop).unwrap().frame(),
        frame
    );
    crate::assert_on_workspace!(harness.world(), 100, EXT_WORKSPACE_ID);
}

#[test]
fn wake_waits_for_window_lists_and_replays_frames_without_adopting_macos_ownership() {
    let mut harness = two_displays();
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    let entity = find_window_entity(0, world);
    let size = world.get::<Bounds>(entity).unwrap().0;
    let logical = world.get::<LayoutPosition>(entity).unwrap().0;
    let original = world.get::<Window>(entity).unwrap().frame();
    harness.world().write_message(Event::SystemWillSleep);
    harness.advance(Duration::from_millis(50));
    harness.mock_state.update_window(0, |window| {
        window.workspace_id = EXT_WORKSPACE_ID;
        window.frame = IRect::new(-750, 25, -250, 500);
    });
    harness
        .mock_state
        .set_space_unavailable(TEST_WORKSPACE_ID, true);
    harness
        .world()
        .write_message(Event::SystemWoke { msg: String::new() });
    harness.advance(Duration::from_secs(2));
    assert!(
        harness
            .world()
            .contains_resource::<crate::ecs::sleep::WakeRecovery>()
    );
    harness
        .mock_state
        .set_space_unavailable(TEST_WORKSPACE_ID, false);
    harness.advance(Duration::from_secs(2));
    crate::assert_on_workspace!(harness.world(), 0, TEST_WORKSPACE_ID);
    crate::assert_on_workspace!(harness.world(), 100, EXT_WORKSPACE_ID);
    assert_eq!(harness.world().get::<Bounds>(entity).unwrap().0, size);
    assert_eq!(
        harness.world().get::<LayoutPosition>(entity).unwrap().0,
        logical
    );
    assert_eq!(
        harness.world().get::<Window>(entity).unwrap().frame(),
        original
    );
}

#[test]
fn wake_does_not_remap_a_real_native_space_switch() {
    let mut harness = TestHarness::new().with_windows(2);
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::SystemWillSleep);
    harness.advance(Duration::from_millis(50));
    harness
        .mock_state
        .activate_workspace(TEST_DISPLAY_ID, 99, false);
    harness
        .world()
        .write_message(Event::SystemWoke { msg: String::new() });
    harness.advance(Duration::from_secs(2));
    crate::assert_on_workspace!(harness.world(), 0, TEST_WORKSPACE_ID);
    crate::assert_on_workspace!(harness.world(), 1, TEST_WORKSPACE_ID);
}

#[test]
fn wake_does_not_skip_settling_on_the_first_large_time_step() {
    let mut harness = two_displays();
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::SystemWillSleep);
    harness.advance(Duration::from_millis(50));
    harness
        .world()
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_secs(10),
        ));
    harness
        .world()
        .write_message(Event::SystemWoke { msg: String::new() });
    harness.app.update();
    assert!(
        harness
            .world()
            .contains_resource::<crate::ecs::sleep::WakeRecovery>()
    );
}

#[test]
fn wake_remaps_the_return_row_of_an_existing_native_fullscreen_window() {
    const FULLSCREEN_SPACE: WorkspaceId = TEST_WORKSPACE_ID + 100;
    let mut harness = TestHarness::new().with_windows(2);
    harness.advance(Duration::from_secs(1));
    harness.mock_state.update_window(0, |window| {
        window.workspace_id = FULLSCREEN_SPACE;
        window.is_full_screen = true;
    });
    harness
        .mock_state
        .activate_workspace(TEST_DISPLAY_ID, FULLSCREEN_SPACE, true);
    harness.world().write_message(Event::SpaceChanged);
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::SystemWillSleep);
    harness.advance(Duration::from_millis(50));
    harness.mock_state.add_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![99, FULLSCREEN_SPACE],
    );
    harness.mock_state.update_window(1, |window| {
        window.workspace_id = 99;
    });
    harness
        .world()
        .write_message(Event::SystemWoke { msg: String::new() });
    harness.advance(Duration::from_secs(2));
    let world = harness.world();
    let marker = world
        .query::<&crate::ecs::NativeFullscreenMarker>()
        .single(world)
        .expect("fullscreen return row");
    assert_eq!(marker.workspace_id, 99);
    crate::assert_on_workspace!(world, 1, 99);

    harness.mock_state.update_window(0, |window| {
        window.workspace_id = 99;
        window.is_full_screen = false;
    });
    harness
        .mock_state
        .activate_workspace(TEST_DISPLAY_ID, 99, false);
    harness.world().write_message(Event::SpaceDestroyed {
        space_id: FULLSCREEN_SPACE,
    });
    harness.advance(Duration::from_secs(1));
    crate::assert_on_workspace!(harness.world(), 0, 99);
    crate::assert_on_workspace!(harness.world(), 1, 99);
}
