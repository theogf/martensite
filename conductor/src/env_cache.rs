// LRU-ish cache (up to 5 entries) for environment fingerprints.

#[derive(Clone, Debug)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug)]
struct Entry {
    fingerprint: u64,
    env: Vec<EnvVar>,
    julia_project: Option<String>,
    access_time: u64,
}

pub struct LookupResult<'a> {
    pub env: &'a [EnvVar],
    pub julia_project: Option<&'a str>,
}

pub struct EnvCache {
    entries: [Option<Entry>; 5],
    counter: u64,
}

impl EnvCache {
    pub fn new() -> Self {
        Self { entries: Default::default(), counter: 0 }
    }

    pub fn lookup(&mut self, fingerprint: u64) -> Option<(&[EnvVar], Option<&str>)> {
        for slot in &mut self.entries {
            if let Some(e) = slot {
                if e.fingerprint == fingerprint {
                    e.access_time = self.counter;
                    self.counter += 1;
                    // Safety: return borrows of the entry's data
                    let e = slot.as_ref().unwrap();
                    return Some((&e.env, e.julia_project.as_deref()));
                }
            }
        }
        None
    }

    // Takes ownership of env; returns borrow of stored data.
    pub fn insert(&mut self, fingerprint: u64, env: Vec<EnvVar>) -> (&[EnvVar], Option<&str>) {
        let julia_project = env.iter()
            .find(|e| e.key == "JULIA_PROJECT")
            .map(|e| e.value.clone());

        let slot_idx = self.eviction_slot();
        self.entries[slot_idx] = Some(Entry {
            fingerprint,
            env,
            julia_project,
            access_time: self.counter,
        });
        self.counter += 1;

        let e = self.entries[slot_idx].as_ref().unwrap();
        (&e.env, e.julia_project.as_deref())
    }

    fn eviction_slot(&self) -> usize {
        // Return empty slot if any, else the slot with the lowest access_time.
        let mut oldest_idx = 0;
        let mut oldest_time = u64::MAX;
        for (i, slot) in self.entries.iter().enumerate() {
            match slot {
                None => return i,
                Some(e) if e.access_time < oldest_time => {
                    oldest_time = e.access_time;
                    oldest_idx = i;
                }
                _ => {}
            }
        }
        oldest_idx
    }
}
