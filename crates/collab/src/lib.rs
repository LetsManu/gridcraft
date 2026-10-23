//! Live co-editing: several people work on one workbook at the same time.
//!
//! The workbook lives in a Yjs document ([yrs]) every participant holds a replica of. The engine
//! keeps editing its own [`Workbook`] as before; this crate translates between the two:
//! [`Collab::local`] writes what changed since the last sync, [`Collab::remote`] merges another
//! participant's update and rebuilds only what it touched.
//!
//! Like co-authoring in a spreadsheet, the unit is the cell: two people typing into different cells
//! never disturb each other, and the last one to change a cell wins it.
//!
//! | Root map | Key | Value |
//! |---|---|---|
//! | `book` | `order` | the sheets' ids in tab order (JSON array) |
//! | `book` | a [`Workbook`] field (defined names, theme, calculation settings…) | that field as JSON |
//! | `props` | `<sheet id>\t<field>` | a [`Sheet`] field (name, column widths, merges, charts…) as JSON |
//! | `cells` | `<sheet id>\t<A1>` | the cell as JSON: input (formula or constant) and its style |
//!
//! What only concerns one person's view (scroll position, active cell, zoom, the active sheet) is
//! not shared. Formula results are not shared either: every participant calculates them.
//! Undo and redo take back only this participant's own steps.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_model::{Cell, CellStore, Formula, Sheet, Style, StyleId, Workbook};
use serde_json::{Map as JsonMap, Value as Json, json};
use yrs::undo::{Options as UndoOptions, UndoManager};
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Map, MapRef, Origin, Out, ReadTxn, StateVector, Transact, TransactionMut, Update};

const LOCAL: &str = "gridcraft-local";
const SEP: char = '\t';

/// Workbook fields that belong to the document (shared); the others are this person's view or
/// too large to send with every change (`vba_project`).
const BOOK_SKIP: &[&str] = &["sheets", "styles", "active_sheet", "vba_project", "csv_delimiter"];
/// Sheet fields that are this person's view, or kept elsewhere (`cells`).
const SHEET_SKIP: &[&str] = &["cells", "view_active", "view_top_left", "zoom", "view"];

#[derive(Debug, thiserror::Error)]
pub enum CollabError {
    #[error("the update from the other participants could not be read: {0}")]
    Update(String),
    #[error("the shared workbook could not be read: {0}")]
    Shape(String),
}

pub type Result<T> = std::result::Result<T, CollabError>;

/// What a merge changed, so the caller recalculates no more than needed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Changed {
    /// Cells whose input changed: (sheet index, cell).
    pub cells: Vec<(usize, CellRef)>,
    /// Sheets were added, removed, reordered or renamed, or a sheet or workbook setting changed:
    /// recalculate everything.
    pub structure: bool,
}

/// One participant's replica of the shared workbook.
pub struct Collab {
    doc: Doc,
    book: MapRef,
    props: MapRef,
    cells: MapRef,
    client: u64,
    next_sheet: u64,
    /// The workbook as last written to or read from the shared document.
    synced: Option<Arc<Workbook>>,
    /// The shared id of every sheet of `synced`, in order.
    ids: Vec<String>,
    /// The JSON last written or read per entry (`map\tkey`), to send only what changed.
    seen: HashMap<String, Arc<str>>,
    undo: UndoManager<()>,
}

impl std::fmt::Debug for Collab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collab").field("client", &self.client).finish_non_exhaustive()
    }
}

/// A cell as shared: what was typed (formula or constant), and the style itself (style ids are
/// only meaningful in one participant's style table).
fn cell_json(c: &Cell, wb: &Workbook) -> Option<String> {
    let mut o = JsonMap::new();
    match &c.formula {
        Some(f) => {
            o.insert("f".into(), Json::String(f.text.clone()));
            if let Some(a) = f.array {
                o.insert("a".into(), Json::String(a.a1()));
            }
            if f.dynamic {
                o.insert("d".into(), Json::Bool(true));
            }
        }
        None => {
            if !c.value.is_empty() {
                o.insert("v".into(), serde_json::to_value(&c.value).ok()?);
            }
        }
    }
    if c.style != StyleId::DEFAULT {
        o.insert("s".into(), serde_json::to_value(wb.styles.get(c.style)).ok()?);
    }
    serde_json::to_string(&Json::Object(o)).ok()
}

