//! Objects, arrays, strings and classes, with a mark-and-sweep collector.

use std::any::Any;
use std::collections::HashMap;

use crate::value::Value;

/// Index of an object in the heap. Stable for the object's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub u32);

/// Index of a class in the class table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClassId(pub u32);

#[derive(Debug, Clone, PartialEq)]
pub enum Array {
    Boolean(Vec<u8>),
    Byte(Vec<i8>),
    Char(Vec<u16>),
    Short(Vec<i16>),
    Int(Vec<i32>),
    Long(Vec<i64>),
    Float(Vec<f32>),
    Double(Vec<f64>),
    Object(Vec<Option<ObjectId>>),
}

impl Array {
    pub fn len(&self) -> usize {
        match self {
            Array::Boolean(v) => v.len(),
            Array::Byte(v) => v.len(),
            Array::Char(v) => v.len(),
            Array::Short(v) => v.len(),
            Array::Int(v) => v.len(),
            Array::Long(v) => v.len(),
            Array::Float(v) => v.len(),
            Array::Double(v) => v.len(),
            Array::Object(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes per element as JNI lays them out.
    pub fn element_size(&self) -> usize {
        match self {
            Array::Boolean(_) | Array::Byte(_) => 1,
            Array::Char(_) | Array::Short(_) => 2,
            Array::Int(_) | Array::Float(_) => 4,
            Array::Long(_) | Array::Double(_) => 8,
            Array::Object(_) => 0,
        }
    }

    /// Little-endian bytes of elements `start..start + count` of a primitive array.
    pub fn bytes(&self, start: usize, count: usize) -> Option<Vec<u8>> {
        let end = start.checked_add(count)?;
        if end > self.len() {
            return None;
        }
        Some(match self {
            Array::Boolean(v) => v[start..end].to_vec(),
            Array::Byte(v) => v[start..end].iter().map(|&b| b as u8).collect(),
            Array::Char(v) => v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
            Array::Short(v) => v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
            Array::Int(v) => v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
            Array::Long(v) => v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
            Array::Float(v) => v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
            Array::Double(v) => v[start..end].iter().flat_map(|x| x.to_le_bytes()).collect(),
            Array::Object(_) => return None,
        })
    }

    /// Overwrites elements from `start` with little-endian `bytes`. False if out of bounds.
    pub fn set_bytes(&mut self, start: usize, bytes: &[u8]) -> bool {
        let size = self.element_size();
        if size == 0 || !bytes.len().is_multiple_of(size) {
            return false;
        }
        let count = bytes.len() / size;
        let Some(end) = start.checked_add(count) else { return false };
        if end > self.len() {
            return false;
        }
        fn fill<T, const N: usize>(dst: &mut [T], src: &[u8], f: fn([u8; N]) -> T) {
            for (d, chunk) in dst.iter_mut().zip(src.chunks_exact(N)) {
                *d = f(chunk.try_into().expect("chunk size"));
            }
        }
        match self {
            Array::Boolean(v) => v[start..end].copy_from_slice(bytes),
            Array::Byte(v) => fill(&mut v[start..end], bytes, |b: [u8; 1]| b[0] as i8),
            Array::Char(v) => fill(&mut v[start..end], bytes, u16::from_le_bytes),
            Array::Short(v) => fill(&mut v[start..end], bytes, i16::from_le_bytes),
            Array::Int(v) => fill(&mut v[start..end], bytes, i32::from_le_bytes),
            Array::Long(v) => fill(&mut v[start..end], bytes, i64::from_le_bytes),
            Array::Float(v) => fill(&mut v[start..end], bytes, f32::from_le_bytes),
            Array::Double(v) => fill(&mut v[start..end], bytes, f64::from_le_bytes),
            Array::Object(_) => return false,
        }
        true
    }

    /// The descriptor of this array's component type, for primitive arrays.
    pub fn primitive_descriptor(&self) -> Option<&'static str> {
        Some(match self {
            Array::Boolean(_) => "[Z",
            Array::Byte(_) => "[B",
            Array::Char(_) => "[C",
            Array::Short(_) => "[S",
            Array::Int(_) => "[I",
            Array::Long(_) => "[J",
            Array::Float(_) => "[F",
            Array::Double(_) => "[D",
            Array::Object(_) => return None,
        })
    }
}

/// What an object holds besides its fields.
pub enum Data {
    Plain,
    /// java.lang.String, UTF-16 code units (lone surrogates are kept).
    String(Vec<u16>),
    Array(Array),
    /// The java.lang.Class object of a class.
    Class(ClassId),
    /// A host value attached by a model or the user (for example the entries of a HashMap).
    Host(Box<dyn Any + Send>),
}

impl std::fmt::Debug for Data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Data::Plain => write!(f, "Plain"),
            Data::String(s) => write!(f, "String({:?})", String::from_utf16_lossy(s)),
            Data::Array(a) => write!(f, "Array(len {})", a.len()),
            Data::Class(c) => write!(f, "Class({})", c.0),
            Data::Host(_) => write!(f, "Host(..)"),
        }
    }
}

