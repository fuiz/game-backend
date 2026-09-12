//! The website's add-slide picker, played end to end.
//!
//! The JSON below is what the create page actually posts to `/add` for a
//! freshly added slide of each type, captured from the website's own
//! `slideTemplates` after its id-stripping and milliseconds passes. If the two
//! sides ever drift apart, this test is where it shows up rather than in a
//! classroom.

use fuiz::tick::Tick;
use fuiz::{
    AlarmMessage, SyncMessage, UpdateMessage,
    fuiz::{
        common::SlideStateManager,
        config::{Fuiz, SlideState},
    },
    game::{
        Game, HostScreen, IncomingHostMessage, IncomingMessage, IncomingPlayerMessage, Options, SlidePosition, State,
    },
    session::Tunnel,
    settings::Settings,
    watcher::{Id, ValueKind},
};
use garde::Validate;
use std::cell::RefCell;
use std::rc::Rc;

/// One freshly added slide per entry in the website's add-slide picker,
/// in the order the picker lists them.
const WEBSITE_SLIDES: &str = r#"[
  { "MultipleChoice": { "title": "", "introduce_question": 5000, "time_limit": 30000, "points_awarded": 1000, "answers": [] } },
  { "MultipleChoice": { "title": "", "introduce_question": 5000, "time_limit": 30000, "points_awarded": 1000,
      "answers": [ { "content": { "Text": "True" }, "correct": true }, { "content": { "Text": "False" }, "correct": false } ] } },
  { "TypeAnswer": { "title": "", "introduce_question": 5000, "time_limit": 60000, "points_awarded": 1000, "case_sensitive": false, "answers": [] } },
  { "Slider": { "title": "", "introduce_question": 5000, "time_limit": 30000, "points_awarded": 1000,
      "range": { "min": 0, "max": 100, "step": 1 }, "correct": 50, "tolerance": 5 } },
  { "Pin": { "title": "", "introduce_question": 5000, "time_limit": 30000, "points_awarded": 1000,
      "correct_area": { "Ellipse": { "center": { "x": 0.5, "y": 0.5 }, "radius_x": 0.12, "radius_y": 0.18 } } } },
  { "Order": { "title": "", "introduce_question": 5000, "time_limit": 60000, "points_awarded": 1000,
      "axis_labels": { "from": "", "to": "" }, "answers": [] } },
  { "Poll": { "title": "", "introduce_question": 5000, "time_limit": 30000, "points_awarded": 0,
      "answers": [ { "Text": "Yes" }, { "Text": "No" } ] } },
  { "Scale": { "title": "", "introduce_question": 5000, "time_limit": 30000, "points_awarded": 0,
      "min": 1, "max": 5, "style": "Agreement", "labels": {} } },
  { "Scale": { "title": "", "introduce_question": 5000, "time_limit": 30000, "points_awarded": 0,
      "min": 0, "max": 10, "style": "Nps", "labels": { "low": "Not at all likely", "high": "Extremely likely" } } },
  { "Pin": { "title": "", "introduce_question": 5000, "time_limit": 30000, "points_awarded": 0, "correct_area": null } },
  { "FreeText": { "title": "", "introduce_question": 5000, "time_limit": 60000, "points_awarded": 0,
      "mode": "WordCloud", "max_entries": 3, "max_entry_length": 40 } },
  { "FreeText": { "title": "", "introduce_question": 5000, "time_limit": 60000, "points_awarded": 0,
      "mode": "OpenEnded", "max_entries": 1, "max_entry_length": 200 } },
  { "Brainstorm": { "title": "", "introduce_question": 5000, "idea_time_limit": 120000, "vote_time_limit": 60000,
      "points_awarded": 0, "max_ideas_per_player": 3, "max_votes_per_player": 3, "max_idea_length": 200 } },
  { "InfoSlide": { "title": "", "duration": null } }
]"#;

/// How many slide types the picker offers. Kahoot's list, mirrored.
const PICKER_ENTRIES: usize = 14;

fn website_config() -> Fuiz {
    let slides = serde_json::from_str(WEBSITE_SLIDES).expect("website payloads should deserialize");
    Fuiz {
        title: "Every slide type".to_string(),
        slides,
    }
}

/// Collects everything the server would have sent, so a test can look at the
/// wire format rather than at internal state.
#[derive(Clone, Default)]
struct RecordingTunnel {
    sent: Rc<RefCell<Vec<String>>>,
}

impl Tunnel for RecordingTunnel {
    fn send_message(&self, message: &UpdateMessage) {
        self.sent
            .borrow_mut()
            .push(serde_json::to_string(message).expect("update messages serialize"));
    }

    fn send_state(&self, state: &SyncMessage) {
        self.sent
            .borrow_mut()
            .push(serde_json::to_string(state).expect("sync messages serialize"));
    }

    fn close(self) {}
}

