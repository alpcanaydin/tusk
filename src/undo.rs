//! Undo / redo of pending (unsaved) changes: every mutation records the
//! state it replaced; ⌘Z steps back, ⇧⌘Z forward. Text inputs keep their own
//! text undo — this covers grid cells, deleted rows, structure edits and the
//! sidebar's pending drops / renames.

/// Snapshots before each change (`past`) and after undone ones (`future`).
#[derive(Default)]
pub struct History<T> {
    past: Vec<T>,
    future: Vec<T>,
}

const LIMIT: usize = 200;

impl<T: Clone + PartialEq> History<T> {
    /// Record a change from `before` to `after` (no-op when nothing changed).
    pub fn record(&mut self, before: T, after: &T) {
        if before == *after {
            return;
        }
        self.past.push(before);
        if self.past.len() > LIMIT {
            self.past.remove(0);
        }
        self.future.clear();
    }

    /// The state to go back to; `current` becomes redoable.
    pub fn undo(&mut self, current: T) -> Option<T> {
        let prev = self.past.pop()?;
        self.future.push(current);
        Some(prev)
    }

    pub fn redo(&mut self, current: T) -> Option<T> {
        let next = self.future.pop()?;
        self.past.push(current);
        Some(next)
    }

    pub fn clear(&mut self) {
        self.past.clear();
        self.future.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::History;

    #[test]
    fn undo_redo_walks_the_changes() {
        let mut h = History::default();
        let mut v = 0;
        for next in [1, 2, 3] {
            h.record(v, &next);
            v = next;
        }
        h.record(v, &v); // no-op
        v = h.undo(v).unwrap();
        assert_eq!(v, 2);
        v = h.undo(v).unwrap();
        assert_eq!(v, 1);
        v = h.redo(v).unwrap();
        assert_eq!(v, 2);
        h.record(v, &9); // a new change drops the redo branch
        v = 9;
        assert!(h.redo(v).is_none());
        assert_eq!(h.undo(v), Some(2));
    }
}
