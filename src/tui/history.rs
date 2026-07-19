//! Reusable back/forward navigation history for interactive tools.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationHistory<T> {
    entries: Vec<T>,
    cursor: Option<usize>,
}

impl<T> Default for NavigationHistory<T> {
    fn default() -> Self {
        Self { entries: Vec::new(), cursor: None }
    }
}

impl<T: Eq> NavigationHistory<T> {
    pub fn visit(&mut self, entry: T) {
        if self.current() == Some(&entry) {
            return;
        }
        let keep = self.cursor.map_or(0, |cursor| cursor + 1);
        self.entries.truncate(keep);
        self.entries.push(entry);
        self.cursor = Some(self.entries.len() - 1);
    }
}

impl<T> NavigationHistory<T> {
    pub fn current(&self) -> Option<&T> {
        self.cursor.and_then(|cursor| self.entries.get(cursor))
    }

    pub fn target(&self, delta: isize) -> Option<(usize, &T)> {
        let cursor = self.cursor?;
        let target = cursor.checked_add_signed(delta)?;
        self.entries.get(target).map(|entry| (target, entry))
    }

    pub fn select(&mut self, cursor: usize) {
        debug_assert!(cursor < self.entries.len());
        self.cursor = Some(cursor);
    }

    pub fn replace_current(&mut self, entry: T) {
        match self.cursor.and_then(|cursor| self.entries.get_mut(cursor)) {
            Some(current) => *current = entry,
            None => {
                self.entries.push(entry);
                self.cursor = Some(0);
            }
        }
    }

    pub fn entries(&self) -> &[T] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visiting_after_back_truncates_the_forward_branch() {
        let mut history = NavigationHistory::default();
        history.visit("a");
        history.visit("b");
        history.visit("c");
        let (cursor, _) = history.target(-1).unwrap();
        history.select(cursor);

        history.visit("d");

        assert_eq!(history.entries(), ["a", "b", "d"]);
        assert!(history.target(1).is_none());
    }

    #[test]
    fn replacing_current_preserves_back_and_forward_neighbors() {
        let mut history = NavigationHistory::default();
        history.visit("a");
        history.visit("b");
        history.visit("c");
        let (cursor, _) = history.target(-1).unwrap();
        history.select(cursor);

        history.replace_current("updated-b");

        assert_eq!(history.entries(), ["a", "updated-b", "c"]);
        assert_eq!(history.target(-1).map(|(_, entry)| *entry), Some("a"));
        assert_eq!(history.target(1).map(|(_, entry)| *entry), Some("c"));
    }
}
