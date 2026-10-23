use std::sync::Arc;

use gridcraft_core::{CellRef, Value};
use gridcraft_model::{Cell, Formula, Workbook};

use crate::{Collab, merge};

fn set(wb: &mut Arc<Workbook>, sheet: usize, a1: &str, cell: Cell) {
    let w = Arc::make_mut(wb);
    let Some(s) = w.sheet_mut(sheet) else { panic!("no sheet {sheet}") };
    s.cells.set(CellRef::parse(a1).unwrap(), cell);
}

fn get(wb: &Workbook, sheet: usize, a1: &str) -> Option<Cell> {
    wb.sheet(sheet)?.cells.get(CellRef::parse(a1)?).cloned()
}

fn num(n: f64) -> Cell {
    Cell::value(Value::Number(n))
}

/// Two participants on the same workbook.
fn pair() -> (Collab, Arc<Workbook>, Collab, Arc<Workbook>) {
    let mut wb = Arc::new(Workbook::new());
    set(&mut wb, 0, "A1", num(1.0));
    let mut a = Collab::new(1);
    let seed = a.seed(&wb);
    let mut b = Collab::new(2);
    let opened = Arc::new(Workbook::new());
    let (wb_b, _) = b.join(&[seed], &opened).unwrap();
    (a, wb, b, Arc::new(wb_b))
}

#[test]
fn joining_gets_the_seeded_cells() {
    let (_, _, _, wb_b) = pair();
    assert_eq!(get(&wb_b, 0, "A1").map(|c| c.value), Some(Value::Number(1.0)));
}

#[test]
fn edits_to_different_cells_merge() {
    let (mut a, mut wa, mut b, mut wb) = pair();
    set(&mut wa, 0, "B2", num(2.0));
    set(&mut wb, 0, "C3", Cell::formula(Formula::new("=A1*3")));
    let ua = a.local(&wa).unwrap();
    let ub = b.local(&wb).unwrap();
    let (na, changed, _) = a.remote(&ub, &wa).unwrap();
    let (nb, _, _) = b.remote(&ua, &wb).unwrap();
    assert!(changed.cells.contains(&(0, CellRef::parse("C3").unwrap())));
    for w in [&na, &nb] {
        assert_eq!(get(w, 0, "B2").map(|c| c.value), Some(Value::Number(2.0)));
        assert_eq!(get(w, 0, "C3").and_then(|c| c.formula).map(|f| f.text.clone()), Some("A1*3".to_string()));
    }
}

#[test]
fn styles_travel_by_value_not_by_id() {
    let (mut a, mut wa, mut b, wb) = pair();
    let w = Arc::make_mut(&mut wa);
    let id = w.styles.derive(gridcraft_model::StyleId::DEFAULT, |s| s.font.bold = true);
    let mut cell = num(5.0);
    cell.style = id;
    set(&mut wa, 0, "D4", cell);
    let u = a.local(&wa).unwrap();
    let (nb, _, _) = b.remote(&u, &wb).unwrap();
    let c = get(&nb, 0, "D4").unwrap();
    assert!(nb.styles.get(c.style).font.bold);
}

#[test]
fn clearing_a_cell_reaches_the_others() {
    let (mut a, mut wa, mut b, wb) = pair();
    Arc::make_mut(&mut wa).sheet_mut(0).unwrap().cells.remove(CellRef::parse("A1").unwrap());
    let u = a.local(&wa).unwrap();
    let (nb, _, _) = b.remote(&u, &wb).unwrap();
    assert!(get(&nb, 0, "A1").is_none());
}

#[test]
fn sheets_added_and_renamed() {
    let (mut a, mut wa, mut b, wb) = pair();
    {
        let w = Arc::make_mut(&mut wa);
        w.sheets.push(Arc::new(gridcraft_model::Sheet::new("Plan")));
        w.sheets[0] = Arc::new(gridcraft_model::Sheet { name: "Daten".into(), ..(*w.sheets[0]).clone() });
    }
    set(&mut wa, 1, "A1", num(9.0));
    let u = a.local(&wa).unwrap();
    let (nb, changed, _) = b.remote(&u, &wb).unwrap();
    assert!(changed.structure);
    let names: Vec<&str> = nb.sheets.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["Daten", "Plan"]);
    assert_eq!(get(&nb, 0, "A1").map(|c| c.value), Some(Value::Number(1.0)));
    assert_eq!(get(&nb, 1, "A1").map(|c| c.value), Some(Value::Number(9.0)));
}

#[test]
fn undo_takes_back_only_my_own_step() {
    let (mut a, mut wa, mut b, mut wb) = pair();
    set(&mut wa, 0, "B1", num(10.0));
    let ua = a.local(&wa).unwrap();
    set(&mut wb, 0, "C1", num(20.0));
    let ub = b.local(&wb).unwrap();
    let (na, _, _) = a.remote(&ub, &wa).unwrap();
    let (nb, _, _) = b.remote(&ua, &wb).unwrap();
    // A undoes: B1 goes, B's C1 stays.
    let (na2, _, u) = a.undo(&na, false).unwrap().unwrap();
    assert!(get(&na2, 0, "B1").is_none());
    assert_eq!(get(&na2, 0, "C1").map(|c| c.value), Some(Value::Number(20.0)));
    let (nb2, _, _) = b.remote(&u, &nb).unwrap();
    assert!(get(&nb2, 0, "B1").is_none());
    assert_eq!(get(&nb2, 0, "C1").map(|c| c.value), Some(Value::Number(20.0)));
    // And redo brings it back.
    let (na3, _, _) = a.undo(&na2, true).unwrap().unwrap();
    assert_eq!(get(&na3, 0, "B1").map(|c| c.value), Some(Value::Number(10.0)));
}

#[test]
fn same_cell_last_writer_wins_on_both_sides() {
    let (mut a, mut wa, mut b, mut wb) = pair();
    set(&mut wa, 0, "A1", num(100.0));
    set(&mut wb, 0, "A1", num(200.0));
    let ua = a.local(&wa).unwrap();
    let ub = b.local(&wb).unwrap();
    let (na, _, _) = a.remote(&ub, &wa).unwrap();
    let (nb, _, _) = b.remote(&ua, &wb).unwrap();
    assert_eq!(get(&na, 0, "A1").map(|c| c.value), get(&nb, 0, "A1").map(|c| c.value));
}

#[test]
fn a_late_joiner_sees_everything_and_compaction_keeps_it() {
    let (mut a, mut wa, _, _) = pair();
    set(&mut wa, 0, "E5", num(7.0));
    let u = a.local(&wa).unwrap();
    let state = a.state();
    let mut c = Collab::new(3);
    let (wc, _) = c.join(&[merge(&[state, u])], &Arc::new(Workbook::new())).unwrap();
    assert_eq!(get(&wc, 0, "E5").map(|c| c.value), Some(Value::Number(7.0)));
    assert_eq!(get(&wc, 0, "A1").map(|c| c.value), Some(Value::Number(1.0)));
}

#[test]
fn nothing_changed_sends_nothing() {
    let (mut a, wa, _, _) = pair();
    assert!(a.local(&wa).is_none());
}

#[test]
fn junk_updates_are_errors_not_crashes() {
    let (_, _, mut b, wb) = pair();
    assert!(b.remote(&[1, 2, 3, 255, 0, 7], &wb).is_err());
    assert!(Collab::new(9).join(&[vec![255; 10]], &wb).is_err());
}
