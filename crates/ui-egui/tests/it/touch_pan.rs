//! Touch input on the grid: one finger pans, a tap selects (issue #1907).

use egui::{Event, Modifiers, PointerButton, TouchDeviceId, TouchId, TouchPhase};
use gridcraft_engine::Session;
use gridcraft_engine::core::CellRef;
use gridcraft_ui_egui::SheetApp;

fn harness(session: Session) -> egui_kittest::Harness<'static, SheetApp> {
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_ui_state(
        |ui, app: &mut SheetApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        SheetApp::new(session, Default::default()),
    );
    h.run_steps(4);
    h
}

fn blank() -> Session {
    let mut s = Session::new();
    s.new_workbook();
    s
}

/// One finger, sent the way egui-winit does: a touch event plus the emulated pointer events.
fn touch(h: &mut egui_kittest::Harness<'static, SheetApp>, phase: TouchPhase, pos: egui::Pos2) {
    let ev = &mut h.input_mut().events;
    ev.push(Event::Touch { device_id: TouchDeviceId(1), id: TouchId(0), phase, pos, force: None });
    match phase {
        TouchPhase::Start => {
            ev.push(Event::PointerMoved(pos));
            ev.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
        }
        TouchPhase::Move => ev.push(Event::PointerMoved(pos)),
        TouchPhase::End | TouchPhase::Cancel => {
            ev.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
            ev.push(Event::PointerGone);
        }
    }
}

fn swipe(h: &mut egui_kittest::Harness<'static, SheetApp>, from: egui::Pos2, by: egui::Vec2) {
    touch(h, TouchPhase::Start, from);
    h.run_steps(2);
    for i in 1..=10 {
        touch(h, TouchPhase::Move, from + by * (i as f32 / 10.0));
        h.run_steps(1);
    }
    touch(h, TouchPhase::End, from + by);
    h.run_steps(3);
}

fn assert_selection_untouched(h: &egui_kittest::Harness<'static, SheetApp>) {
    let sel = &h.state().session.active().unwrap().selection;
    assert_eq!(sel.active, CellRef::new(0, 0), "a touch pan must not move the active cell: {sel:?}");
    assert_eq!(sel.ranges.len(), 1, "{sel:?}");
    assert_eq!(sel.ranges[0].start, CellRef::new(0, 0), "{sel:?}");
    assert_eq!(sel.ranges[0].end, CellRef::new(0, 0), "a touch pan must not select cells: {sel:?}");
}

#[test]
fn one_finger_drag_pans_the_grid_instead_of_selecting() {
    let mut h = harness(blank());
    let cells = h.state().grid.cells_rect.unwrap();
    swipe(&mut h, cells.center() + egui::vec2(0.0, 100.0), egui::vec2(0.0, -200.0));
    let scroll = h.state().view().scroll;
    assert!(scroll.y > 0.0, "dragging a finger up scrolls down: scroll = {scroll:?}");
    assert_selection_untouched(&h);

    swipe(&mut h, cells.center() + egui::vec2(100.0, 0.0), egui::vec2(-200.0, 0.0));
    let scroll = h.state().view().scroll;
    assert!(scroll.x > 0.0, "dragging a finger left scrolls right: scroll = {scroll:?}");
    assert_selection_untouched(&h);
}

#[test]
fn one_finger_tap_still_selects_the_cell() {
    let mut h = harness(blank());
    let cells = h.state().grid.cells_rect.unwrap();
    let p = cells.min + egui::vec2(5.0, 5.0) + egui::vec2(0.0, cells.height() / 2.0);
    touch(&mut h, TouchPhase::Start, p);
    h.run_steps(1);
    touch(&mut h, TouchPhase::End, p);
    h.run_steps(3);
    let sel = &h.state().session.active().unwrap().selection;
    assert_eq!(sel.active.col, 0, "{sel:?}");
    assert!(sel.active.row > 0, "a tap selects the tapped cell: {sel:?}");
    assert_eq!(h.state().view().scroll, egui::Vec2::ZERO);
}

fn tap(h: &mut egui_kittest::Harness<'static, SheetApp>, p: egui::Pos2) {
    touch(h, TouchPhase::Start, p);
    h.run_steps(1);
    touch(h, TouchPhase::End, p);
    h.run_steps(3);
}

