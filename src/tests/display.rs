use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;

use crate::commands::{Command, Direction, MouseMove, MoveFocus, Operation};
use crate::config::{Config, MainOptions};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{DockPosition, Timeout};
use crate::events::Event;
use crate::manager::{Display, Origin, Size, Window};
use crate::platform::WinID;
use crate::{assert_not_on_workspace, assert_on_workspace, assert_window_at, assert_window_size};

use super::*;

#[test]
fn test_scrolling_centers_columns_without_crossing_shared_edge() {
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_windows(5)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(-1920, 0, 0, 1200),
            vec![EXT_WORKSPACE_ID],
        );
    harness.advance(Duration::from_secs(1));
    for id in [4, 3, 2, 1, 0, 1, 2, 3, 4] {
        harness.world().write_message(Event::Command {
            command: Command::Window(Operation::Focus(Direction::Nth(id))),
        });
        harness.advance(Duration::from_secs(1));
        let world = harness.world();
        let focused = find_window_entity(WinID::try_from(id).unwrap(), world);
        let frame = world.get::<Window>(focused).unwrap().frame();
        assert_eq!(frame.min.x, (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2);
        for window in world.query::<&Window>().iter(world) {
            assert!(window.frame().min.x >= 0);
            assert_eq!(window.frame().width(), TEST_WINDOW_WIDTH);
        }
    }
}

#[test]
fn test_resize_notification_for_move_does_not_resize_stack_neighbor() {
    use bevy::ecs::system::RunSystemOnce as _;
    let mut harness = TestHarness::new().with_windows(2);
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Focus(Direction::Last)),
    });
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Stack(true)),
    });
    harness.advance(Duration::from_secs(1));
    let state = harness.mock_state.clone();
    let world = harness.world();
    let above = find_window_entity(0, world);
    let below = find_window_entity(1, world);
    let original = world.get::<crate::ecs::Bounds>(above).unwrap().0;
    let origin = world.get::<crate::ecs::Position>(below).unwrap().0;
    state.os_move_window(1, Origin::new(origin.x + 20, origin.y + 40));
    world.write_message(Event::WindowResized { window_id: 1 });
    world
        .run_system_once(crate::ecs::systems::window_resized_update_frame)
        .unwrap();
    assert_eq!(world.get::<crate::ecs::Bounds>(above).unwrap().0, original);
}

#[test]
fn test_late_move_echo_cannot_rewrite_snapped_position() {
    use bevy::ecs::system::RunSystemOnce as _;
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_secs(1));
    let state = harness.mock_state.clone();
    let world = harness.world();
    let entity = find_window_entity(0, world);
    let origin = world.get::<crate::ecs::Position>(entity).unwrap().0;
    let until = world.resource::<Time>().elapsed() + Duration::from_millis(150);
    world
        .entity_mut(entity)
        .insert(crate::ecs::window_geometry::RecentWindowMove { origin, until });
    state.os_move_window(0, Origin::new(origin.x + 75, origin.y + 25));
    world.write_message(Event::WindowMoved { window_id: 0 });
    world
        .run_system_once(crate::ecs::systems::window_moved_update_frame)
        .unwrap();
    assert_eq!(world.get::<crate::ecs::Position>(entity).unwrap().0, origin);
}

