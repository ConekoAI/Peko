//! The desk against a fake computer (ADR-123 §5): frame mapping, the
//! focus guard, the owner's hand, one run at a time, and keeping awake.

#![allow(clippy::many_single_char_names)]

use std::sync::Arc;

use super::fake::FakePlatform;
use super::*;

fn caller(run: &str) -> Caller {
    Caller {
        principal_id: "p1".into(),
        peko: "mochi".into(),
        session_id: Some(format!("s-{run}")),
        run_id: run.into(),
    }
}

fn desk() -> (Arc<FakePlatform>, Arc<ComputerDesk>) {
    let fake = FakePlatform::new();
    let desk = ComputerDesk::with_platform(Arc::new(Arc::clone(&fake)));
    (fake, desk)
}

fn shot(desk: &ComputerDesk, who: &Caller, req: ShotRequest) -> Shot {
    match desk.screenshot(who, &req).unwrap() {
        Outcome::Done(shot) => shot,
        Outcome::Refused(r) => panic!("screenshot refused: {r:?}"),
    }
}

fn act(desk: &ComputerDesk, who: &Caller, input: Input) -> Outcome<Acted> {
    desk.act(who, &input).unwrap()
}

fn refused(outcome: Outcome<Acted>) -> Refusal {
    match outcome {
        Outcome::Refused(r) => r,
        Outcome::Done(a) => panic!("expected a refusal, got {}", a.done),
    }
}

fn presses(fake: &FakePlatform) -> Vec<(f64, f64)> {
    fake.posted()
        .into_iter()
        .filter_map(|e| match e {
            InputEvent::Press {
                x, y, down: true, ..
            } => Some((x, y)),
            _ => None,
        })
        .collect()
}

fn click(frame: u64, x: f64, y: f64, window: u64) -> Input {
    Input::Click {
        frame,
        x,
        y,
        button: Button::Left,
        count: 1,
        window,
    }
}

#[test]
fn a_screen_frame_maps_picture_pixels_back_to_points() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let s = shot(&desk, &me, ShotRequest::default());
    // 1440×900 points at scale 2 is 2880×1800 pixels; the model gets at
    // most 1568 pixels on the long edge.
    let (w, h) = s.frame.picture;
    assert!(w <= picture::MAX_EDGE && h <= picture::MAX_EDGE);
    assert_eq!(&s.jpeg[..2], &[0xFF, 0xD8]);
    // The middle of Peko's window (100,50 800×600) in picture pixels.
    let sx = f64::from(w) / 1440.0;
    let (px, py) = ((500.0 * sx).floor(), (350.0 * sx).floor());
    assert!(matches!(
        act(&desk, &me, click(s.frame.id, px, py, 1)),
        Outcome::Done(_)
    ));
    let (x, y) = presses(&fake)[0];
    assert!(
        (x - 500.0).abs() < 1.5 && (y - 350.0).abs() < 1.5,
        "{x},{y}"
    );
}

#[test]
fn a_window_frame_follows_the_window_when_it_moves() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let s = shot(
        &desk,
        &me,
        ShotRequest {
            window: Some(1),
            ..ShotRequest::default()
        },
    );
    assert_eq!(s.frame.target, Target::Window(1));
    // 800×600 points at scale 2: 1600×1200 pixels, fitted to 1.2 MP.
    let (w, _) = s.frame.picture;
    let k = f64::from(w) / 800.0;
    fake.window_mut(1, |w| w.bounds = Rect::new(400.0, 200.0, 800.0, 600.0));
    assert!(matches!(
        act(&desk, &me, click(s.frame.id, 10.0 * k, 20.0 * k, 1)),
        Outcome::Done(_)
    ));
    let (x, y) = presses(&fake)[0];
    assert!(
        (x - 410.0).abs() < 1.5 && (y - 220.0).abs() < 1.5,
        "{x},{y}"
    );

    // A window that changed size needs a new screenshot; nothing is sent.
    fake.window_mut(1, |w| w.bounds.width = 700.0);
    let before = fake.posted().len();
    let r = refused(act(&desk, &me, click(s.frame.id, 5.0, 5.0, 1)));
    assert_eq!(r.status(), "stale");
    assert!(r.message().contains("new screenshot"));
    assert_eq!(fake.posted().len(), before);
}