#[derive(Debug)]
pub struct Object {
    pub class: ClassId,
    pub data: Data,
    /// Instance fields by name, set through SetXxxField or by the host.
    pub fields: HashMap<String, Value>,
}

/// Objects that reference other objects, for the collector. Host data is opaque; a model that
/// keeps objects inside `Data::Host` registers them as children through `Heap::retain_in`.
#[derive(Default)]
pub struct Heap {
    slots: Vec<Option<Object>>,
    free: Vec<u32>,
    live: usize,
    /// Objects kept alive by a host value inside another object.
    host_children: HashMap<ObjectId, Vec<ObjectId>>,
}

impl Heap {
    pub fn alloc(&mut self, object: Object) -> ObjectId {
        self.live += 1;
        if let Some(index) = self.free.pop() {
            self.slots[index as usize] = Some(object);
            ObjectId(index)
        } else {
            self.slots.push(Some(object));
            ObjectId(self.slots.len() as u32 - 1)
        }
    }

    pub fn get(&self, id: ObjectId) -> Option<&Object> {
        self.slots.get(id.0 as usize).and_then(Option::as_ref)
    }

    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        self.slots.get_mut(id.0 as usize).and_then(Option::as_mut)
    }

    pub fn live(&self) -> usize {
        self.live
    }

    /// Declares that `owner`'s host data keeps `children` alive (replacing earlier children).
    pub fn retain_in(&mut self, owner: ObjectId, children: Vec<ObjectId>) {
        if children.is_empty() {
            self.host_children.remove(&owner);
        } else {
            self.host_children.insert(owner, children);
        }
    }

    /// Frees every object not reachable from `roots`. Returns how many were freed.
    pub fn collect(&mut self, roots: impl IntoIterator<Item = ObjectId>) -> usize {
        let mut marked = vec![false; self.slots.len()];
        let mut stack: Vec<ObjectId> = roots.into_iter().collect();
        while let Some(id) = stack.pop() {
            let index = id.0 as usize;
            if index >= marked.len() || marked[index] {
                continue;
            }
            let Some(object) = self.slots[index].as_ref() else { continue };
            marked[index] = true;
            for value in object.fields.values() {
                if let Value::Object(child) = value {
                    stack.push(*child);
                }
            }
            if let Data::Array(Array::Object(items)) = &object.data {
                stack.extend(items.iter().flatten());
            }
            if let Some(children) = self.host_children.get(&id) {
                stack.extend(children.iter().copied());
            }
        }
        let mut freed = 0;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_some() && !marked[index] {
                *slot = None;
                self.free.push(index as u32);
                self.host_children.remove(&ObjectId(index as u32));
                freed += 1;
            }
        }
        self.live -= freed;
        freed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(heap: &mut Heap) -> ObjectId {
        heap.alloc(Object { class: ClassId(0), data: Data::Plain, fields: HashMap::new() })
    }

    #[test]
    fn collector_keeps_reachable_objects() {
        let mut heap = Heap::default();
        let root = plain(&mut heap);
        let child = plain(&mut heap);
        let in_array = plain(&mut heap);
        let held = plain(&mut heap);
        let garbage = plain(&mut heap);
        let array = heap.alloc(Object { class: ClassId(0), data: Data::Array(Array::Object(vec![Some(in_array), None])), fields: HashMap::new() });
        heap.get_mut(root).unwrap().fields.insert("child".into(), Value::Object(child));
        heap.get_mut(child).unwrap().fields.insert("array".into(), Value::Object(array));
        heap.retain_in(array, vec![held]);
        assert_eq!(heap.collect([root]), 1);
        assert!(heap.get(garbage).is_none());
        for id in [root, child, in_array, held, array] {
            assert!(heap.get(id).is_some());
        }
        let reused = plain(&mut heap);
        assert_eq!(reused, garbage);
    }

    #[test]
    fn array_bytes_round_trip() {
        let mut a = Array::Int(vec![0; 4]);
        assert!(a.set_bytes(1, &[1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]));
        assert_eq!(a, Array::Int(vec![0, 1, -1, 0]));
        assert_eq!(a.bytes(1, 2).unwrap(), vec![1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        assert!(a.bytes(3, 2).is_none());
        assert!(!a.set_bytes(3, &[0; 8]));
    }
}