#[test]
fn test_new_external_window_stays_on_its_display_without_stealing_focus() {
    TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(1024, 0, 2944, 1200),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(1)
        .on_iteration(1, |world, state| {
            let window = state.spawn_window(
                TEST_PROCESS_ID,
                EXT_WORKSPACE_ID,
                100,
                IRect::new(1100, 20, 1500, 1020),
            );
            world.trigger(crate::ecs::SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(2, |world, _| {
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 100, TEST_WORKSPACE_ID);
            crate::assert_focused!(world, 0);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn test_focusing_external_window_updates_display_before_notification() {
    TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(1024, 0, 2944, 1200),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(1)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(1100, 20, 1500, 1020);
        })
        .on_iteration(1, |world, state| {
            state.set_focused_window(100);
            world.write_message(Event::WindowFocused { window_id: 100 });
        })
        .on_iteration(2, |world, _| {
            assert_eq!(
                world
                    .query_filtered::<&Display, With<crate::ecs::ActiveDisplayMarker>>()
                    .single(world)
                    .unwrap()
                    .id(),
                EXT_DISPLAY_ID
            );
            crate::assert_focused!(world, 100);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn test_external_window_width_uses_its_own_display() {
    let config = Config::try_from("[windows.default]\ntitle = '.*'\nwidth = 0.5\n").unwrap();
    TestHarness::new()
        .with_config(config)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(1024, 0, 2944, 1200),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(1)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(1100, 20, 1500, 1020);
        })
        .on_iteration(1, |world, _| {
            assert_window_size!(world, 100, 960, 1180);
            let (window, ratio) = world
                .query::<(&Window, &crate::ecs::WidthRatio)>()
                .iter(world)
                .find(|(window, _)| window.id() == 100)
                .unwrap();
            assert!(
                (ratio.0 - 0.5).abs() < 0.001,
                "{} has ratio {}",
                window.id(),
                ratio.0
            );
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn test_horizontal_neighbor_never_receives_scrolled_windows() {
    TestHarness::new()
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(1024, 0, 2944, 1200),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(6)
        .on_iteration(1, |world, _| {
            let neighbor = IRect::new(1024, 0, 2944, 1200);
            for window in world.query::<&Window>().iter(world) {
                let overlap = window.frame().intersect(neighbor);
                assert!(
                    overlap.width() == 0 || overlap.height() == 0,
                    "window {} overlaps neighbor: {:?}",
                    window.id(),
                    window.frame()
                );
            }
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn test_multi_display_lifecycle() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
        Event::DisplayAdded {
            display_id: TEST_DISPLAY_ID,
        },
    ];

    let mut harness = TestHarness::new().with_windows(1).with_display(
        EXT_DISPLAY_ID,
        IRect::new(1024, 0, 2944, 1200),
        vec![EXT_WORKSPACE_ID],
    );
    harness
        .app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            500,
        )));

    harness
        .on_iteration(1, |world, state| {
            let mut query = world.query_filtered::<Entity, With<Display>>();
            assert_eq!(query.iter(world).count(), 2);
            state.remove_display(TEST_DISPLAY_ID);
        })
        .on_iteration(2, |world, mut state| {
            assert!(
                world
                    .query::<&Display>()
                    .iter(world)
                    .all(|display| display.id() != TEST_DISPLAY_ID),
                "display should be despawned"
            );

            let workspace_entity = {
                world
                    .query::<(Entity, &LayoutStrip)>()
                    .iter(world)
                    .find(|(_, strip)| strip.id() == TEST_WORKSPACE_ID)
                    .unwrap()
                    .0
            };
            let workspace = world.entity(workspace_entity);
            assert!(
                workspace.get::<Timeout>().is_some(),
                "orphaned workspace should have a timeout"
            );
            assert!(
                workspace.get::<ChildOf>().is_none(),
                "orphaned workspace should have no parent"
            );
            state.add_display(
                TEST_DISPLAY_ID,
                IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
                vec![TEST_WORKSPACE_ID],
            );
        })
        .on_iteration(3, |world, _state| {
            let new_display_entity = world
                .query::<(Entity, &Display)>()
                .iter(world)
                .find(|(_, display)| display.id() == TEST_DISPLAY_ID)
                .expect("display should be spawned again")
                .0;

            let workspace_entity = {
                world
                    .query::<(Entity, &LayoutStrip)>()
                    .iter(world)
                    .find(|(_, strip)| strip.id() == TEST_WORKSPACE_ID)
                    .unwrap()
                    .0
            };
            let workspace = world.entity(workspace_entity);
            assert!(
                workspace.get::<Timeout>().is_none(),
                "re-parented workspace should no longer have a timeout"
            );
            let child_of: &ChildOf = workspace
                .get::<ChildOf>()
                .expect("re-parented workspace should have a parent");
            assert_eq!(
                child_of.parent(),
                new_display_entity,
                "workspace should be child of the new display"
            );
        })
        .run(commands);
}

#[test]
fn test_multi_workspace_orphaning() {
    let mut commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
    ];
    commands.extend((0..6).map(|_| Event::Command {
        command: Command::PrintState,
    }));

    let workspaces = vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1];
    let harness = TestHarness::new()
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            workspaces,
        )
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(1024, 0, 2944, 1200),
            vec![EXT_WORKSPACE_ID],
        );
    harness
        .on_iteration(1, |world, state| {
            let display_entity = world
                .query::<(Entity, &Display)>()
                .iter(world)
                .find(|(_, display)| display.id() == TEST_DISPLAY_ID)
                .unwrap()
                .0;

            let workspace_entities = world
                .query::<(Entity, &LayoutStrip)>()
                .iter(world)
                .filter_map(|(entity, strip)| (strip.id() < EXT_WORKSPACE_ID).then_some(entity))
                .collect::<Vec<_>>();
            assert_eq!(workspace_entities.len(), 2, "should have two workspaces");

            for &ws in &workspace_entities {
                let child_of: &ChildOf = world
                    .entity(ws)
                    .get::<ChildOf>()
                    .expect("workspace should have parent");
                assert_eq!(child_of.parent(), display_entity);
            }
            state.remove_display(TEST_DISPLAY_ID);
        })
        .on_iteration(8, |world, _state| {
            let workspace_entities = world
                .query::<(Entity, &LayoutStrip)>()
                .iter(world)
                .filter_map(|(entity, strip)| (strip.id() < EXT_WORKSPACE_ID).then_some(entity))
                .collect::<Vec<_>>();
            for &ws in &workspace_entities {
                let entity: EntityRef = world.entity(ws);
                assert!(
                    entity.get::<Timeout>().is_some(),
                    "each workspace should have a timeout"
                );
                assert!(
                    entity.get::<ChildOf>().is_none(),
                    "each workspace should have no parent"
                );
            }
        })
        .run(commands);
}

#[test]
fn test_multi_display_no_height_crosstalk() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let origin = Origin::new(0, 0);
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);
    let ext_frame = IRect::from_corners(ext_origin, ext_origin + size);

    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, ext_frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 200, frame);

    let ext_usable_height = EXT_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayChanged,
        Event::MenuOpened { window_id: 100 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world, _state| {
            assert_window_size!(world, 100, TEST_WINDOW_WIDTH, ext_usable_height);
        })
        .on_iteration(2, |world, _state| {
            use crate::ecs::ActiveWorkspaceMarker;
            let mut strip_query =
                world.query_filtered::<&mut LayoutStrip, Without<ActiveWorkspaceMarker>>();
            for mut strip in strip_query.iter_mut(world) {
                strip.set_changed();
            }
        })
        .on_iteration(4, move |world, _state| {
            assert_window_size!(world, 100, TEST_WINDOW_WIDTH, ext_usable_height);
        })
        .run(commands);
}

