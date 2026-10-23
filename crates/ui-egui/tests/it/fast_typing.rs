//! Typing faster than the app draws: keys land in the cells they were typed into. A slow frame
//! (a phone, software graphics, a workbook edited together) gets several keys at once; the
//! editor used to commit on Tab or Enter before taking the frame's text, and a Tab right after an
//! edit began also moved focus away from the grid, so the next key went nowhere.
use egui::{Context, Event, Key, Modifiers, RawInput, Rect, pos2, vec2};
use gridcraft_engine::Session;
use gridcraft_ui_egui::SheetApp;
use serde_json::json;

fn frame(ctx: &Context, app: &mut SheetApp, time: f64, events: Vec<Event>) {
    let mut raw =
        RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0))), time: Some(time), events, ..Default::default() };
    app.raw_input_hook(&mut raw);
    let mut output = ctx.run_ui(raw, |ui| {
        let ctx = ui.ctx().clone();
        app.logic(&ctx);
        app.ui(ui);
    });
    output.textures_delta.clear();
}

fn key(key: Key) -> Event {
    Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }
}

fn text(s: &str) -> Event {
    Event::Text(s.into())
}

fn shown(app: &mut SheetApp, cell: &str) -> String {
    app.session.run("cell.get", json!({"cell": cell})).ok().and_then(|v| v["text"].as_str().map(str::to_string)).unwrap_or_default()
}

/// A row and the start of the next one, typed into a new workbook in `frames` (each a list of
/// the keys that arrive together), then a few quiet frames.
fn typed(frames: Vec<Vec<Event>>) -> SheetApp {
    let ctx = Context::default();
    let mut session = Session::new();
    session.new_workbook();
    let mut app = SheetApp::new(session, Default::default());
    let mut t = 0.0;
    for events in std::iter::repeat_n(Vec::new(), 4).chain(frames).chain(std::iter::repeat_n(Vec::new(), 20)) {
        frame(&ctx, &mut app, t, events);
        t += 0.01;
    }
    app
}

fn keys() -> Vec<Event> {
    vec![text("K"), text("anal"), key(Key::Tab), text("Budget"), key(Key::Tab), text("=2*21"), key(Key::Enter), text("Messe"), key(Key::Enter)]
}

fn check(mut app: SheetApp) {
    assert_eq!(shown(&mut app, "A1"), "Kanal");
    assert_eq!(shown(&mut app, "B1"), "Budget");
    assert_eq!(shown(&mut app, "C1"), "42");
    assert_eq!(shown(&mut app, "A2"), "Messe", "Enter after a run of Tabs goes back to the first column");
    assert!(app.editor.is_none());
}

#[test]
fn keys_that_arrive_in_one_frame_go_to_their_own_cells() {
    check(typed(vec![keys()]));
}

#[test]
fn a_key_in_every_frame_goes_to_its_own_cell() {
    check(typed(keys().into_iter().map(|e| vec![e]).collect()));
}