/// A shared cell in this participant's workbook (its style interned in `wb`'s table).
fn cell_from(json: &str, wb: &mut Workbook) -> Option<Cell> {
    let v: Json = serde_json::from_str(json).ok()?;
    let style = match v.get("s") {
        Some(s) => {
            let style: Style = serde_json::from_value(s.clone()).ok()?;
            wb.styles.intern(style)
        }
        None => StyleId::DEFAULT,
    };
    let mut cell = match v.get("f").and_then(Json::as_str) {
        Some(text) => {
            let mut f = Formula::new(text);
            f.array = v.get("a").and_then(Json::as_str).and_then(RangeRef::parse);
            f.dynamic = v.get("d").and_then(Json::as_bool).unwrap_or(false);
            Cell::formula(f)
        }
        None => Cell::value(v.get("v").and_then(|x| serde_json::from_value::<Value>(x.clone()).ok()).unwrap_or_default()),
    };
    cell.style = style;
    Some(cell)
}

/// The shared fields of a sheet, each as JSON.
fn sheet_fields(s: &Sheet) -> Vec<(String, String)> {
    let mut q = s.clone();
    q.cells = CellStore::new();
    let Ok(Json::Object(m)) = serde_json::to_value(&q) else { return Vec::new() };
    m.into_iter().filter(|(k, _)| !SHEET_SKIP.contains(&k.as_str())).filter_map(|(k, v)| serde_json::to_string(&v).ok().map(|s| (k, s))).collect()
}

/// The shared fields of a workbook, each as JSON.
fn book_fields(wb: &Workbook) -> Vec<(String, String)> {
    let mut q = wb.clone();
    q.sheets = Vec::new();
    q.vba_project = None;
    let Ok(Json::Object(m)) = serde_json::to_value(&q) else { return Vec::new() };
    m.into_iter().filter(|(k, _)| !BOOK_SKIP.contains(&k.as_str())).filter_map(|(k, v)| serde_json::to_string(&v).ok().map(|s| (k, s))).collect()
}

/// `sheet` with the shared `fields` replaced (the cells and this person's view stay).
fn patch_sheet(sheet: &Sheet, fields: &[(String, Json)]) -> Result<Sheet> {
    let mut q = sheet.clone();
    let cells = std::mem::take(&mut q.cells);
    let mut v = serde_json::to_value(&q).map_err(|e| CollabError::Shape(e.to_string()))?;
    if let Json::Object(m) = &mut v {
        for (k, val) in fields {
            m.insert(k.clone(), val.clone());
        }
    }
    let mut out: Sheet = serde_json::from_value(v).map_err(|e| CollabError::Shape(e.to_string()))?;
    out.cells = cells;
    out.preserved = sheet.preserved.clone();
    out.spill = sheet.spill.clone();
    out.spill_ranges = sheet.spill_ranges.clone();
    Ok(out)
}

/// `wb` with the shared workbook `fields` replaced (sheets, styles and this person's view stay).
fn patch_book(wb: &Workbook, fields: &[(String, Json)]) -> Result<Workbook> {
    let mut q = wb.clone();
    let sheets = std::mem::take(&mut q.sheets);
    let vba = q.vba_project.take();
    let mut v = serde_json::to_value(&q).map_err(|e| CollabError::Shape(e.to_string()))?;
    if let Json::Object(m) = &mut v {
        for (k, val) in fields {
            m.insert(k.clone(), val.clone());
        }
    }
    let mut out: Workbook = serde_json::from_value(v).map_err(|e| CollabError::Shape(e.to_string()))?;
    out.sheets = sheets;
    out.styles = wb.styles.clone();
    out.vba_project = vba;
    out.preserved = wb.preserved.clone();
    out.source_dir = wb.source_dir.clone();
    out.source_path = wb.source_path.clone();
    out.active_sheet = wb.active_sheet;
    out.csv_delimiter = wb.csv_delimiter;
    Ok(out)
}