#[test]
fn test_next_display_inserts_into_target_strip() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToNextDisplay(MoveFocus::Follow)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .on_iteration(1, move |world, _state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
        })
        .on_iteration(2, move |world, _state| {
            assert_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, TEST_WORKSPACE_ID);
        })
        .run(commands);
}

#[test]
fn test_send_next_display_stays_on_source() {
    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let origin = Origin::new(0, 0);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);

    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 101, frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 100, frame);

    let commands = vec![
        Event::MenuOpened { window_id: 101 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToNextDisplay(MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(1, move |world, _state| {
            assert_on_workspace!(world, 100, TEST_WORKSPACE_ID);
        })
        .on_iteration(2, move |world, state| {
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 100, TEST_WORKSPACE_ID);
            assert_eq!(state.active_display(), TEST_DISPLAY_ID);
        })
        .run(commands);
}

#[test]
fn test_mouse_to_next_display() {
    let commands = vec![
        Event::MenuOpened { window_id: 101 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Mouse(MouseMove::ToNextDisplay),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];
    let origin = Origin::new(0, 0);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);
    let display_bounds = IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0);

    // harness
    //     .mock_state
    //     .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 101, frame);
    // harness
    //     .mock_state
    //     .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 100, frame);
    TestHarness::new()
        .with_display(EXT_DISPLAY_ID, display_bounds, vec![EXT_WORKSPACE_ID])
        .with_window(100, |data| {
            data.pid = TEST_PROCESS_ID;
            data.workspace_id = TEST_WORKSPACE_ID;
            data.frame = frame;
        })
        .on_iteration(1, move |world, state| {
            let entity = find_window_entity(100, world);
            let window = world.get::<Window>(entity).expect("need window");
            assert_eq!(state.cursor_position(), window.frame().center());
        })
        .on_iteration(3, move |world, state| {
            let mut query = world.query::<(&Display, Option<&DockPosition>)>();
            let (display, dock) = query
                .iter(world)
                .find(|display| display.0.id() == EXT_DISPLAY_ID)
                .expect("need display");
            let config = world.resource::<Config>();
            let bounds = display.actual_display_bounds(dock, config);
            assert_eq!(state.cursor_position(), bounds.center());
        })
        .run(commands);
}

/// Regression test: paneru's init pass must not drag windows that live on
/// inactive displays onto the active display. `apply_window_properties`
/// initially appends every observed window to the active strip; if the
/// layout writers run before `finish_setup` has reassigned them, they
/// cache active-display coordinates into `Position` and `commit_window_position`
/// later pushes those to macOS, moving the windows.
#[test]
fn test_init_keeps_windows_on_their_real_displays() {
    // Internal (test) display is active. Window 100 lives on the external
    // display's space, window 200 lives on the active display's space.

    let mut harness = TestHarness::new();
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    let origin = Origin::new(0, 0);
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let frame = IRect::from_corners(origin, origin + size);
    let ext_frame = IRect::from_corners(ext_origin, ext_origin + size);

    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 200, ext_frame);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, frame);

    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(0, move |world, _state| {
            assert_on_workspace!(world, 100, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 100, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 200, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 200, EXT_WORKSPACE_ID);
            // The OS frame for window 100 must stay within the external
            // display's vertical bounds (negative y); if init moved it
            // onto the active display the frame would land at y >= 0.
            assert_window_at!(world, 100, ext_origin.x, ext_origin.y);
        })
        .run(commands);
}

/// Waking from sleep (or a resolution/configuration change) with a monitor
/// gone should reconcile the ECS display set against the OS even though no
/// per-display `DisplayRemoved` flag arrives: the vanished display is removed
/// and its workspace is orphaned.
#[test]
fn test_wake_reconciles_unplugged_display() {
    let mut harness = TestHarness::new().with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );

    // A window on the external display so its workspace strip actually exists.
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    let ext_frame = IRect::from_corners(ext_origin, ext_origin + size);
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, ext_frame);

    harness.advance(Duration::from_secs(1));
    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness
        .world()
        .write_message(Event::SystemWoke { msg: String::new() });
    harness.advance(Duration::from_secs(5));
    let world = harness.world();
    assert_eq!(world.query::<&Display>().iter(world).count(), 1);
    let (_, parent) = world
        .query::<(&LayoutStrip, Option<&ChildOf>)>()
        .iter(world)
        .find(|(strip, _)| strip.id() == EXT_WORKSPACE_ID)
        .unwrap();
    assert!(
        parent.is_none(),
        "removed display's rows should remain available for reconnection"
    );
}