#[test]
fn a_region_zooms_into_an_earlier_frame() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let full = shot(&desk, &me, ShotRequest::default());
    let (w, _) = full.frame.picture;
    let k = f64::from(w) / 1440.0;
    // Zoom on the 100×100 points at (600, 400).
    let zoom = shot(
        &desk,
        &me,
        ShotRequest {
            region: Some((full.frame.id, 600.0 * k, 400.0 * k, 100.0 * k, 100.0 * k)),
            ..ShotRequest::default()
        },
    );
    assert!((zoom.frame.area.x - 600.0).abs() < 1.0);
    assert!((zoom.frame.area.width - 100.0).abs() < 1.5);
    // Shown at full resolution: 200×200 pixels for 100 points at scale 2.
    assert!(zoom.frame.picture.0 >= 198);
    let (zw, _) = zoom.frame.picture;
    let zk = f64::from(zw) / zoom.frame.area.width;
    assert!(matches!(
        act(&desk, &me, click(zoom.frame.id, 50.0 * zk, 50.0 * zk, 1)),
        Outcome::Done(_)
    ));
    let (x, y) = presses(&fake)[0];
    assert!(
        (x - 650.0).abs() < 1.5 && (y - 450.0).abs() < 1.5,
        "{x},{y}"
    );
}

#[test]
fn frames_are_kept_for_the_runs_last_eight_screenshots() {
    let (_fake, desk) = desk();
    let me = caller("r1");
    let first = shot(&desk, &me, ShotRequest::default());
    for _ in 0..KEPT_FRAMES {
        shot(&desk, &me, ShotRequest::default());
    }
    let r = refused(act(&desk, &me, click(first.frame.id, 1.0, 1.0, 1)));
    assert!(r.message().contains("no frame"));
    // Another run has no frames of this one.
    let other = caller("r2");
    let last = shot(&desk, &me, ShotRequest::default());
    let r = refused(act(&desk, &other, click(last.frame.id, 1.0, 1.0, 1)));
    assert_eq!(r.status(), "stale");
}

#[test]
fn a_point_outside_the_picture_sends_nothing() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let s = shot(&desk, &me, ShotRequest::default());
    let (w, _) = s.frame.picture;
    let r = refused(act(
        &desk,
        &me,
        click(s.frame.id, f64::from(w) + 5.0, 1.0, 1),
    ));
    assert!(r.message().contains("outside frame"));
    assert!(fake.posted().is_empty());
}

#[test]
fn the_focus_guard_brings_the_window_forward_first() {
    let (fake, desk) = desk();
    let me = caller("r1");
    fake.bring_front(2);
    let r = act(
        &desk,
        &me,
        Input::Type {
            text: "hello".into(),
            secret: None,
            window: 1,
        },
    );
    assert!(matches!(r, Outcome::Done(_)));
    assert_eq!(fake.state().raised, vec![1]);
    assert_eq!(fake.posted(), vec![InputEvent::Text("hello".into())]);
}

#[test]
fn the_focus_guard_sends_nothing_when_another_app_stays_in_front() {
    let (fake, desk) = desk();
    let me = caller("r1");
    fake.state().raise_works = false;
    fake.bring_front(2);
    let r = refused(act(
        &desk,
        &me,
        Input::Type {
            text: "secret plans".into(),
            secret: None,
            window: 1,
        },
    ));
    assert_eq!(r.status(), "not_in_front");
    assert!(
        r.message().contains("Finder is in front"),
        "{}",
        r.message()
    );
    assert!(r.message().contains("nothing was sent"));
    assert!(fake.posted().is_empty());
}

#[test]
fn the_focus_guard_is_checked_between_pieces_of_text() {
    let (fake, desk) = desk();
    let me = caller("r1");
    fake.state().raise_works = false;
    // Finder comes in front after the first piece is typed.
    let f = Arc::clone(&fake);
    fake.state().after_post = Some(Arc::new(move |_| f.bring_front(2)));
    let text = "a".repeat(TEXT_PIECE * 3);
    let r = refused(act(
        &desk,
        &me,
        Input::Type {
            text,
            secret: None,
            window: 1,
        },
    ));
    assert_eq!(r.status(), "not_in_front");
    assert!(r.message().contains("1 of 3 pieces"), "{}", r.message());
    assert_eq!(fake.posted().len(), 1);
}

