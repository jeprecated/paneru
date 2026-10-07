//! Gesture focus stepping must use the normal focus path without starting the
//! strip's free-scrolling/inertia pipeline, and fire only once until finger lift.
use bevy::prelude::*;

use crate::assert_focused;
use crate::commands::{Command, Direction, Operation};
use crate::config::Config;
use crate::ecs::{ActiveWorkspaceMarker, Scrolling, layout::LayoutStrip};
use crate::events::Event;

use super::*;

fn config(direction: &str, vertical: bool) -> Config {
    format!(
        "[options]\nauto_center = true\ncreate_virtual_workspace_automatically = true\n\
         [swipe.gesture]\nfingers_count = 3\nwindow_step = true\n\
         direction = '{direction}'\nvertical = {vertical}\n"
    )
    .as_str()
    .try_into()
    .unwrap()
}

fn swipe(delta: f64) -> Event {
    Event::Swipe { delta, fingers: 3 }
}

fn vertical_swipe(delta: f64) -> Event {
    Event::VerticalSwipe { delta, fingers: 3 }
}

fn workspace(world: &mut World) -> u32 {
    world
        .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
        .single(world)
        .unwrap()
        .virtual_index
}

fn assert_no_scrolling(world: &mut World) {
    assert_eq!(world.query::<&Scrolling>().iter(world).count(), 0);
}

#[test]
fn gesture_steps_focus_once_without_scrolling_and_rearm_on_lift() {
    TestHarness::new()
        .with_config(config("Natural", true))
        .with_windows(3)
        .on_iteration(3, |world, _| {
            assert_focused!(world, 0);
            assert_no_scrolling(world);
        })
        .on_iteration(4, |world, _| {
            assert_focused!(world, 1);
            assert_no_scrolling(world);
        })
        .on_iteration(6, |world, _| {
            assert_focused!(world, 1);
            assert_eq!(
                workspace(world),
                0,
                "diagonal tail must not switch workspace"
            );
            assert_no_scrolling(world);
        })
        .on_iteration(9, |world, _| assert_focused!(world, 2))
        .on_iteration(12, |world, _| {
            assert_focused!(world, 1);
            assert_no_scrolling(world);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::Window(Operation::Focus(Direction::First)),
            },
            Event::TouchpadDown,
            swipe(0.2),           // Subthreshold: no movement or focus change.
            swipe(0.3),           // Cumulative threshold: focus the adjacent column.
            swipe(2.0),           // Same gesture: never skip more columns.
            vertical_swipe(-2.0), // Same gesture: no second action on the other axis.
            Event::TouchpadUp,
            Event::TouchpadDown,
            swipe(0.5),
            Event::TouchpadUp,
            Event::TouchpadDown,
            swipe(-0.5),
        ]);
}

#[test]
fn gesture_steps_reverse_direction_ignore_wrong_fingers_and_stop_at_edge() {
    TestHarness::new()
        .with_config(config("Reversed", true))
        .with_windows(2)
        .on_iteration(3, |world, _| assert_focused!(world, 0))
        .on_iteration(4, |world, _| assert_focused!(world, 0))
        .on_iteration(7, |world, _| assert_focused!(world, 1))
        .on_iteration(10, |world, _| {
            assert_focused!(world, 1);
            assert_no_scrolling(world);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::Window(Operation::Focus(Direction::First)),
            },
            Event::TouchpadDown,
            Event::Swipe {
                delta: -1.0,
                fingers: 4,
            },
            swipe(0.5), // Reversed goes west: already at the edge.
            Event::TouchpadUp,
            Event::TouchpadDown,
            swipe(-0.5),
            Event::TouchpadUp,
            Event::TouchpadDown,
            swipe(-0.5), // Already at the east edge; never wrap.
        ]);
}

#[test]
fn gesture_steps_vertical_switches_once_and_rearms_without_timeout() {
    TestHarness::new()
        .with_config(config("Natural", true))
        .with_windows(1)
        .on_iteration(2, |world, _| assert_eq!(workspace(world), 1))
        .on_iteration(4, |world, _| {
            assert_eq!(workspace(world), 1);
            assert_no_scrolling(world);
        })
        .on_iteration(7, |world, _| assert_eq!(workspace(world), 0))
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::TouchpadDown,
            vertical_swipe(-0.5),
            vertical_swipe(-1.0),
            swipe(1.0),
            Event::TouchpadUp,
            Event::TouchpadDown,
            vertical_swipe(0.5),
        ]);
}

#[test]
fn gesture_steps_respects_disabled_vertical_gestures() {
    TestHarness::new()
        .with_config(config("Natural", false))
        .with_windows(1)
        .on_iteration(2, |world, _| {
            assert_eq!(workspace(world), 0);
            assert_no_scrolling(world);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::TouchpadDown,
            vertical_swipe(-1.0),
        ]);
}

#[test]
#[allow(clippy::cast_precision_loss)] // Counts below are exactly 3 and 4.
fn gesture_steps_small_threshold_uses_average_finger_travel_on_both_axes() {
    for fingers in [3, 4] {
        let config: Config = format!(
            "[options]\ncreate_virtual_workspace_automatically = true\n\
             [swipe.gesture]\nfingers_count = {fingers}\nwindow_step = true\n\
             step_threshold = 0.02\n"
        )
        .as_str()
        .try_into()
        .unwrap();
        TestHarness::new()
            .with_config(config)
            .with_windows(2)
            .on_iteration(3, |world, _| {
                assert_focused!(world, 0);
                assert_no_scrolling(world);
            })
            .on_iteration(4, |world, _| assert_focused!(world, 1))
            .on_iteration(5, |world, _| assert_focused!(world, 1))
            .on_iteration(8, |world, _| assert_eq!(workspace(world), 0))
            .on_iteration(9, |world, _| assert_eq!(workspace(world), 1))
            .run(vec![
                Event::MenuOpened { window_id: 0 },
                Event::Command {
                    command: Command::Window(Operation::Focus(Direction::First)),
                },
                Event::TouchpadDown,
                Event::Swipe {
                    delta: 0.01 * fingers as f64,
                    fingers,
                },
                Event::Swipe {
                    delta: 0.011 * fingers as f64,
                    fingers,
                },
                Event::Swipe {
                    delta: 0.5 * fingers as f64,
                    fingers,
                },
                Event::TouchpadUp,
                Event::TouchpadDown,
                Event::VerticalSwipe {
                    delta: -0.01 * fingers as f64,
                    fingers,
                },
                Event::VerticalSwipe {
                    delta: -0.011 * fingers as f64,
                    fingers,
                },
            ]);
    }
}

#[test]
fn gesture_step_threshold_clamps_invalid_values_and_is_independent_of_sensitivity() {
    for (threshold, expected) in [
        ("0.02", 0.02),
        ("0.0", 0.001),
        ("-1.0", 0.001),
        ("2.0", 1.0),
    ] {
        for sensitivity in [0.1, 2.0] {
            let config: Config = format!(
                "[swipe]\nsensitivity = {sensitivity}\n[swipe.gesture]\nstep_threshold = {threshold}\n"
            ).as_str().try_into().unwrap();
            assert!((config.swipe_gesture_step_threshold() - expected).abs() < f64::EPSILON);
        }
    }
    for threshold in ["nan", "inf", "-inf"] {
        let config: Config = format!("[swipe.gesture]\nstep_threshold = {threshold}\n")
            .as_str()
            .try_into()
            .unwrap();
        assert!((config.swipe_gesture_step_threshold() - 0.15 / 0.35 / 3.0).abs() < f64::EPSILON);
    }
}