#[test]
fn test_display_configuration_reapplies_layout_after_geometry_change() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_millis(500));
    let window_entity = find_window_entity(0, harness.world());
    let original = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();

    let new_height = TEST_DISPLAY_HEIGHT - 100;
    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(120, 0, TEST_DISPLAY_WIDTH + 120, new_height),
    );
    harness
        .world()
        .write_message::<Event>(Event::DisplayConfigured {
            display_id: TEST_DISPLAY_ID,
        });

    harness.advance(Duration::from_millis(200));
    let world = harness.world();
    let display = world.query::<&Display>().single(world).unwrap();
    assert_eq!(display.bounds().min.x, 0, "wait for the display to settle");

    harness.advance(Duration::from_secs(1));
    let updated = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();
    assert_eq!(updated.min.x, original.min.x + 120);
    assert_eq!(updated.height(), new_height - TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_display_configuration_catches_late_geometry_change() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_millis(500));
    let window_entity = find_window_entity(0, harness.world());
    let original_x = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame()
        .min
        .x;
    harness
        .world()
        .write_message::<Event>(Event::DisplayConfigured {
            display_id: TEST_DISPLAY_ID,
        });
    // The first settled scan still sees the old display configuration.
    harness.advance(Duration::from_millis(600));

    harness.mock_state.set_display_bounds(
        TEST_DISPLAY_ID,
        IRect::new(90, 0, TEST_DISPLAY_WIDTH + 90, TEST_DISPLAY_HEIGHT),
    );
    harness.advance(Duration::from_millis(1300));

    let world = harness.world();
    let display = world.query::<&Display>().single(world).unwrap();
    assert_eq!(display.bounds().min.x, 90);
    assert_eq!(
        world.get::<Window>(window_entity).unwrap().frame().min.x,
        original_x + 90
    );
}

#[test]
fn test_transient_empty_display_list_keeps_layout_and_retries() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_millis(500));
    harness.mock_state.remove_display(TEST_DISPLAY_ID);
    harness
        .world()
        .write_message::<Event>(Event::DisplayConfigured {
            display_id: TEST_DISPLAY_ID,
        });
    harness.advance(Duration::from_millis(600));

    let world = harness.world();
    let display_entity = world
        .query_filtered::<Entity, With<Display>>()
        .single(world)
        .unwrap();
    let strip_entity = world
        .query_filtered::<Entity, With<LayoutStrip>>()
        .single(world)
        .unwrap();
    assert_eq!(
        world.get::<ChildOf>(strip_entity).unwrap().parent(),
        display_entity
    );

    harness.mock_state.add_display(
        TEST_DISPLAY_ID,
        IRect::new(100, 0, TEST_DISPLAY_WIDTH + 100, TEST_DISPLAY_HEIGHT),
        vec![TEST_WORKSPACE_ID],
    );
    harness.advance(Duration::from_millis(1600));
    let world = harness.world();
    let display = world.query::<&Display>().single(world).unwrap();
    assert_eq!(
        display.bounds().min.x,
        100,
        "retry should use the recovered display"
    );
}

#[test]
fn test_unplug_rehomes_workspace_and_window_on_surviving_display() {
    let mut harness = TestHarness::new().with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    let frame = IRect::from_corners(
        ext_origin,
        ext_origin + Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
    );
    harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, EXT_WORKSPACE_ID, 100, frame);
    harness.advance(Duration::from_millis(500));

    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness.mock_state.add_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![TEST_WORKSPACE_ID, EXT_WORKSPACE_ID],
    );
    harness
        .world()
        .write_message::<Event>(Event::DisplayRemoved {
            display_id: EXT_DISPLAY_ID,
        });
    harness.advance(Duration::from_secs(1));

    let world = harness.world();
    let main = world.query::<(&Display, Entity)>().single(world).unwrap().1;
    let external_parent = world
        .query::<(&LayoutStrip, &ChildOf)>()
        .iter(world)
        .find(|(strip, _)| strip.id() == EXT_WORKSPACE_ID)
        .map(|(_, child)| child.parent())
        .expect("external workspace should remain managed");
    assert_eq!(external_parent, main);
    let window_entity = find_window_entity(100, world);
    let window = world.get::<Window>(window_entity).unwrap();
    assert!(window.frame().min.y >= TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_late_space_rehome_reapplies_window_frame() {
    let mut harness = TestHarness::new().with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );
    let ext_origin = Origin::new(0, -EXT_DISPLAY_HEIGHT + TEST_MENUBAR_HEIGHT);
    harness.mock_state.spawn_window(
        TEST_PROCESS_ID,
        EXT_WORKSPACE_ID,
        100,
        IRect::from_corners(
            ext_origin,
            ext_origin + Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
        ),
    );
    harness.advance(Duration::from_millis(500));
    harness.mock_state.remove_display(EXT_DISPLAY_ID);
    harness
        .world()
        .write_message::<Event>(Event::DisplayRemoved {
            display_id: EXT_DISPLAY_ID,
        });
    harness.advance(Duration::from_millis(650));

    let world = harness.world();
    let orphan = world
        .query::<(&LayoutStrip, Option<&ChildOf>)>()
        .iter(world)
        .find(|(strip, _)| strip.id() == EXT_WORKSPACE_ID)
        .expect("external workspace should be retained");
    assert!(orphan.1.is_none(), "workspace should await its new display");

    harness.mock_state.add_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![TEST_WORKSPACE_ID, EXT_WORKSPACE_ID],
    );
    harness.advance(Duration::from_millis(1200));

    let world = harness.world();
    let external_parent = world
        .query::<(&LayoutStrip, &ChildOf)>()
        .iter(world)
        .find(|(strip, _)| strip.id() == EXT_WORKSPACE_ID)
        .map(|(_, child)| child.parent())
        .expect("external workspace should be reparented");
    let main = world.query::<(&Display, Entity)>().single(world).unwrap().1;
    assert_eq!(external_parent, main);
    let window_entity = find_window_entity(100, world);
    assert!(world.get::<Window>(window_entity).unwrap().frame().min.y >= TEST_MENUBAR_HEIGHT);
}