fn text(v: Out) -> Option<Arc<str>> {
    match v {
        Out::Any(Any::String(s)) => Some(s),
        _ => None,
    }
}

fn split(key: &str) -> Option<(&str, &str)> {
    key.split_once(SEP)
}

impl Collab {
    /// A replica; `client_id` identifies it in the CRDT and must be unique (random is fine).
    pub fn new(client_id: u64) -> Self {
        let client = client_id & ((1 << 53) - 1);
        let doc = Doc::with_client_id(client);
        let book = doc.get_or_insert_map("book");
        let props = doc.get_or_insert_map("props");
        let cells = doc.get_or_insert_map("cells");
        let mut undo = UndoManager::with_options(UndoOptions {
            capture_timeout_millis: u64::MAX,
            tracked_origins: [Origin::from(LOCAL)].into_iter().collect(),
            capture_transaction: None,
            timestamp: Arc::new(|| 1u64),
            init_undo_stack: Vec::new(),
            init_redo_stack: Vec::new(),
        });
        for m in [&book, &props, &cells] {
            undo.expand_scope(&doc, m);
        }
        Collab { doc, book, props, cells, client, next_sheet: 0, synced: None, ids: Vec::new(), seen: HashMap::new(), undo }
    }

    /// The first participant fills the shared document with the workbook it opened.
    pub fn seed(&mut self, wb: &Arc<Workbook>) -> Vec<u8> {
        self.synced = Some(Arc::new(Workbook { sheets: Vec::new(), ..Workbook::new() }));
        self.ids.clear();
        let update = self.local(wb).unwrap_or_default();
        self.undo.clear_all();
        self.step();
        update
    }

    /// Join a session: merge everything so far and build the workbook on top of `opened` (the
    /// same file, so parts the shared document does not carry, like unmodelled XLSX content, stay).
    pub fn join(&mut self, updates: &[Vec<u8>], opened: &Arc<Workbook>) -> Result<(Workbook, Changed)> {
        {
            let mut txn = self.doc.transact_mut();
            for u in updates {
                let update = Update::decode_v1(u).map_err(|e| CollabError::Update(e.to_string()))?;
                txn.apply_update(update).map_err(|e| CollabError::Update(e.to_string()))?;
            }
        }
        // Sheets are matched to the opened file by name the first time.
        self.synced = Some(opened.clone());
        self.ids.clear();
        let (wb, changed) = self.materialize(opened, true)?;
        let wb = Arc::new(wb);
        self.synced = Some(wb.clone());
        Ok((Arc::unwrap_or_clone(wb), Changed { structure: true, ..changed }))
    }

    /// Everything shared so far, as one update (to compact the session's log).
    pub fn state(&self) -> Vec<u8> {
        self.doc.transact().encode_state_as_update_v1(&StateVector::default())
    }