#[test]
fn a_click_on_a_point_another_app_covers_is_refused() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let s = shot(&desk, &me, ShotRequest::default());
    let k = f64::from(s.frame.picture.0) / 1440.0;
    // Finder (300,300 600×400) is behind Peko; put it in front, then click
    // where they overlap with Peko named.
    fake.bring_front(2);
    fake.state().raise_works = false;
    let r = refused(act(&desk, &me, click(s.frame.id, 500.0 * k, 400.0 * k, 1)));
    assert_eq!(r.status(), "not_in_front");
    // Peko in front again: the overlap is Peko's.
    fake.bring_front(1);
    assert!(matches!(
        act(&desk, &me, click(s.frame.id, 500.0 * k, 400.0 * k, 1)),
        Outcome::Done(_)
    ));
    // A point outside every window of Peko (over Finder only) is refused.
    let r = refused(act(&desk, &me, click(s.frame.id, 880.0 * k, 680.0 * k, 1)));
    assert!(r.message().contains("Finder covers"), "{}", r.message());
}

#[test]
fn a_frame_of_one_window_cannot_steer_input_to_another() {
    let (_fake, desk) = desk();
    let me = caller("r1");
    let s = shot(
        &desk,
        &me,
        ShotRequest {
            window: Some(2),
            ..ShotRequest::default()
        },
    );
    let r = refused(act(&desk, &me, click(s.frame.id, 1.0, 1.0, 1)));
    assert!(r.message().contains("shows window 2"));
}

#[test]
fn the_owners_hand_pauses_the_run_and_continue_lifts_it() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let s = shot(&desk, &me, ShotRequest::default());
    assert!(matches!(
        act(&desk, &me, click(s.frame.id, 500.0, 300.0, 1)),
        Outcome::Done(_)
    ));
    fake.owner_touches();
    let pause = desk.paused().expect("paused");
    assert_eq!(pause.session_id.as_deref(), Some("s-r1"));
    let r = refused(act(&desk, &me, click(s.frame.id, 500.0, 300.0, 1)));
    assert_eq!(r, Refusal::Paused);
    assert!(r.message().contains("End your turn"));
    // Looking still works while paused.
    shot(&desk, &me, ShotRequest::default());
    // Every run is held while the owner has the computer.
    let other = caller("r2");
    let o = shot(&desk, &other, ShotRequest::default());
    assert_eq!(
        refused(act(&desk, &other, click(o.frame.id, 1.0, 1.0, 1))),
        Refusal::Paused
    );
    assert_eq!(desk.state_view()["paused"]["peko"], "mochi");
    // Let it continue.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert!(rt.block_on(desk.resume()).is_some());
    assert!(desk.paused().is_none());
    assert!(matches!(
        act(&desk, &me, click(s.frame.id, 500.0, 300.0, 1)),
        Outcome::Done(_)
    ));
}

#[test]
fn an_owner_message_in_the_paused_session_lifts_the_pause() {
    let (fake, desk) = desk();
    let me = caller("r1");
    shot(&desk, &me, ShotRequest::default());
    fake.owner_touches();
    desk.owner_spoke("s-other");
    assert!(desk.paused().is_some());
    desk.owner_spoke("s-r1");
    assert!(desk.paused().is_none());
}

#[test]
fn the_owners_hand_stops_a_long_action_between_events() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let f = Arc::clone(&fake);
    fake.state().after_post = Some(Arc::new(move |n| {
        if n == 1 {
            f.owner_touches();
        }
    }));
    let r = refused(act(
        &desk,
        &me,
        Input::Type {
            text: "b".repeat(TEXT_PIECE * 4),
            secret: None,
            window: 1,
        },
    ));
    assert_eq!(r, Refusal::Paused);
    assert_eq!(fake.posted().len(), 1);
}