#[test]
fn test_wake_reapplies_window_frame_without_geometry_change() {
    let mut harness = TestHarness::new().with_windows(1);
    harness.advance(Duration::from_millis(500));
    let window_entity = find_window_entity(0, harness.world());
    let original = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();

    harness.mock_state.update_window(0, |window| {
        window.frame.min.x += 200;
        window.frame.max.x += 200;
    });
    harness
        .world()
        .write_message::<Event>(Event::SystemWoke { msg: String::new() });
    harness.advance(Duration::from_secs(2));

    let restored = harness
        .world()
        .get::<Window>(window_entity)
        .unwrap()
        .frame();
    assert_eq!(restored, original);
}

#[test]
fn test_vertical_swap_within_stack_stays_on_display() {
    // Regression test: with a display arranged *below* the active one, a
    // `Swap(South)` inside a stack used to swap the two windows and then
    // immediately send the focused one to the display below, because the
    // "is there anything left to swap with?" check ran against the layout
    // after the swap had already happened.
    let mut harness = TestHarness::new().with_windows(2);
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(
            0,
            TEST_DISPLAY_HEIGHT,
            EXT_DISPLAY_WIDTH,
            TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
        ),
        vec![EXT_WORKSPACE_ID],
    );

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::North)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::South)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(4, |world, state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_eq!(state.active_display(), TEST_DISPLAY_ID);
        })
        .on_iteration(6, |world, state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 1, EXT_WORKSPACE_ID);
            assert_eq!(
                state.active_display(),
                TEST_DISPLAY_ID,
                "swapping inside a stack must not move focus to another display"
            );
        })
        .run(commands);
}

#[test]
fn test_hidden_stack_stays_off_the_display_below() {
    // Regression test: hiding a virtual workspace parks its strip at the
    // display's bottom-right corner. Windows below the strip origin - the
    // lower members of a stack - used to land past the bottom edge entirely,
    // inside the display underneath, which macOS then adopts them onto. The
    // window came back on the wrong display once the workspace was shown
    // again.
    let mut harness = TestHarness::new().with_windows(2);
    harness.mock_state.add_display(
        EXT_DISPLAY_ID,
        IRect::new(
            0,
            TEST_DISPLAY_HEIGHT,
            EXT_DISPLAY_WIDTH,
            TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
        ),
        vec![EXT_WORKSPACE_ID],
    );

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    harness
        .on_iteration(4, |world, _state| {
            // Hidden, but still parked on their own display: every window keeps
            // its origin above the top edge of the display below.
            let mut query = world.query::<&crate::manager::Window>();
            for window in query.iter(world) {
                let frame = window.frame();
                assert!(
                    frame.min.y < TEST_DISPLAY_HEIGHT,
                    "window {} parked at {:?}, inside the display below",
                    window.id(),
                    frame
                );
                let neighbor = IRect::new(
                    0,
                    TEST_DISPLAY_HEIGHT,
                    EXT_DISPLAY_WIDTH,
                    TEST_DISPLAY_HEIGHT + EXT_DISPLAY_HEIGHT,
                );
                let overlap = frame.intersect(neighbor);
                assert!(
                    overlap.width() == 0 || overlap.height() == 0,
                    "window {} should avoid the display below",
                    window.id()
                );
            }
        })
        .on_iteration(6, |world, _state| {
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, EXT_WORKSPACE_ID);
            assert_not_on_workspace!(world, 1, EXT_WORKSPACE_ID);
            assert_window_at!(world, 0, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, 394);
        })
        .run(commands);
}