/// On a phone: the first tap selects a cell, tapping it again starts editing it (the on-screen
/// keyboard appears for an editor only); tapping another cell enters the text.
#[test]
fn tapping_the_active_cell_again_edits_it() {
    let mut h = harness(blank());
    let cells = h.state().grid.cells_rect.unwrap();
    let p = cells.min + egui::vec2(100.0, 50.0);
    tap(&mut h, p);
    let active = h.state().session.active().unwrap().selection.active;
    assert!(h.state().editor.is_none(), "the first tap only selects");
    // Well past a double-tap.
    h.run_steps(1);
    std::thread::sleep(std::time::Duration::from_millis(500));
    tap(&mut h, p);
    let ed = h.state().editor.as_ref().expect("the second tap edits the cell");
    assert_eq!(ed.cell, active);
    assert!(h.ctx.egui_wants_keyboard_input(), "the editor takes the keyboard (the web shell focuses its text field)");
    h.input_mut().events.push(Event::Text("42".into()));
    h.run_steps(2);
    tap(&mut h, p + egui::vec2(0.0, 80.0));
    assert!(h.state().editor.is_none());
    let d = h.state().session.active().unwrap();
    assert_eq!(d.wb.active().unwrap().value(active), gridcraft_engine::core::Value::number(42.0));
}

/// A mouse click on the active cell still only selects (desktop behaviour is unchanged).
#[test]
fn clicking_the_active_cell_again_does_not_edit() {
    let mut h = harness(blank());
    let cells = h.state().grid.cells_rect.unwrap();
    let p = cells.min + egui::vec2(100.0, 50.0);
    for _ in 0..2 {
        h.input_mut().events.push(Event::PointerMoved(p));
        h.input_mut().events.push(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
        h.run_steps(1);
        h.input_mut().events.push(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
        h.run_steps(2);
        std::thread::sleep(std::time::Duration::from_millis(500));
        h.run_steps(1);
    }
    assert!(h.state().editor.is_none());
}

/// Two fingers spreading apart zoom in smoothly, by about how far they spread.
#[test]
fn pinching_zooms_with_the_fingers() {
    let mut h = harness(blank());
    let cells = h.state().grid.cells_rect.unwrap();
    let c = cells.center();
    let fingers = |h: &mut egui_kittest::Harness<'static, SheetApp>, phase: TouchPhase, d: f32| {
        for (id, dx) in [(0u64, -d), (1, d)] {
            h.input_mut().events.push(Event::Touch {
                device_id: TouchDeviceId(1),
                id: TouchId(id),
                phase,
                pos: c + egui::vec2(dx, 0.0),
                force: None,
            });
        }
    };
    // A browser also moves the pointer to the first finger.
    h.input_mut().events.push(Event::PointerMoved(c - egui::vec2(60.0, 0.0)));
    fingers(&mut h, TouchPhase::Start, 60.0);
    h.run_steps(1);
    for i in 1..=10 {
        fingers(&mut h, TouchPhase::Move, 60.0 + 3.0 * i as f32);
        h.run_steps(1);
    }
    fingers(&mut h, TouchPhase::End, 90.0);
    h.run_steps(3);
    let zoom = h.state().session.active().unwrap().wb.active().unwrap().zoom;
    assert!((140..=160).contains(&zoom), "fingers spread 1.5 times zoom to about 150%: {zoom}");
    assert!(h.state().editor.is_none());
}

/// The phone layout (a taller formula bar, a narrower Name Box) draws at phone sizes, also when
/// the on-screen keyboard leaves little room, and on a screen narrower than the bar's buttons.
#[test]
fn phone_layout_fits_small_screens() {
    for size in [egui::vec2(390.0, 844.0), egui::vec2(390.0, 380.0), egui::vec2(120.0, 300.0)] {
        let mut app = SheetApp::new(blank(), Default::default());
        app.touch_layout = true;
        app.ui.ribbon_collapsed = true;
        let mut h = egui_kittest::Harness::builder().with_size(size).build_ui_state(
            |ui, app: &mut SheetApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            app,
        );
        h.run_steps(4);
        if size.x > 300.0 {
            let cells = h.state().grid.cells_rect.unwrap();
            assert!(cells.top() < size.y / 2.0, "the cells keep room below the bars at {size:?}: {cells:?}");
            tap(&mut h, cells.min + egui::vec2(60.0, 30.0));
            std::thread::sleep(std::time::Duration::from_millis(500));
            tap(&mut h, cells.min + egui::vec2(60.0, 30.0));
            assert!(h.state().editor.is_some(), "editing works at {size:?}");
        }
    }
}