    /// Ends this participant's current undo step.
    pub fn step(&mut self) {
        self.undo.reset();
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    /// The shared id of sheet `index` of the synced workbook.
    pub fn sheet_id(&self, index: usize) -> Option<&str> {
        self.ids.get(index).map(String::as_str)
    }

    /// The index of the sheet with shared id `id`.
    pub fn sheet_index(&self, id: &str) -> Option<usize> {
        self.ids.iter().position(|s| s == id)
    }

    fn new_sheet_id(&mut self) -> String {
        self.next_sheet += 1;
        format!("{:x}-{}", self.client, self.next_sheet)
    }

    /// Shared ids for the sheets of `new`, keeping each sheet's id from `old`: the same sheet
    /// (unchanged, or by name), or the sheet that was at the same place (a rename).
    fn align(&mut self, old: &Workbook, new: &Workbook) -> Vec<String> {
        let mut used = vec![false; old.sheets.len()];
        let mut out: Vec<Option<String>> = vec![None; new.sheets.len()];
        for (i, s) in new.sheets.iter().enumerate() {
            if let Some(j) = old.sheets.iter().enumerate().position(|(j, o)| !used[j] && Arc::ptr_eq(o, s))
                && let Some(id) = self.ids.get(j)
            {
                used[j] = true;
                out[i] = Some(id.clone());
            }
        }
        for (i, s) in new.sheets.iter().enumerate() {
            if out[i].is_some() {
                continue;
            }
            if let Some(j) = old.sheets.iter().enumerate().position(|(j, o)| !used[j] && o.name == s.name)
                && let Some(id) = self.ids.get(j)
            {
                used[j] = true;
                out[i] = Some(id.clone());
            }
        }
        for (i, slot) in out.iter_mut().enumerate() {
            if slot.is_some() {
                continue;
            }
            if old.sheets.len() == new.sheets.len()
                && !used.get(i).copied().unwrap_or(true)
                && let Some(id) = self.ids.get(i)
            {
                used[i] = true;
                *slot = Some(id.clone());
            }
        }
        out.into_iter().map(|s| s.unwrap_or_else(|| self.new_sheet_id())).collect()
    }

    fn put(&mut self, txn: &mut TransactionMut, map: &MapRef, name: &str, key: &str, json: String) {
        let value: Arc<str> = Arc::from(json);
        let seen_key = format!("{name}{SEP}{key}");
        if self.seen.get(&seen_key).is_some_and(|s| *s == value) {
            return;
        }
        map.insert(txn, key, Any::String(value.clone()));
        self.seen.insert(seen_key, value);
    }

    fn delete(&mut self, txn: &mut TransactionMut, map: &MapRef, name: &str, key: &str) {
        if self.seen.remove(&format!("{name}{SEP}{key}")).is_some() || map.get(txn, key).is_some() {
            map.remove(txn, key);
        }
    }

    /// Write what changed since the last sync (one undo step). `None`: nothing to send.
    pub fn local(&mut self, wb: &Arc<Workbook>) -> Option<Vec<u8>> {
        let old = self.synced.clone().unwrap_or_default();
        if Arc::ptr_eq(&old, wb) {
            return None;
        }
        let ids = self.align(&old, wb);
        let before = self.doc.transact().state_vector();
        {
            let doc = self.doc.clone();
            let (book, props, cells) = (self.book.clone(), self.props.clone(), self.cells.clone());
            let mut txn = doc.transact_mut_with(LOCAL);
            if let Ok(order) = serde_json::to_string(&ids) {
                self.put(&mut txn, &book, "book", "order", order);
            }
            for (k, v) in book_fields(wb) {
                self.put(&mut txn, &book, "book", &k, v);
            }
            for (i, sheet) in wb.sheets.iter().enumerate() {
                let Some(id) = ids.get(i) else { continue };
                let prev = self.ids.iter().position(|x| x == id).and_then(|j| old.sheets.get(j));
                if prev.is_some_and(|p| Arc::ptr_eq(p, sheet)) {
                    continue;
                }
                for (k, v) in sheet_fields(sheet) {
                    self.put(&mut txn, &props, "props", &format!("{id}{SEP}{k}"), v);
                }
                let changed: Vec<CellRef> = match prev {
                    Some(p) => p.cells.diff(&sheet.cells),
                    None => sheet.cells.iter().map(|(c, _)| c).collect(),
                };
                for c in changed {
                    let key = format!("{id}{SEP}{}", c.a1());
                    match sheet.cells.get(c).filter(|cell| !cell.is_blank()).and_then(|cell| cell_json(cell, wb)) {
                        Some(j) => self.put(&mut txn, &cells, "cells", &key, j),
                        None => self.delete(&mut txn, &cells, "cells", &key),
                    }
                }
            }
            // Sheets that are gone take their settings and cells with them.
            let gone: HashSet<&String> = self.ids.iter().filter(|id| !ids.contains(id)).collect();
            if !gone.is_empty() {
                let doomed: Vec<(String, String)> = self
                    .seen
                    .keys()
                    .filter_map(|k| {
                        let (map, rest) = split(k)?;
                        let (sheet, _) = split(rest)?;
                        (map != "book" && gone.contains(&sheet.to_string())).then(|| (map.to_string(), rest.to_string()))
                    })
                    .collect();
                for (map, key) in doomed {
                    let m = if map == "props" { props.clone() } else { cells.clone() };
                    self.delete(&mut txn, &m, &map, &key);
                }
            }
        }
        self.ids = ids;
        self.synced = Some(wb.clone());
        self.step();
        let update = self.doc.transact().encode_diff_v1(&before);
        (update.len() > 2).then_some(update)
    }

    /// Merge another participant's update into `current` (local edits not sent yet are written
    /// first and returned, to send).
    pub fn remote(&mut self, update: &[u8], current: &Arc<Workbook>) -> Result<(Arc<Workbook>, Changed, Option<Vec<u8>>)> {
        let pending = self.local(current);
        {
            let update = Update::decode_v1(update).map_err(|e| CollabError::Update(e.to_string()))?;
            let mut txn = self.doc.transact_mut();
            txn.apply_update(update).map_err(|e| CollabError::Update(e.to_string()))?;
        }
        let base = self.synced.clone().unwrap_or_else(|| current.clone());
        let (wb, changed) = self.materialize(&base, false)?;
        let wb = Arc::new(wb);
        self.synced = Some(wb.clone());
        Ok((wb, changed, pending))
    }

    /// The caller recalculated (formula results changed, nothing shared): keep this as synced.
    pub fn settle(&mut self, wb: &Arc<Workbook>) {
        self.synced = Some(wb.clone());
    }

    /// Undo or redo this participant's last step. `None`: nothing to take back.
    pub fn undo(&mut self, current: &Arc<Workbook>, redo: bool) -> Result<Option<(Arc<Workbook>, Changed, Vec<u8>)>> {
        let mut out = Vec::new();
        if let Some(u) = self.local(current) {
            out.push(u);
        }
        self.step();
        let before = self.doc.transact().state_vector();
        let done = if redo { self.undo.redo_blocking() } else { self.undo.undo_blocking() };
        if !done {
            return Ok(None);
        }
        out.push(self.doc.transact().encode_diff_v1(&before));
        let base = self.synced.clone().unwrap_or_else(|| current.clone());
        let (wb, changed) = self.materialize(&base, false)?;
        let wb = Arc::new(wb);
        self.synced = Some(wb.clone());
        Ok(Some((wb, changed, merge(&out))))
    }

    /// The workbook the shared document describes, built on `base` (only what changed is touched).
    /// `by_name`: match `base`'s sheets by name (joining), otherwise by their shared ids.
    fn materialize(&mut self, base: &Arc<Workbook>, by_name: bool) -> Result<(Workbook, Changed)> {
        let doc = self.doc.clone();
        let txn = doc.transact();
        let mut changed = Changed::default();
        let mut touched: HashSet<String> = HashSet::new();

        // The workbook's own fields.
        let mut book_patch: Vec<(String, Json)> = Vec::new();
        let mut order: Vec<String> = Vec::new();
        for (k, v) in self.book.iter(&txn) {
            let Some(s) = text(v) else { continue };
            let key = format!("book{SEP}{k}");
            let same = self.seen.get(&key).is_some_and(|old| Arc::ptr_eq(old, &s) || *old == s);
            if k == "order" {
                order = serde_json::from_str(&s).unwrap_or_default();
            } else if !same && let Ok(value) = serde_json::from_str::<Json>(&s) {
                book_patch.push((k.to_string(), value));
            }
            touched.insert(key.clone());
            self.seen.insert(key, s);
        }
        let mut wb = if book_patch.is_empty() { (**base).clone() } else { patch_book(base, &book_patch)? };
        if !book_patch.is_empty() {
            changed.structure = true;
        }

        // Group the sheets' entries by sheet.
        let mut sheet_props: HashMap<String, Vec<(String, Json)>> = HashMap::new();
        let mut sheet_cells: HashMap<String, Vec<(String, Option<Arc<str>>)>> = HashMap::new();
        for (k, v) in self.props.iter(&txn) {
            let Some(s) = text(v) else { continue };
            let key = format!("props{SEP}{k}");
            let same = self.seen.get(&key).is_some_and(|old| Arc::ptr_eq(old, &s) || *old == s);
            if let Some((sheet, field)) = split(k)
                && (!same || by_name)
                && let Ok(value) = serde_json::from_str::<Json>(&s)
            {
                sheet_props.entry(sheet.to_string()).or_default().push((field.to_string(), value));
            }
            touched.insert(key.clone());
            self.seen.insert(key, s);
        }
        for (k, v) in self.cells.iter(&txn) {
            let Some(s) = text(v) else { continue };
            let key = format!("cells{SEP}{k}");
            let same = self.seen.get(&key).is_some_and(|old| Arc::ptr_eq(old, &s) || *old == s);
            if let Some((sheet, cell)) = split(k)
                && (!same || by_name)
            {
                sheet_cells.entry(sheet.to_string()).or_default().push((cell.to_string(), Some(s.clone())));
            }
            touched.insert(key.clone());
            self.seen.insert(key, s);
        }
        // Entries that disappeared: cleared cells (settings are never removed one by one).
        let gone: Vec<String> = self.seen.keys().filter(|k| !touched.contains(*k)).cloned().collect();
        for k in gone {
            self.seen.remove(&k);
            if let Some(("cells", rest)) = split(&k)
                && let Some((sheet, cell)) = split(rest)
            {
                sheet_cells.entry(sheet.to_string()).or_default().push((cell.to_string(), None));
            }
        }

        // The sheets, in the shared order.
        if order.is_empty() {
            order = self.ids.clone();
        }
        let old_sheets = std::mem::take(&mut wb.sheets);
        let mut sheets: Vec<Arc<Sheet>> = Vec::with_capacity(order.len());
        let mut used = vec![false; old_sheets.len()];
        for (i, id) in order.iter().enumerate() {
            let props = sheet_props.remove(id).unwrap_or_default();
            let found = if by_name {
                let name = props.iter().find(|(k, _)| k == "name").and_then(|(_, v)| v.as_str()).map(str::to_string);
                old_sheets.iter().enumerate().position(|(j, s)| !used[j] && Some(&s.name) == name.as_ref())
            } else {
                self.ids.iter().position(|x| x == id).filter(|j| old_sheets.get(*j).is_some())
            };
            let mut sheet: Arc<Sheet> = match found {
                Some(j) => {
                    used[j] = true;
                    if by_name {
                        // Joining: the shared cells replace the opened file's.
                        let mut s = (*old_sheets[j]).clone();
                        s.cells = CellStore::new();
                        Arc::new(s)
                    } else {
                        old_sheets[j].clone()
                    }
                }
                None => {
                    changed.structure = true;
                    Arc::new(Sheet::new(format!("Sheet{}", i + 1)))
                }
            };
            if !props.is_empty() {
                changed.structure = true;
                sheet = Arc::new(patch_sheet(&sheet, &props)?);
            }
            if let Some(cells) = sheet_cells.remove(id) {
                let s = Arc::make_mut(&mut sheet);
                for (a1, json) in cells {
                    let Some(c) = CellRef::parse(&a1) else { continue };
                    match json.and_then(|j| cell_from(&j, &mut wb)) {
                        Some(cell) => s.cells.set(c, cell),
                        None => {
                            s.cells.remove(c);
                        }
                    }
                    changed.cells.push((i, c));
                }
            }
            sheets.push(sheet);
        }
        if sheets.is_empty() {
            // Never leave a workbook without sheets.
            sheets = old_sheets;
        } else if sheets.len() != old_sheets.len() || sheets.iter().zip(&old_sheets).any(|(a, b)| a.name != b.name) {
            changed.structure = true;
        }
        wb.sheets = sheets;
        if wb.active_sheet >= wb.sheets.len() {
            wb.active_sheet = wb.sheets.len().saturating_sub(1);
        }
        self.ids = if order.is_empty() { self.ids.clone() } else { order };
        Ok((wb, changed))
    }
}

/// Several updates as one.
pub fn merge(updates: &[Vec<u8>]) -> Vec<u8> {
    match updates {
        [one] => one.clone(),
        _ => yrs::merge_updates_v1(updates).unwrap_or_else(|_| updates.concat()),
    }
}

/// Where a participant is: the sheet (shared id) and the selected range, as presence JSON.
pub fn presence(sheet: Option<&str>, active: CellRef, range: RangeRef) -> Json {
    json!({"sheet": sheet, "cell": active.a1(), "range": range.a1()})
}

#[cfg(test)]
mod tests;
