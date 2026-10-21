//! Ordered sessions with identities that never change when a neighbor closes.
use std::collections::BTreeMap;
use std::ops::{Index, IndexMut};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TabId(u64);
pub(crate) struct Tabs<T> {
    entries: BTreeMap<TabId, T>,
    next: u64,
}
impl<T> Tabs<T> {
    pub(crate) fn new(first: T) -> (Self, TabId) {
        let mut tabs = Self {
            entries: BTreeMap::new(),
            next: 0,
        };
        let id = tabs.insert(first);
        (tabs, id)
    }
    pub(crate) fn insert(&mut self, tab: T) -> TabId {
        let id = TabId(self.next);
        self.next = self.next.checked_add(1).expect("tab identity exhausted");
        self.entries.insert(id, tab);
        id
    }
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    #[cfg(test)]
    pub(crate) fn id_at(&self, index: usize) -> Option<TabId> {
        self.entries.keys().nth(index).copied()
    }
    #[cfg(test)]
    pub(crate) fn position(&self, id: TabId) -> usize {
        self.entries
            .keys()
            .position(|&key| key == id)
            .expect("live tab")
    }
    pub(crate) fn remove(&mut self, id: TabId) -> Option<T> {
        self.entries.remove(&id)
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = (TabId, &T)> {
        self.entries.iter().map(|(&id, tab)| (id, tab))
    }
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (TabId, &mut T)> {
        self.entries.iter_mut().map(|(&id, tab)| (id, tab))
    }
}
impl<T> Index<TabId> for Tabs<T> {
    type Output = T;
    fn index(&self, id: TabId) -> &T {
        &self.entries[&id]
    }
}
impl<T> IndexMut<TabId> for Tabs<T> {
    fn index_mut(&mut self, id: TabId) -> &mut T {
        self.entries.get_mut(&id).expect("live tab")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removing_background_or_active_neighbor_preserves_identity() {
        let (mut tabs, a) = Tabs::new("a");
        let b = tabs.insert("b");
        let c = tabs.insert("c");
        tabs.remove(b);
        assert_eq!(tabs.id_at(1), Some(c));
        assert_eq!(tabs[c], "c");
        tabs.remove(a);
        assert_eq!(tabs.position(c), 0);
        let d = tabs.insert("d");
        assert!(d > c);
        assert_eq!(tabs[c], "c");
    }
}