#[test]
fn a_drag_cut_short_never_leaves_the_button_down() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let s = shot(&desk, &me, ShotRequest::default());
    let f = Arc::clone(&fake);
    fake.state().after_post = Some(Arc::new(move |n| {
        if n == 3 {
            f.owner_touches();
        }
    }));
    let r = refused(act(
        &desk,
        &me,
        Input::Drag {
            frame: s.frame.id,
            from: (300.0, 200.0),
            to: (500.0, 300.0),
            window: 1,
        },
    ));
    assert_eq!(r, Refusal::Paused);
    let last = fake.posted().last().cloned().unwrap();
    assert!(
        matches!(last, InputEvent::Press { down: false, .. }),
        "{last:?}"
    );
}

#[test]
fn the_owner_idle_does_not_pause_anything() {
    let (fake, desk) = desk();
    fake.owner_touches();
    assert!(desk.paused().is_none());
}

#[test]
fn one_run_has_the_screen_at_a_time() {
    let (_fake, desk) = desk();
    let a = caller("r1");
    let b = Caller {
        principal_id: "p2".into(),
        peko: "taro".into(),
        ..caller("r2")
    };
    let sa = shot(&desk, &a, ShotRequest::default());
    assert!(matches!(
        act(&desk, &a, click(sa.frame.id, 500.0, 300.0, 1)),
        Outcome::Done(_)
    ));
    // The other peko may look, but not act.
    let sb = shot(&desk, &b, ShotRequest::default());
    let r = refused(act(&desk, &b, click(sb.frame.id, 500.0, 300.0, 1)));
    assert_eq!(r, Refusal::Busy("mochi".into()));
    // The holder keeps going.
    assert!(matches!(
        act(&desk, &a, click(sa.frame.id, 500.0, 300.0, 1)),
        Outcome::Done(_)
    ));
}

#[test]
fn stop_refuses_the_run_from_then_on() {
    let (_fake, desk) = desk();
    let me = caller("r1");
    let s = shot(&desk, &me, ShotRequest::default());
    assert!(matches!(
        act(&desk, &me, click(s.frame.id, 500.0, 300.0, 1)),
        Outcome::Done(_)
    ));
    let stopped = desk.stop().expect("a run to stop");
    assert_eq!(stopped.run_id, "r1");
    assert_eq!(
        refused(act(&desk, &me, click(s.frame.id, 500.0, 300.0, 1))),
        Refusal::Stopped
    );
    assert!(desk.stop().is_none());
    // Another run may use the computer.
    let other = caller("r2");
    let o = shot(&desk, &other, ShotRequest::default());
    assert!(matches!(
        act(&desk, &other, click(o.frame.id, 500.0, 300.0, 1)),
        Outcome::Done(_)
    ));
}

#[test]
fn a_locked_screen_stops_everything() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let s = shot(&desk, &me, ShotRequest::default());
    fake.state().obstruction = Some("The screen is locked.".into());
    let r = refused(act(&desk, &me, click(s.frame.id, 500.0, 300.0, 1)));
    assert_eq!(r.status(), "blocked");
    assert!(matches!(
        desk.screenshot(&me, &ShotRequest::default()).unwrap(),
        Outcome::Refused(Refusal::Blocked(_))
    ));
    assert!(fake.posted().is_empty());
}

#[test]
fn a_secret_is_never_typed_into_a_secure_prompt_and_is_scrubbed_from_titles() {
    let (fake, desk) = desk();
    let me = caller("r1");
    fake.state().secure_input = true;
    let r = refused(act(
        &desk,
        &me,
        Input::Type {
            text: "hunter2-hunter2".into(),
            secret: Some("PASS".into()),
            window: 1,
        },
    ));
    assert_eq!(r.status(), "blocked");
    assert!(fake.posted().is_empty());
    fake.state().secure_input = false;
    assert!(matches!(
        act(
            &desk,
            &me,
            Input::Type {
                text: "hunter2-hunter2".into(),
                secret: Some("PASS".into()),
                window: 1,
            },
        ),
        Outcome::Done(_)
    ));
    fake.window_mut(1, |w| w.title = "Signed in with hunter2-hunter2".into());
    let Outcome::Done(list) = desk.windows(&me, None).unwrap() else {
        panic!("windows refused")
    };
    assert_eq!(list[0].title, "Signed in with [secret:PASS]");
    assert_eq!(desk.secrets_typed("p1"), vec!["PASS".to_string()]);
}

