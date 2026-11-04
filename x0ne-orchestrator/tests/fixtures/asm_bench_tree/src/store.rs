use std::collections::BTreeMap;

#[derive(Default)]
pub struct Store {
    items: BTreeMap<u64, String>,
    next: u64,
}

impl Store {
    pub fn insert(&mut self, value: String) -> u64 {
        let id = self.next;
        self.next += 1;
        self.items.insert(id, value);
        id
    }

    pub fn get(&self, id: u64) -> Option<&String> {
        self.items.get(&id)
    }

    pub fn remove(&mut self, id: u64) -> String {
        self.items.remove(&id).unwrap()
    }

    // TODO: iteration order guarantees
    pub fn len(&self) -> usize {
        self.items.len()
    }
}