/// Focusing a window on another display and coming back must not re-derive the
/// strip offset on the display we left: the centering the user asked for there
/// is still what they want to see when they return.
#[test]
fn test_center_survives_display_round_trip() {
    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            continuous_swipe: Some(false),
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
    let window_x = |world: &mut World, id: WinID| -> i32 {
        let mut query = world.query::<&Window>();
        query
            .iter(world)
            .find(|window| window.id() == id)
            .expect("window not found")
            .frame()
            .min
            .x
    };

    let commands = vec![
        // 0: boot with focus on window 0.
        Event::MenuOpened { window_id: 0 },
        // 1: center it on the main display.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: focus moves to the window on the external display.
        Event::Command {
            command: Command::PrintState,
        },
        // 3: and back to window 0.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_config(config)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
            vec![EXT_WORKSPACE_ID],
        )
        .with_windows(4)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.workspace_id = EXT_WORKSPACE_ID;
        })
        .on_iteration(1, move |world, state| {
            assert_eq!(window_x(world, 0), centered, "window 0 must be centered");
            state.focus_window(100);
        })
        .on_iteration(2, move |_world, state| {
            state.focus_window(0);
        })
        .on_iteration(3, move |world, _state| {
            assert_eq!(
                window_x(world, 0),
                centered,
                "returning from another display must not undo the centering"
            );
        })
        .run(commands);
}

/// An empty row 0 must survive its display going away. Despawning it left the
/// space renumbered from "2" — the menu bar lists only the rows that exist —
/// with no switch or reap path that recreates row 0.
#[test]
fn test_empty_baseline_row_survives_display_removal() {
    let mut commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::DisplayRemoved {
            display_id: TEST_DISPLAY_ID,
        },
    ];
    commands.extend((0..6).map(|_| Event::Command {
        command: Command::PrintState,
    }));
    commands.push(Event::DisplayAdded {
        display_id: TEST_DISPLAY_ID,
    });

    TestHarness::new()
        .on_iteration(0, |world, state| {
            let strips = world
                .query::<&LayoutStrip>()
                .iter(world)
                .map(|strip| strip.virtual_index)
                .collect::<Vec<_>>();
            assert_eq!(strips, vec![0], "the space starts with an empty row 0");
            state.remove_display(TEST_DISPLAY_ID);
        })
        .on_iteration(7, |world, mut state| {
            let entity = world
                .query_filtered::<Entity, With<LayoutStrip>>()
                .single(world)
                .expect("empty row 0 should be orphaned, not despawned");
            assert!(
                world.entity(entity).get::<ChildOf>().is_some(),
                "an empty display scan must retain the baseline and its display"
            );
            state.add_display(
                TEST_DISPLAY_ID,
                IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
                vec![TEST_WORKSPACE_ID],
            );
        })
        .on_iteration(8, |world, _state| {
            let entity = world
                .query_filtered::<Entity, With<LayoutStrip>>()
                .single(world)
                .expect("row 0 should still exist after the display returns");
            assert!(
                world.entity(entity).get::<ChildOf>().is_some(),
                "row 0 should be re-parented to the returning display"
            );
        })
        .run(commands);
}

#[test]
fn test_reload_prunes_stale_layout_entries() {
    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |world, _| {
            let dead = world.spawn_empty().id();
            world
                .query::<&mut LayoutStrip>()
                .single_mut(world)
                .unwrap()
                .append(dead);
            world.entity_mut(dead).despawn();
        })
        .on_iteration(2, |world, _| {
            let members = world
                .query::<&LayoutStrip>()
                .single(world)
                .unwrap()
                .all_windows();
            assert_eq!(members.len(), 1);
            assert!(world.get::<Window>(members[0]).is_some());
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::Reload,
            },
        ]);
}

#[test]
fn test_reload_discovers_missed_window_and_replays_frames() {
    TestHarness::new()
        .with_windows(2)
        .on_iteration(1, |world, state| {
            let original = world
                .query::<(Entity, &Window)>()
                .iter(world)
                .find(|(_, window)| window.id() == 0)
                .unwrap()
                .0;
            world
                .entity_mut(original)
                .insert(crate::ecs::Unmanaged::Floating);
            state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                100,
                IRect::new(50, 50, 450, 1050),
            );
            state.update_window(1, |window| window.frame = IRect::new(75, 75, 475, 1075));
        })
        .on_iteration(2, |world, _| {
            assert_on_workspace!(world, 1, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 100, TEST_WORKSPACE_ID);
            assert_not_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            let window = world
                .query::<&Window>()
                .iter(world)
                .find(|window| window.id() == 1)
                .unwrap();
            assert_eq!(window.frame().min.y, TEST_MENUBAR_HEIGHT);
        })
        .run(vec![
            Event::MenuOpened { window_id: 1 },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::Reload,
            },
        ]);
}

