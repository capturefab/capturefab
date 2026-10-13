//! In-memory history. Entries move only after their effects have been restored.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const LIMIT: usize = 200;
const GROUP_GAP: Duration = Duration::from_millis(800);

#[derive(Clone)]
pub(super) struct Entry<T> {
    pub label: String,
    pub change: T,
    key: Option<String>,
    at: Instant,
}

pub(super) struct History<T> {
    undo: VecDeque<Entry<T>>,
    redo: Vec<Entry<T>>,
    grouping: bool,
}

impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            grouping: false,
        }
    }
}

impl<T> History<T> {
    pub fn expand(&mut self, expand: impl Fn(&T) -> Option<Vec<(String, T)>>) {
        fn entries<T>(
            source: impl Iterator<Item = Entry<T>>,
            expand: &impl Fn(&T) -> Option<Vec<(String, T)>>,
        ) -> Vec<Entry<T>> {
            source
                .flat_map(|entry| {
                    if let Some(changes) = expand(&entry.change) {
                        changes
                            .into_iter()
                            .map(|(label, change)| Entry {
                                label,
                                change,
                                key: None,
                                at: entry.at,
                            })
                            .collect()
                    } else {
                        vec![entry]
                    }
                })
                .collect()
        }
        let changed = std::cell::Cell::new(false);
        let tracked = |change: &T| {
            let result = expand(change);
            if result.is_some() {
                changed.set(true);
            }
            result
        };
        self.undo = entries(std::mem::take(&mut self.undo).into_iter(), &tracked).into();
        self.redo = entries(std::mem::take(&mut self.redo).into_iter(), &tracked);
        self.trim();
        if changed.get() {
            self.break_group();
        }
    }
    pub fn undo(&self) -> Option<&Entry<T>> {
        self.undo.back()
    }
    pub fn redo(&self) -> Option<&Entry<T>> {
        self.redo.last()
    }
    pub fn contains(&self, matches: impl Fn(&T) -> bool) -> bool {
        self.undo
            .iter()
            .chain(self.redo.iter())
            .any(|entry| matches(&entry.change))
    }
    pub fn find(&self, matches: impl Fn(&T) -> bool) -> Option<&T> {
        self.undo
            .iter()
            .chain(self.redo.iter())
            .find(|entry| matches(&entry.change))
            .map(|entry| &entry.change)
    }
    fn trim(&mut self) {
        while self.undo.len() + self.redo.len() > LIMIT {
            let oldest_redo = self
                .redo
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.at);
            if self
                .undo
                .front()
                .is_some_and(|entry| oldest_redo.is_none_or(|(_, redo)| entry.at <= redo.at))
            {
                self.undo.pop_front();
            } else if let Some((index, _)) = oldest_redo {
                self.redo.remove(index);
            }
        }
    }
    fn insert_undo(&mut self, entry: Entry<T>) {
        let position = self
            .undo
            .iter()
            .position(|old| old.at > entry.at)
            .unwrap_or(self.undo.len());
        self.undo.insert(position, entry);
        self.trim();
    }
    fn insert_redo(&mut self, entry: Entry<T>) {
        let position = self
            .redo
            .iter()
            .position(|old| old.at < entry.at)
            .unwrap_or(self.redo.len());
        self.redo.insert(position, entry);
        self.trim();
    }
    pub fn import_redo(&mut self, label: String, change: T, at: Instant) {
        self.insert_redo(Entry {
            label,
            change,
            key: None,
            at,
        });
        self.break_group();
    }
    pub fn synchronize(&mut self, matches: impl Fn(&T) -> bool, applied: bool, at: Instant) {
        if applied {
            if let Some(index) = self.redo.iter().position(|entry| matches(&entry.change)) {
                let mut entry = self.redo.remove(index);
                entry.at = at;
                self.insert_undo(entry);
                self.break_group();
            } else if let Some(index) = self.undo.iter().position(|entry| matches(&entry.change)) {
                let mut entry = self.undo.remove(index).expect("entry exists");
                entry.at = at;
                self.insert_undo(entry);
                self.break_group();
            }
        } else if let Some(index) = self.undo.iter().position(|entry| matches(&entry.change)) {
            let mut entry = self.undo.remove(index).expect("entry exists");
            entry.at = at;
            self.redo.push(entry);
            self.break_group();
        } else if let Some(index) = self.redo.iter().position(|entry| matches(&entry.change)) {
            let mut entry = self.redo.remove(index);
            entry.at = at;
            self.redo.push(entry);
            self.break_group();
        }
    }
    pub fn break_group(&mut self) {
        self.grouping = false;
    }
    pub fn retain(&mut self, keep: impl Fn(&T) -> bool) {
        let count = self.undo.len() + self.redo.len();
        self.undo.retain(|entry| keep(&entry.change));
        self.redo.retain(|entry| keep(&entry.change));
        if count != self.undo.len() + self.redo.len() {
            self.break_group();
        }
    }

    pub fn record(
        &mut self,
        label: String,
        key: Option<String>,
        change: T,
        at: Instant,
        merge: impl FnOnce(&mut T, &T) -> bool,
    ) {
        self.redo.clear();
        if self.grouping
            && key.is_some()
            && let Some(last) = self.undo.back_mut()
            && last.key == key
            && (key.as_deref() == Some("focus-region")
                || at.saturating_duration_since(last.at) <= GROUP_GAP)
            && merge(&mut last.change, &change)
        {
            last.at = at;
            return;
        }
        self.insert_undo(Entry {
            label,
            key,
            change,
            at,
        });
        self.grouping = true;
    }

    pub fn committed(&mut self, undo: bool) {
        self.break_group();
        if undo {
            if let Some(entry) = self.undo.pop_back() {
                self.redo.push(entry);
            }
        } else if let Some(entry) = self.redo.pop() {
            self.undo.push_back(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coalesces_typing_branches_and_bounds_history() {
        let mut h = History::default();
        let now = Instant::now();
        let record = |h: &mut History<(u32, u32)>, key, a, b| {
            h.record("Edit".into(), key, (a, b), now, |old, new| {
                old.1 = new.1;
                true
            })
        };
        record(&mut h, Some("field".into()), 0, 1);
        record(&mut h, Some("field".into()), 1, 2);
        assert_eq!(h.undo().unwrap().change, (0, 2));
        h.committed(true);
        assert!(h.undo().is_none());
        assert_eq!(h.redo().unwrap().change, (0, 2));
        record(&mut h, None, 0, 3);
        assert!(h.redo().is_none());
        for i in 0..300 {
            record(&mut h, None, i, i + 1);
        }
        assert_eq!(h.undo.len(), LIMIT);
        // Looking at or unsuccessfully replaying a change leaves it available.
        assert_eq!(h.undo().unwrap().change, (299, 300));
        assert_eq!(h.undo().unwrap().change, (299, 300));
    }
    #[test]
    fn undo_and_unrelated_gestures_end_typing_groups() {
        let mut h = History::default();
        let now = Instant::now();
        h.record("Edit".into(), Some("a".into()), 1, now, |_, _| false);
        h.break_group();
        h.record("Edit".into(), Some("a".into()), 2, now, |_, _| true);
        assert_eq!(h.undo.len(), 2);
        h.committed(true);
        h.committed(false);
        h.record("Edit".into(), Some("a".into()), 3, now, |_, _| true);
        assert_eq!(h.undo.len(), 3);
    }

    #[test]
    fn focus_drag_keeps_one_entry_until_the_next_press_even_after_a_pause() {
        let mut h = History::default();
        let now = Instant::now();
        h.record(
            "Move region".into(),
            Some("focus-region".into()),
            (0, 1),
            now,
            |_, _| false,
        );
        h.record(
            "Move region".into(),
            Some("focus-region".into()),
            (1, 2),
            now + Duration::from_secs(10),
            |old, new| {
                old.1 = new.1;
                true
            },
        );
        assert_eq!(h.undo.len(), 1);
        assert_eq!(h.undo().unwrap().change, (0, 2));
        h.break_group();
        h.record(
            "Move region".into(),
            Some("focus-region".into()),
            (2, 3),
            now + Duration::from_secs(11),
            |old, new| {
                old.1 = new.1;
                true
            },
        );
        assert_eq!(h.undo.len(), 2);
    }
    #[test]
    fn external_undo_order_and_late_acknowledgments_follow_the_user() {
        let mut h = History::default();
        let now = Instant::now();
        h.record(
            "Later local".into(),
            None,
            2,
            now + Duration::from_millis(2),
            |_, _| false,
        );
        h.record("Earlier hardware".into(), None, 1, now, |_, _| false);
        assert_eq!(h.undo().unwrap().change, 2);
        h.synchronize(|change| *change == 1, false, now + Duration::from_millis(3));
        h.synchronize(|change| *change == 2, false, now + Duration::from_millis(4));
        assert_eq!(h.redo().unwrap().change, 2);
        h.committed(false);
        assert_eq!(h.redo().unwrap().change, 1);
        assert_eq!(h.undo().unwrap().change, 2);
    }
}