#[test]
fn the_indicator_is_never_listed() {
    let (fake, desk) = desk();
    fake.state().windows.insert(
        0,
        WindowInfo {
            id: 9,
            pid: 300,
            app: "Peko".into(),
            title: INDICATOR_TITLE.into(),
            bounds: Rect::new(0.0, 0.0, 300.0, 40.0),
            minimized: false,
        },
    );
    let Outcome::Done(list) = desk.windows(&caller("r1"), None).unwrap() else {
        panic!("windows refused")
    };
    assert!(list.iter().all(|w| w.id != 9));
    let Outcome::Done(by_pid) = desk.windows(&caller("r1"), Some("200")).unwrap() else {
        panic!("windows refused")
    };
    assert_eq!(by_pid.len(), 1);
    assert_eq!(by_pid[0].app, "Finder");
}

#[test]
fn the_display_is_kept_awake_while_in_use() {
    let (fake, desk) = desk();
    assert!(!desk.keeping_awake());
    shot(&desk, &caller("r1"), ShotRequest::default());
    assert!(desk.keeping_awake());
    assert_eq!(fake.state().awake_holds, 1);
    desk.tidy();
    assert!(
        desk.keeping_awake(),
        "kept for a minute after the last action"
    );
    desk.state().last_action = std::time::Instant::now().checked_sub(AWAKE_LINGER * 2);
    desk.tidy();
    assert!(!desk.keeping_awake());
    assert_eq!(fake.state().awake_holds, 0);
}