#[test]
fn test_reload_preserves_virtual_workspace_membership() {
    TestHarness::new()
        .with_windows(3)
        .on_iteration(1, |world, _| {
            let entity = world
                .query::<(Entity, &Window)>()
                .iter(world)
                .find(|(_, window)| window.id() == 0)
                .unwrap()
                .0;
            assert!(
                world
                    .query::<&LayoutStrip>()
                    .iter(world)
                    .any(|strip| strip.virtual_index == 1 && strip.contains(entity))
            );
        })
        .on_iteration(2, |world, _| {
            let entity = world
                .query::<(Entity, &Window)>()
                .iter(world)
                .find(|(_, window)| window.id() == 0)
                .unwrap()
                .0;
            assert!(
                world
                    .query::<&LayoutStrip>()
                    .iter(world)
                    .any(|strip| strip.virtual_index == 1 && strip.contains(entity))
            );
            assert_eq!(
                world
                    .query_filtered::<&LayoutStrip, With<crate::ecs::ActiveWorkspaceMarker>>()
                    .single(world)
                    .unwrap()
                    .virtual_index,
                1
            );
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::Window(Operation::VirtualMove(
                    Direction::South,
                    MoveFocus::Follow,
                )),
            },
            Event::Command {
                command: Command::Reload,
            },
        ]);
}

#[test]
fn test_supplementary_swaps_one_window_without_moving_main_strip() {
    let config: Config = (
        MainOptions {
            supplementary_display: Some(EXT_DISPLAY_ID),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(-800, 0, 0, 600),
            vec![EXT_WORKSPACE_ID],
        )
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(-750, 20, -350, 600);
        });
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Focus(Direction::Nth(1))),
    });
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    let sender = find_window_entity(1, world);
    let old = find_window_entity(100, world);
    let size = world.get::<crate::ecs::Bounds>(sender).unwrap().0;
    let offset = world
        .query::<(&LayoutStrip, &crate::ecs::Position)>()
        .iter(world)
        .find(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
        .unwrap()
        .1
        .0;
    harness.world().write_message(Event::Command {
        command: Command::DisplaySupplementary,
    });
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    assert_on_workspace!(world, 1, EXT_WORKSPACE_ID);
    assert_on_workspace!(world, 100, TEST_WORKSPACE_ID);
    crate::assert_focused!(world, 100);
    let expected = vec![
        find_window_entity(0, world),
        old,
        find_window_entity(2, world),
    ];
    let strip = world
        .query::<(&LayoutStrip, &crate::ecs::Position)>()
        .iter(world)
        .find(|(strip, _)| strip.id() == TEST_WORKSPACE_ID)
        .unwrap();
    assert_eq!(strip.0.all_windows(), expected);
    assert_eq!(strip.1.0, offset);
    assert_eq!(world.get::<crate::ecs::Bounds>(old).unwrap().0, size);
    let frame = world.get::<Window>(sender).unwrap().frame();
    assert_eq!(frame, IRect::new(-800, TEST_MENUBAR_HEIGHT, 0, 600));
    // The mock changes frames without WindowServer's automatic Space reassignment.
    harness
        .mock_state
        .update_window(1, |window| window.workspace_id = EXT_WORKSPACE_ID);
    harness
        .mock_state
        .update_window(100, |window| window.workspace_id = TEST_WORKSPACE_ID);
    // Focus the new laptop occupant through the ordinary AX event path;
    // the screen-focus command is introduced in the next feature chunk.
    harness.mock_state.set_focused_window(1);
    harness
        .world()
        .write_message(Event::WindowFocused { window_id: 1 });
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Virtual(Direction::South)),
    });
    harness.advance(Duration::from_secs(1));
    assert_on_workspace!(harness.world(), 1, EXT_WORKSPACE_ID);
    assert_eq!(
        harness.world().get::<Window>(sender).unwrap().frame(),
        frame
    );
    harness.world().write_message(Event::Command {
        command: Command::Reload,
    });
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    assert!(
        world
            .get::<crate::ecs::supplementary::SupplementaryWindow>(sender)
            .is_some()
    );
    assert_eq!(world.get::<Window>(sender).unwrap().frame(), frame);
    let strip = world
        .query::<&LayoutStrip>()
        .iter(world)
        .find(|strip| strip.id() == TEST_WORKSPACE_ID)
        .unwrap();
    assert_eq!(strip.all_windows(), expected);
}

#[test]
fn test_supplementary_swap_ignores_stale_space_ownership_during_screen_switch() {
    let config: Config = (
        MainOptions {
            supplementary_display: Some(EXT_DISPLAY_ID),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(-800, 0, 0, 600),
            vec![EXT_WORKSPACE_ID],
        )
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(-750, 20, -350, 600);
        });
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::Command {
        command: Command::DisplaySupplementary,
    });
    harness.advance(Duration::from_millis(50));
    harness.mock_state.set_focused_window(0);
    harness
        .world()
        .write_message(Event::WindowFocused { window_id: 0 });
    harness.advance(Duration::from_millis(200));
    assert_on_workspace!(harness.world(), 0, EXT_WORKSPACE_ID);
    assert_on_workspace!(harness.world(), 100, TEST_WORKSPACE_ID);
    assert_not_on_workspace!(harness.world(), 100, EXT_WORKSPACE_ID);
    harness
        .mock_state
        .update_window(0, |window| window.workspace_id = EXT_WORKSPACE_ID);
    harness
        .mock_state
        .update_window(100, |window| window.workspace_id = TEST_WORKSPACE_ID);
    harness.advance(Duration::from_secs(1));
    assert_on_workspace!(harness.world(), 0, EXT_WORKSPACE_ID);
    assert_on_workspace!(harness.world(), 100, TEST_WORKSPACE_ID);
}