/// The screen the host is looking at, derived the way the website derives it
/// from the messages it received. Building it here rather than asking the game
/// keeps the test honest about the client's half of the "ignore a stale Next"
/// handshake.
fn host_screen(game: &Game) -> HostScreen {
    match &game.state {
        State::WaitingScreen | State::TeamDisplay => HostScreen::Lobby,
        State::Leaderboard(index) => HostScreen::Leaderboard { index: *index },
        State::Done => HostScreen::Summary,
        State::Slide(current) => {
            let index = current.index;
            HostScreen::Slide(match &current.state {
                SlideState::MultipleChoice(s) => SlidePosition::MultipleChoice {
                    index,
                    phase: s.state(),
                },
                SlideState::TypeAnswer(s) => SlidePosition::TypeAnswer {
                    index,
                    phase: s.state(),
                },
                SlideState::Order(s) => SlidePosition::Order {
                    index,
                    phase: s.state(),
                },
                SlideState::Slider(s) => SlidePosition::Slider {
                    index,
                    phase: s.state(),
                },
                SlideState::Scale(s) => SlidePosition::Scale {
                    index,
                    phase: s.state(),
                },
                SlideState::Poll(s) => SlidePosition::Poll {
                    index,
                    phase: s.state(),
                },
                SlideState::Pin(s) => SlidePosition::Pin {
                    index,
                    phase: s.state(),
                },
                SlideState::FreeText(s) => SlidePosition::FreeText {
                    index,
                    phase: s.state(),
                },
                SlideState::Brainstorm(s) => SlidePosition::Brainstorm {
                    index,
                    phase: s.state(),
                },
                SlideState::InfoSlide(s) => SlidePosition::InfoSlide {
                    index,
                    phase: s.state(),
                },
            })
        }
    }
}

/// A fresh game plus the host id it was created with. `Game::new` registers
/// the host itself, so the caller has to keep hold of that id to drive it.
fn new_game() -> (Game, Id, RecordingTunnel) {
    let tunnel = RecordingTunnel::default();
    let host = Id::new();
    let game = Game::new(website_config(), Options::default(), host, &Settings::default());
    (game, host, tunnel)
}

/// Joins a player and gets them past the name screen, so their answers are
/// accepted rather than rejected as coming from an unassigned watcher.
fn join_player<F>(game: &mut Game, name: &str, finder: F) -> Id
where
    F: fuiz::session::TunnelFinder,
{
    let player = Id::new();
    game.add_unassigned(player, Tick::default(), &finder)
        .expect("under capacity");
    game.receive_message(
        player,
        IncomingMessage::Unassigned(fuiz::game::IncomingUnassignedMessage::NameRequest(name.to_string())),
        |_: AlarmMessage, _: std::time::Duration| {},
        Tick::default(),
        &finder,
    );
    player
}

#[test]
fn every_picker_payload_deserializes_and_validates() {
    let config = website_config();
    assert_eq!(
        config.len(),
        PICKER_ENTRIES,
        "the picker and this fixture should offer the same slides"
    );
    config
        .validate_with(&Settings::default())
        .expect("every freshly added slide should be a valid config");
}

#[test]
fn every_picker_payload_can_be_played_to_the_summary() {
    let (mut game, host, tunnel) = new_game();
    let finder = {
        let tunnel = tunnel.clone();
        move |_: Id| Some(tunnel.clone())
    };

    let mut schedule = |_: AlarmMessage, _: std::time::Duration| {};
    game.play(&mut schedule, Tick::default(), &finder);

    // Advancing repeatedly must eventually finish rather than stall on a slide.
    // Every phase of every slide gets its own press, so the bound is generous.
    for _ in 0..(PICKER_ENTRIES * 10) {
        if matches!(game.state, State::Done) {
            break;
        }
        let screen = host_screen(&game);
        game.receive_message(
            host,
            IncomingMessage::Host(IncomingHostMessage::Next(screen)),
            &mut schedule,
            Tick::default(),
            &finder,
        );
    }

    assert!(
        matches!(game.state, State::Done),
        "advancing through every slide should reach the summary"
    );
    assert!(
        !tunnel.sent.borrow().is_empty(),
        "playing should have produced messages"
    );
}

