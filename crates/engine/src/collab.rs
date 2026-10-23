//! Live co-editing through [`gridcraft_collab`]: the session sends what this person changed and
//! takes in what the others changed, recalculating only what that touched. Undo and redo take back
//! only this person's own steps while editing together.

use std::sync::Arc;

use gridcraft_collab::{Changed, Collab};
use gridcraft_model::Workbook;
use serde_json::Value as Json;

use crate::Session;

/// The live session of the active workbook.
#[derive(Debug)]
pub struct Live {
    inner: Collab,
    /// Changes merged before sending (written while taking in someone else's).
    pending: Vec<Vec<u8>>,
}

impl Session {
    /// Whether the active workbook is being edited together with others.
    pub fn is_collab(&self) -> bool {
        self.collab.is_some()
    }

    /// The first participant: fill the shared workbook with the open one. Returns the update to send.
    pub fn collab_seed(&mut self, client: u64) -> Vec<u8> {
        let mut inner = Collab::new(client);
        let wb = self.active().map(|d| d.wb.clone()).unwrap_or_default();
        let update = inner.seed(&wb);
        self.collab = Some(Box::new(Live { inner, pending: Vec::new() }));
        update
    }

    /// Join: build the workbook from everything shared so far (on top of the opened file).
    pub fn collab_join(&mut self, client: u64, log: &[Vec<u8>]) -> Result<(), String> {
        let mut inner = Collab::new(client);
        let opened = self.active().map(|d| d.wb.clone()).unwrap_or_default();
        let (wb, changed) = inner.join(log, &opened).map_err(|e| e.to_string())?;
        self.collab = Some(Box::new(Live { inner, pending: Vec::new() }));
        self.adopt(Arc::new(wb), &changed);
        if let Some(d) = self.active_mut() {
            // The joined workbook is the starting point: nothing to save or undo yet.
            d.saved = d.wb.clone();
            d.undo.clear();
            d.redo.clear();
        }
        Ok(())
    }

    /// This person's changes since the last call, to send (`None`: nothing changed).
    pub fn collab_outgoing(&mut self) -> Option<Vec<u8>> {
        let wb = self.active().map(|d| d.wb.clone())?;
        let live = self.collab.as_mut()?;
        if let Some(u) = live.inner.local(&wb) {
            live.pending.push(u);
        }
        if live.pending.is_empty() {
            return None;
        }
        Some(gridcraft_collab::merge(&std::mem::take(&mut live.pending)))
    }

    /// Take in someone else's change.
    pub fn collab_incoming(&mut self, update: &[u8]) -> Result<(), String> {
        let Some(wb) = self.active().map(|d| d.wb.clone()) else { return Ok(()) };
        let Some(live) = self.collab.as_mut() else { return Ok(()) };
        let (next, changed, pending) = live.inner.remote(update, &wb).map_err(|e| e.to_string())?;
        if let Some(p) = pending {
            live.pending.push(p);
        }
        self.adopt(next, &changed);
        Ok(())
    }

    /// Everything shared so far as one update (to compact the session's log).
    pub fn collab_state(&self) -> Option<Vec<u8>> {
        self.collab.as_ref().map(|l| l.inner.state())
    }

    /// Where this person is (sheet and selection), for the others.
    pub fn collab_presence(&self) -> Option<Json> {
        let live = self.collab.as_ref()?;
        let d = self.active()?;
        let sel = &d.selection;
        let range = sel.ranges.last().copied().unwrap_or_else(|| gridcraft_core::RangeRef::new(sel.active, sel.active));
        Some(gridcraft_collab::presence(live.inner.sheet_id(d.wb.active_sheet), sel.active, range))
    }

    /// The sheet with the shared id `id` (someone else's presence).
    pub fn collab_sheet_index(&self, id: &str) -> Option<usize> {
        self.collab.as_ref()?.inner.sheet_index(id)
    }

    /// Undo or redo this person's own last step while editing together.
    pub(crate) fn collab_undo(&mut self, redo: bool) -> crate::Result<Json> {
        let Some(wb) = self.active().map(|d| d.wb.clone()) else { return Ok(Json::Null) };
        let Some(live) = self.collab.as_mut() else { return Ok(Json::Null) };
        match live.inner.undo(&wb, redo) {
            Ok(Some((next, changed, update))) => {
                live.pending.push(update);
                self.adopt(next, &changed);
                Ok(serde_json::json!({ if redo { "redone" } else { "undone" }: "" }))
            }
            Ok(None) => Ok(Json::Null),
            Err(e) => Err(crate::EngineError::Internal(if redo { "edit.redo" } else { "edit.undo" }.into(), e.to_string())),
        }
    }

    /// Show a merged workbook: keep this person's view, recalculate what changed.
    fn adopt(&mut self, next: Arc<Workbook>, changed: &Changed) {
        let Some(d) = self.active_mut() else { return };
        let mut wb = next;
        {
            let w = Arc::make_mut(&mut wb);
            w.active_sheet = d.wb.active_sheet.min(w.sheets.len().saturating_sub(1));
            if changed.structure {
                d.calc.recalc_all(w);
            } else if !changed.cells.is_empty() {
                d.calc.cells_changed(w, &changed.cells);
            }
        }
        d.wb = wb;
        d.revision += 1;
        let wb = d.wb.clone();
        if let Some(live) = self.collab.as_mut() {
            // Recalculated results are not shared: what is shown now counts as in sync.
            live.inner.settle(&wb);
        }
    }
}