#[test]
fn test_supplementary_allows_normal_laptop_scrolling_when_alone() {
    let config: Config = (
        MainOptions {
            supplementary_display: Some(TEST_DISPLAY_ID),
            ..Default::default()
        },
        vec![],
    )
        .into();
    TestHarness::new().with_config(config).with_windows(4)
        .on_iteration(1, |world, _| {
            crate::assert_focused!(world, 3);
            assert_eq!(world.query_filtered::<Entity, With<crate::ecs::supplementary::SupplementaryDisplay>>().iter(world).count(), 0);
            assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
            assert_on_workspace!(world, 3, TEST_WORKSPACE_ID);
        }).run(vec![Event::MenuOpened {window_id: 0}, Event::Command { command: Command::Window(Operation::Focus(Direction::Last)) }]);
}

#[test]
fn test_builtin_supplementary_evicts_extras_and_releases_role_after_unplug() {
    let mut harness = TestHarness::new()
        .with_windows(2)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(-800, 0, 0, 600),
            vec![EXT_WORKSPACE_ID],
        )
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(-750, 20, -350, 600);
        })
        .with_workspace_window(101, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(-350, 20, 0, 600);
        });
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    let (_, mut display) = world
        .query::<(Entity, &mut Display)>()
        .iter_mut(world)
        .find(|(_, display)| display.id() == EXT_DISPLAY_ID)
        .unwrap();
    display.set_built_in(true);
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    let members = world
        .query::<&LayoutStrip>()
        .iter(world)
        .filter(|strip| strip.id() == EXT_WORKSPACE_ID)
        .flat_map(LayoutStrip::all_windows)
        .collect::<Vec<_>>();
    assert_eq!(members.len(), 1);
    let occupant = members[0];
    let before = world.get::<Window>(occupant).unwrap().frame().width();
    harness.mock_state.remove_display(TEST_DISPLAY_ID);
    harness.world().write_message(Event::DisplayRemoved {
        display_id: TEST_DISPLAY_ID,
    });
    harness.advance(Duration::from_secs(3));
    assert!(
        harness
            .world()
            .get::<crate::ecs::supplementary::SupplementaryWindow>(occupant)
            .is_none()
    );
    assert!(
        harness
            .world()
            .get::<Window>(occupant)
            .unwrap()
            .frame()
            .width()
            < before
    );
    assert_eq!(
        harness
            .world()
            .query_filtered::<Entity, With<crate::ecs::supplementary::SupplementaryDisplay>>()
            .iter(harness.world())
            .count(),
        0
    );
}

#[test]
fn test_screen_cycle_restores_focus_and_offsets_across_three_displays() {
    let mut harness = TestHarness::new()
        .with_windows(3)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(-1920, 0, 0, 1200),
            vec![EXT_WORKSPACE_ID],
        )
        .with_display(
            EXT_DISPLAY_ID + 1,
            IRect::new(0, -1200, 1920, 0),
            vec![EXT_WORKSPACE_ID + 1],
        )
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(-1600, 20, -1200, 1000);
        })
        .with_workspace_window(200, EXT_WORKSPACE_ID + 1, |window| {
            window.frame = IRect::new(0, -1100, 400, -100);
        });
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Focus(Direction::Last)),
    });
    harness.advance(Duration::from_secs(1));
    let offsets = |world: &mut World| {
        world
            .query::<(&LayoutStrip, &crate::ecs::Position)>()
            .iter(world)
            .map(|(strip, position)| (strip.id(), position.0))
            .collect::<std::collections::HashMap<_, _>>()
    };
    let before = offsets(harness.world());
    for expected in [100, 200, 2, 100, 200, 2] {
        harness.world().write_message(Event::Command {
            command: Command::DisplayNext,
        });
        harness.advance(Duration::from_secs(1));
        crate::assert_focused!(harness.world(), expected);
        assert_eq!(offsets(harness.world()), before);
    }
}

#[test]
fn test_directional_screen_focus_preserves_each_strip_offset() {
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(-1920, 0, 0, 1200),
            vec![EXT_WORKSPACE_ID],
        )
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(-1600, 20, -1200, 1020);
        });
    harness.advance(Duration::from_secs(1));
    harness.world().write_message(Event::Command {
        command: Command::Window(Operation::Focus(Direction::Last)),
    });
    harness.advance(Duration::from_secs(1));
    let offsets = |world: &mut World| {
        world
            .query::<(&LayoutStrip, &crate::ecs::Position)>()
            .iter(world)
            .map(|(strip, position)| (strip.id(), position.0))
            .collect::<std::collections::HashMap<_, _>>()
    };
    let before = offsets(harness.world());
    harness.world().write_message(Event::Command {
        command: Command::DisplayFocus(Direction::West),
    });
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 100);
    assert_eq!(offsets(harness.world()), before);
    harness.world().write_message(Event::Command {
        command: Command::DisplayFocus(Direction::East),
    });
    harness.advance(Duration::from_secs(1));
    crate::assert_focused!(harness.world(), 2);
    assert_eq!(offsets(harness.world()), before);
}