#[test]
fn every_answer_shape_is_safe_on_every_slide() {
    // A client can send any answer shape at any moment: a stale message, a
    // rejoin mid-slide, or an outright hostile one. None of it may panic, and
    // none of it may knock the game off its own phase chain.
    let (mut game, host, tunnel) = new_game();
    let finder = {
        let tunnel = tunnel.clone();
        move |_: Id| Some(tunnel.clone())
    };

    let player = join_player(&mut game, "Tester", &finder);

    let mut schedule = |_: AlarmMessage, _: std::time::Duration| {};
    game.play(&mut schedule, Tick::default(), &finder);

    for _ in 0..(PICKER_ENTRIES * 10) {
        if matches!(game.state, State::Done) {
            break;
        }

        for message in [
            IncomingPlayerMessage::IndexAnswer(0),
            IncomingPlayerMessage::IndexArrayAnswer(vec![0, 1]),
            IncomingPlayerMessage::StringAnswer("hello".to_string()),
            IncomingPlayerMessage::StringArrayAnswer(vec!["a".to_string(), "b".to_string()]),
            IncomingPlayerMessage::NumberAnswer(42.0),
            IncomingPlayerMessage::NumberAnswer(f64::NAN),
            IncomingPlayerMessage::PointAnswer { x: 0.5, y: 0.5 },
            IncomingPlayerMessage::PointAnswer {
                x: f64::INFINITY,
                y: -3.0,
            },
        ] {
            game.receive_message(
                player,
                IncomingMessage::Player(message),
                &mut schedule,
                Tick::default(),
                &finder,
            );
        }

        let screen = host_screen(&game);
        game.receive_message(
            host,
            IncomingMessage::Host(IncomingHostMessage::Next(screen)),
            &mut schedule,
            Tick::default(),
            &finder,
        );
    }

    assert!(
        matches!(game.state, State::Done),
        "a noisy client should not be able to stall the game"
    );
}

#[test]
fn a_stale_next_is_ignored() {
    // The host double-clicks: the second press names the screen they were on,
    // not the one the game moved to, so it must do nothing.
    let (mut game, host, tunnel) = new_game();
    let finder = {
        let tunnel = tunnel.clone();
        move |_: Id| Some(tunnel.clone())
    };

    let mut schedule = |_: AlarmMessage, _: std::time::Duration| {};
    game.play(&mut schedule, Tick::default(), &finder);

    let first = host_screen(&game);
    game.receive_message(
        host,
        IncomingMessage::Host(IncomingHostMessage::Next(first)),
        &mut schedule,
        Tick::default(),
        &finder,
    );
    let after_one = host_screen(&game);
    assert_ne!(first, after_one, "the first press should have advanced");

    game.receive_message(
        host,
        IncomingMessage::Host(IncomingHostMessage::Next(first)),
        &mut schedule,
        Tick::default(),
        &finder,
    );
    assert_eq!(
        after_one,
        host_screen(&game),
        "a press naming an old screen should be ignored"
    );
}

#[test]
fn opinion_and_info_slides_skip_the_leaderboard() {
    // Nothing scores on them, so a standings screen would just repeat the
    // previous one. The host should land straight on the next slide.
    let (mut game, host, tunnel) = new_game();
    let finder = {
        let tunnel = tunnel.clone();
        move |_: Id| Some(tunnel.clone())
    };

    let mut schedule = |_: AlarmMessage, _: std::time::Duration| {};
    game.play(&mut schedule, Tick::default(), &finder);

    // Which slide each leaderboard screen followed.
    let mut leaderboards_after = Vec::new();
    for _ in 0..(PICKER_ENTRIES * 10) {
        if matches!(game.state, State::Done) {
            break;
        }
        if let State::Leaderboard(index) = game.state {
            leaderboards_after.push(index);
        }
        let screen = host_screen(&game);
        game.receive_message(
            host,
            IncomingMessage::Host(IncomingHostMessage::Next(screen)),
            &mut schedule,
            Tick::default(),
            &finder,
        );
    }

    leaderboards_after.dedup();
    // Slides 0-5 are the scoring ones (quiz, true/false, type answer, slider,
    // pin answer, puzzle); 6-13 collect opinions or just present.
    assert_eq!(
        leaderboards_after,
        vec![0, 1, 2, 3, 4, 5],
        "only the scoring slides should show standings"
    );
}

#[test]
fn a_rejoining_player_gets_a_sync_message_for_every_slide() {
    // A reconnect must be able to catch up on any slide, whatever phase it's in.
    let (mut game, host, tunnel) = new_game();
    let finder = {
        let tunnel = tunnel.clone();
        move |_: Id| Some(tunnel.clone())
    };

    let player = join_player(&mut game, "Tester", &finder);

    let mut schedule = |_: AlarmMessage, _: std::time::Duration| {};
    game.play(&mut schedule, Tick::default(), &finder);

    let mut visited = Vec::new();
    for _ in 0..(PICKER_ENTRIES * 10) {
        if matches!(game.state, State::Done) {
            break;
        }
        let sync = game.state_message(player, ValueKind::Player, &finder);
        let json = serde_json::to_string(&sync).expect("sync serializes");
        assert!(!json.is_empty());
        if let State::Slide(current) = &game.state {
            visited.push(current.index);
        }

        let screen = host_screen(&game);
        game.receive_message(
            host,
            IncomingMessage::Host(IncomingHostMessage::Next(screen)),
            &mut schedule,
            Tick::default(),
            &finder,
        );
    }

    visited.dedup();
    assert_eq!(
        visited,
        (0..PICKER_ENTRIES).collect::<Vec<_>>(),
        "every slide should have been visited in order, each syncing cleanly"
    );
}
