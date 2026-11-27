//! Local, global and weak global reference tables.
//!
//! A reference handed to the guest is `(slot << 2) | kind`, so it is never 0 (null), and a stale
//! or forged value is detected instead of silently naming another object.

use crate::heap::ObjectId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Local = 1,
    Global = 2,
    WeakGlobal = 3,
}

/// Why a reference could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadRef {
    /// Not a value this VM ever produced.
    Invalid(u64),
    /// A reference that was deleted or whose frame was popped.
    Stale(u64),
}

#[derive(Default, Clone)]
struct Slab {
    slots: Vec<Option<ObjectId>>,
    free: Vec<usize>,
}

impl Slab {
    fn add(&mut self, id: ObjectId) -> usize {
        if let Some(slot) = self.free.pop() {
            self.slots[slot] = Some(id);
            slot
        } else {
            self.slots.push(Some(id));
            self.slots.len() - 1
        }
    }

    fn remove(&mut self, slot: usize) -> bool {
        match self.slots.get_mut(slot) {
            Some(entry @ Some(_)) => {
                *entry = None;
                self.free.push(slot);
                true
            }
            _ => false,
        }
    }
}

#[derive(Default, Clone)]
pub struct Refs {
    /// Local references of every frame, innermost last.
    locals: Vec<Option<ObjectId>>,
    /// Start of each local frame in `locals`.
    frames: Vec<usize>,
    globals: Slab,
    weak: Slab,
    /// Largest number of live locals seen, for the summary.
    pub peak_locals: usize,
}

fn encode(slot: usize, kind: RefKind) -> u64 {
    ((slot as u64 + 1) << 2) | kind as u64
}

impl Refs {
    pub fn push_frame(&mut self) {
        self.frames.push(self.locals.len());
    }

    /// Drops the innermost frame and its local references.
    pub fn pop_frame(&mut self) {
        let start = self.frames.pop().unwrap_or(0);
        self.locals.truncate(start);
    }

    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    pub fn new_local(&mut self, id: Option<ObjectId>) -> u64 {
        let Some(id) = id else { return 0 };
        self.locals.push(Some(id));
        self.peak_locals = self.peak_locals.max(self.locals.len());
        encode(self.locals.len() - 1, RefKind::Local)
    }

    pub fn new_global(&mut self, id: Option<ObjectId>) -> u64 {
        id.map_or(0, |id| encode(self.globals.add(id), RefKind::Global))
    }

    pub fn new_weak(&mut self, id: Option<ObjectId>) -> u64 {
        id.map_or(0, |id| encode(self.weak.add(id), RefKind::WeakGlobal))
    }

    pub fn kind(&self, reference: u64) -> Option<RefKind> {
        match reference & 3 {
            1 => Some(RefKind::Local),
            2 => Some(RefKind::Global),
            3 => Some(RefKind::WeakGlobal),
            _ => None,
        }
    }

    /// The object a reference names; `Ok(None)` for null (and for a cleared weak reference).
    pub fn get(&self, reference: u64) -> Result<Option<ObjectId>, BadRef> {
        if reference == 0 {
            return Ok(None);
        }
        let slot = (reference >> 2).checked_sub(1).ok_or(BadRef::Invalid(reference))? as usize;
        let entry = match self.kind(reference) {
            Some(RefKind::Local) => self.locals.get(slot),
            Some(RefKind::Global) => self.globals.slots.get(slot),
            Some(RefKind::WeakGlobal) => return Ok(self.weak.slots.get(slot).copied().flatten()),
            None => return Err(BadRef::Invalid(reference)),
        };
        match entry {
            Some(Some(id)) => Ok(Some(*id)),
            Some(None) => Err(BadRef::Stale(reference)),
            None if reference >> 2 > u32::MAX as u64 => Err(BadRef::Invalid(reference)),
            None => Err(BadRef::Stale(reference)),
        }
    }

    pub fn delete(&mut self, reference: u64) -> bool {
        if reference == 0 {
            return true;
        }
        let Some(slot) = (reference >> 2).checked_sub(1).map(|s| s as usize) else { return false };
        match self.kind(reference) {
            Some(RefKind::Local) => match self.locals.get_mut(slot) {
                Some(entry @ Some(_)) => {
                    *entry = None;
                    true
                }
                _ => false,
            },
            Some(RefKind::Global) => self.globals.remove(slot),
            Some(RefKind::WeakGlobal) => self.weak.remove(slot),
            None => false,
        }
    }

    /// Objects the collector must keep: locals and globals (weak references are not roots).
    pub fn roots(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.locals.iter().chain(self.globals.slots.iter()).flatten().copied()
    }

    /// Clears weak references whose objects are gone.
    pub fn clear_weak(&mut self, alive: impl Fn(ObjectId) -> bool) {
        for slot in self.weak.slots.iter_mut() {
            if slot.is_some_and(|id| !alive(id)) {
                *slot = None;
            }
        }
    }

    pub fn global_count(&self) -> usize {
        self.globals.slots.iter().flatten().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_drop_their_locals() {
        let mut refs = Refs::default();
        refs.push_frame();
        let a = refs.new_local(Some(ObjectId(7)));
        refs.push_frame();
        let b = refs.new_local(Some(ObjectId(8)));
        assert_eq!(refs.get(b), Ok(Some(ObjectId(8))));
        refs.pop_frame();
        assert_eq!(refs.get(b), Err(BadRef::Stale(b)));
        assert_eq!(refs.get(a), Ok(Some(ObjectId(7))));
        assert_eq!(refs.new_local(None), 0);
    }

    #[test]
    fn globals_and_weak() {
        let mut refs = Refs::default();
        let g = refs.new_global(Some(ObjectId(1)));
        let w = refs.new_weak(Some(ObjectId(2)));
        assert_eq!(refs.kind(g), Some(RefKind::Global));
        assert_eq!(refs.get(w), Ok(Some(ObjectId(2))));
        refs.clear_weak(|id| id != ObjectId(2));
        assert_eq!(refs.get(w), Ok(None));
        assert!(refs.delete(g));
        assert_eq!(refs.get(g), Err(BadRef::Stale(g)));
        assert!(!refs.delete(g));
        assert_eq!(refs.get(0x1234_5670), Err(BadRef::Invalid(0x1234_5670)));
    }
}