#[test]
fn text_goes_in_pieces_that_never_split_a_character() {
    let text = format!("{}\nline two\tend", "😀".repeat(15));
    let events = pieces(&text);
    for e in &events {
        if let InputEvent::Text(t) = e {
            assert!(t.encode_utf16().count() <= TEXT_PIECE);
        }
    }
    let joined: String = events
        .iter()
        .map(|e| match e {
            InputEvent::Text(t) => t.clone(),
            InputEvent::Chord(c) if c.key == Key::Enter => "\n".into(),
            InputEvent::Chord(c) if c.key == Key::Tab => "\t".into(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(joined, text);
}

#[test]
fn the_state_shows_who_uses_the_screen_without_the_text() {
    let (_fake, desk) = desk();
    let me = caller("r1");
    act(
        &desk,
        &me,
        Input::Type {
            text: "my diary".into(),
            secret: None,
            window: 1,
        },
    );
    let view = desk.state_view();
    assert_eq!(view["using"]["peko"], "mochi");
    let action = view["using"]["action"].as_str().unwrap();
    assert_eq!(action, "typing 8 characters in Peko");
    assert!(!view.to_string().contains("diary"));
}

#[test]
fn turned_off_in_config_means_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.toml");
    assert!(enabled_in(&path));
    std::fs::write(&path, "[computer]\nenabled = false\n").unwrap();
    assert!(!enabled_in(&path));
    std::fs::write(&path, "[computer]\nenabled = true\n").unwrap();
    assert!(enabled_in(&path));
}

#[test]
fn a_run_that_ended_lets_go_of_the_screen() {
    let (fake, desk) = desk();
    let first = caller("r1");
    let next = caller("r2");
    let s1 = shot(&desk, &first, ShotRequest::default());
    assert!(matches!(
        act(&desk, &first, click(s1.frame.id, 500.0, 300.0, 1)),
        Outcome::Done(_)
    ));
    // The hand is asked for before the frame.
    assert_eq!(
        refused(act(&desk, &next, click(99, 500.0, 300.0, 1))),
        Refusal::Busy("another of your runs".into())
    );
    desk.run_ended("r1");
    // The owner's hand pauses nothing of a run that is over, and the
    // indicator goes away.
    fake.owner_touches();
    assert!(desk.paused().is_none());
    assert!(desk.state_view()["using"].is_null());
    let s2 = shot(&desk, &next, ShotRequest::default());
    // The next run has the screen at once, and the owner's hand pauses it.
    assert!(matches!(
        act(&desk, &next, click(s2.frame.id, 500.0, 300.0, 1)),
        Outcome::Done(_)
    ));
    fake.owner_touches();
    assert_eq!(desk.paused().unwrap().run_id, "r2");
    // Another run's end changes nothing.
    desk.run_ended("r1");
    assert!(desk.paused().is_some());
}

#[test]
fn a_pause_outlives_the_run_it_paused() {
    let (fake, desk) = desk();
    let me = caller("r1");
    let s = shot(&desk, &me, ShotRequest::default());
    assert!(matches!(
        act(&desk, &me, click(s.frame.id, 500.0, 300.0, 1)),
        Outcome::Done(_)
    ));
    fake.owner_touches();
    // Told it is paused, the run ends its turn; the pause waits for the
    // owner, and the indicator still shows it.
    desk.run_ended("r1");
    assert!(desk.paused().is_some());
    assert_eq!(desk.state_view()["paused"]["peko"], "mochi");
    desk.owner_spoke("s-r1");
    assert!(desk.paused().is_none());
}

#[test]
fn a_point_off_the_screen_sends_nothing() {
    let (fake, desk) = desk();
    let me = caller("r1");
    // Peko's window (800×600) hangs 360 points past the right edge.
    fake.window_mut(1, |w| w.bounds = Rect::new(1000.0, 50.0, 800.0, 600.0));
    fake.bring_front(1);
    let s = shot(
        &desk,
        &me,
        ShotRequest {
            window: Some(1),
            ..ShotRequest::default()
        },
    );
    let k = f64::from(s.frame.picture.0) / 800.0;
    let r = refused(act(&desk, &me, click(s.frame.id, 600.0 * k, 100.0 * k, 1)));
    assert_eq!(r.status(), "not_in_front");
    assert!(r.message().contains("off the screen"), "{}", r.message());
    let r = refused(act(
        &desk,
        &me,
        Input::Drag {
            frame: s.frame.id,
            from: (100.0 * k, 10.0 * k),
            to: (700.0 * k, 10.0 * k),
            window: 1,
        },
    ));
    assert!(r.message().contains("off the screen"), "{}", r.message());
    let r = refused(act(
        &desk,
        &me,
        Input::Move {
            frame: s.frame.id,
            x: 600.0 * k,
            y: 100.0 * k,
        },
    ));
    assert!(r.message().contains("off the screen"), "{}", r.message());
    assert!(fake.posted().is_empty(), "{:?}", fake.posted());
    // The part on the screen still works.
    assert!(matches!(
        act(&desk, &me, click(s.frame.id, 100.0 * k, 100.0 * k, 1)),
        Outcome::Done(_)
    ));
}

#[test]
fn the_result_names_the_window_as_the_input_left_it() {
    let (fake, desk) = desk();
    let me = caller("r1");
    fake.window_mut(1, |w| w.title = "*c6.txt - Notepad".into());
    let f = Arc::clone(&fake);
    fake.state().after_post = Some(Arc::new(move |_| {
        f.window_mut(1, |w| w.title = "c6.txt - Notepad".into());
    }));
    let save = Input::Key {
        chord: keys::parse("ctrl+s", false).unwrap(),
        window: 1,
    };
    let Outcome::Done(acted) = act(&desk, &me, save) else {
        panic!("ctrl+s refused")
    };
    assert_eq!(acted.window.unwrap().title, "c6.txt - Notepad");

    // A window the input closed is said to be gone.
    let f = Arc::clone(&fake);
    fake.state().after_post = Some(Arc::new(move |_| {
        f.state().windows.retain(|w| w.id != 1);
    }));
    let close = Input::Key {
        chord: keys::parse("ctrl+w", false).unwrap(),
        window: 1,
    };
    let Outcome::Done(acted) = act(&desk, &me, close) else {
        panic!("ctrl+w refused")
    };
    assert!(acted.window.is_none());
    assert!(
        acted.done.contains("window 1 is gone now"),
        "{}",
        acted.done
    );
}
