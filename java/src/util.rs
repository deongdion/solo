//! java.util collections: HashMap, LinkedHashMap, TreeMap, HashSet, ArrayList, their key, value
//! and entry views, iterators, and Map.Entry (PLAN.md F11).
//!
//! HashMap and HashSet iterate in the order OpenJDK's HashMap does (bucket by spread hash, the
//! table doubling at 0.75 load, insertion order within a bucket), so native code that walks a map
//! sees the order it sees on a device. Views are snapshots taken when `keySet()`, `values()` or
//! `entrySet()` is called.

use std::cmp::Ordering;

use javavm::{Array, Call, CallKind, Data, JResult, ObjectId, Value, Vm};

use crate::lang::{java_equals, java_hash, unbox};
use crate::{object_arg, throw};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapKind {
    /// HashMap / HashSet order.
    Hash,
    /// LinkedHashMap / LinkedHashSet: insertion order.
    Linked,
    /// TreeMap / TreeSet: natural order of the keys.
    Tree,
}

#[derive(Debug, Clone)]
struct Entry {
    key: Option<ObjectId>,
    value: Option<ObjectId>,
    hash: i32,
    seq: u64,
}

/// Host data of a map, or of a set (a map whose values are unused).
pub(crate) struct MapData {
    kind: MapKind,
    set: bool,
    entries: Vec<Entry>,
    /// HashMap table size: 0 until the first put, then 16, doubling past 0.75 load.
    capacity: usize,
    next_seq: u64,
}

/// Host data of a list or a view snapshot.
pub(crate) struct Seq {
    items: Vec<Option<ObjectId>>,
    /// Lists accept `get(int)` and `add`; views do not.
    list: bool,
}

pub(crate) struct Iter {
    items: Vec<Option<ObjectId>>,
    next: usize,
}

pub(crate) struct EntryData {
    key: Option<ObjectId>,
    value: Option<ObjectId>,
}

fn spread(h: i32) -> i32 {
    h ^ ((h as u32) >> 16) as i32
}

fn compare(vm: &Vm, a: Option<ObjectId>, b: Option<ObjectId>) -> Ordering {
    let (Some(a), Some(b)) = (a, b) else { return a.cmp(&b) };
    if let (Some(x), Some(y)) = (vm.string_units(a), vm.string_units(b)) {
        return x.cmp(y);
    }
    match (unbox(vm, a), unbox(vm, b)) {
        (Some(x), Some(y)) => x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(Ordering::Equal).then(x.as_i64().cmp(&y.as_i64())),
        _ => a.cmp(&b),
    }
}

impl MapData {
    fn new(kind: MapKind, set: bool) -> MapData {
        MapData { kind, set, entries: Vec::new(), capacity: 0, next_seq: 0 }
    }

    fn position(&self, vm: &Vm, key: Option<ObjectId>) -> Option<usize> {
        let hash = spread(java_hash(vm, key));
        self.entries.iter().position(|e| e.hash == hash && java_equals(vm, e.key, key))
    }

    /// Returns the previous value.
    fn put(&mut self, vm: &Vm, key: Option<ObjectId>, value: Option<ObjectId>) -> Option<Option<ObjectId>> {
        if let Some(i) = self.position(vm, key) {
            return Some(std::mem::replace(&mut self.entries[i].value, value));
        }
        let hash = spread(java_hash(vm, key));
        self.entries.push(Entry { key, value, hash, seq: self.next_seq });
        self.next_seq += 1;
        if self.capacity == 0 {
            self.capacity = 16;
        }
        if self.entries.len() > self.capacity * 3 / 4 {
            self.capacity *= 2;
        }
        None
    }

    fn remove(&mut self, vm: &Vm, key: Option<ObjectId>) -> Option<Entry> {
        let i = self.position(vm, key)?;
        Some(self.entries.remove(i))
    }

    /// Entries in iteration order.
    fn ordered(&self, vm: &Vm) -> Vec<Entry> {
        let mut entries = self.entries.clone();
        match self.kind {
            MapKind::Linked => {}
            MapKind::Hash => {
                let mask = self.capacity.max(1) as i64 - 1;
                entries.sort_by_key(|e| ((e.hash as u32 as i64) & mask, e.seq));
            }
            MapKind::Tree => entries.sort_by(|a, b| compare(vm, a.key, b.key)),
        }
        entries
    }

    fn children(&self) -> Vec<ObjectId> {
        self.entries.iter().flat_map(|e| [e.key, e.value]).flatten().collect()
    }
}

fn map_class(kind: MapKind, set: bool) -> &'static str {
    match (kind, set) {
        (MapKind::Hash, false) => "java/util/HashMap",
        (MapKind::Linked, false) => "java/util/LinkedHashMap",
        (MapKind::Tree, false) => "java/util/TreeMap",
        (MapKind::Hash, true) => "java/util/HashSet",
        (MapKind::Linked, true) => "java/util/LinkedHashSet",
        (MapKind::Tree, true) => "java/util/TreeSet",
    }
}

fn sync_map(vm: &mut Vm, id: ObjectId) {
    let children = vm.host_data::<MapData>(id).map(MapData::children).unwrap_or_default();
    vm.heap.retain_in(id, children);
}

fn new_seq(vm: &mut Vm, class: &str, items: Vec<Option<ObjectId>>, list: bool) -> ObjectId {
    let children: Vec<ObjectId> = items.iter().flatten().copied().collect();
    let id = vm.new_host_object(class, Box::new(Seq { items, list }));
    vm.heap.retain_in(id, children);
    id
}

fn new_iter(vm: &mut Vm, items: Vec<Option<ObjectId>>) -> ObjectId {
    let children: Vec<ObjectId> = items.iter().flatten().copied().collect();
    let id = vm.new_host_object("java/util/HashMap$HashIterator", Box::new(Iter { items, next: 0 }));
    vm.heap.retain_in(id, children);
    id
}

fn new_entry(vm: &mut Vm, key: Option<ObjectId>, value: Option<ObjectId>) -> ObjectId {
    let id = vm.new_host_object("java/util/HashMap$Node", Box::new(EntryData { key, value }));
    vm.heap.retain_in(id, [key, value].into_iter().flatten().collect());
    id
}

/// A map of `kind` filled by putting `entries` in order (values are boxed as needed).
pub fn new_map(vm: &mut Vm, kind: MapKind, entries: &[(Value, Value)]) -> ObjectId {
    let mut data = MapData::new(kind, false);
    let boxed: Vec<(Option<ObjectId>, Option<ObjectId>)> =
        entries.iter().map(|(k, v)| (crate::box_value(vm, *k), crate::box_value(vm, *v))).collect();
    for (k, v) in boxed {
        data.put(vm, k, v);
    }
    let id = vm.new_host_object(map_class(kind, false), Box::new(data));
    sync_map(vm, id);
    id
}

/// A set of `kind` filled with `items` in order.
pub fn new_set(vm: &mut Vm, kind: MapKind, items: &[Value]) -> ObjectId {
    let mut data = MapData::new(kind, true);
    let boxed: Vec<Option<ObjectId>> = items.iter().map(|v| crate::box_value(vm, *v)).collect();
    for k in boxed {
        data.put(vm, k, None);
    }
    let id = vm.new_host_object(map_class(kind, true), Box::new(data));
    sync_map(vm, id);
    id
}

/// An ArrayList of `items`.
pub fn new_list(vm: &mut Vm, items: &[Value]) -> ObjectId {
    let boxed: Vec<Option<ObjectId>> = items.iter().map(|v| crate::box_value(vm, *v)).collect();
    new_seq(vm, "java/util/ArrayList", boxed, true)
}

/// Entries of a map in iteration order.
pub fn map_entries(vm: &Vm, map: ObjectId) -> Option<Vec<(Option<ObjectId>, Option<ObjectId>)>> {
    let data = vm.host_data::<MapData>(map)?;
    Some(data.ordered(vm).into_iter().map(|e| (e.key, e.value)).collect())
}

/// Items of a list, set or view in iteration order.
pub fn list_items(vm: &Vm, id: ObjectId) -> Option<Vec<Option<ObjectId>>> {
    if let Some(seq) = vm.host_data::<Seq>(id) {
        return Some(seq.items.clone());
    }
    let data = vm.host_data::<MapData>(id)?;
    data.set.then(|| data.ordered(vm).into_iter().map(|e| e.key).collect())
}

fn to_array(vm: &mut Vm, items: Vec<Option<ObjectId>>) -> Value {
    Value::Object(vm.new_array(Array::Object(items), Some("java/lang/Object")))
}

fn no_element(vm: &mut Vm) -> javavm::Fail {
    throw(vm, "java/util/NoSuchElementException", "")
}

fn map_method(vm: &mut Vm, call: &Call, this: ObjectId, key: &str) -> JResult<Option<Value>> {
    let a = call.args;
    let arg = |i: usize| a.get(i).and_then(|v| v.object());
    let (set, kind) = {
        let d = vm.host_data::<MapData>(this).expect("checked");
        (d.set, d.kind)
    };
    // Reads first, with a shared borrow.
    let read = {
        let d = vm.host_data::<MapData>(this).expect("checked");
        match key {
            "size()I" => Some(Value::Int(d.entries.len() as i32)),
            "isEmpty()Z" => Some(Value::Boolean(d.entries.is_empty())),
            "containsKey(Ljava/lang/Object;)Z" | "contains(Ljava/lang/Object;)Z" => Some(Value::Boolean(d.position(vm, arg(0)).is_some())),
            "containsValue(Ljava/lang/Object;)Z" if !set => Some(Value::Boolean(d.entries.iter().any(|e| java_equals(vm, e.value, arg(0))))),
            "get(Ljava/lang/Object;)Ljava/lang/Object;" if !set => {
                Some(Value::from_object(d.position(vm, arg(0)).and_then(|i| d.entries[i].value)))
            }
            "getOrDefault(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" if !set => {
                Some(d.position(vm, arg(0)).map_or(a[1], |i| Value::from_object(d.entries[i].value)))
            }
            _ => None,
        }
    };
    if read.is_some() {
        return Ok(read);
    }
    let ordered = vm.host_data::<MapData>(this).expect("checked").ordered(vm);
    Ok(Some(match key {
        "keySet()Ljava/util/Set;" if !set => Value::Object(new_seq(vm, "java/util/HashMap$KeySet", ordered.iter().map(|e| e.key).collect(), false)),
        "values()Ljava/util/Collection;" if !set => {
            Value::Object(new_seq(vm, "java/util/HashMap$Values", ordered.iter().map(|e| e.value).collect(), false))
        }
        "entrySet()Ljava/util/Set;" if !set => {
            let entries: Vec<Option<ObjectId>> = ordered.iter().map(|e| Some(new_entry(vm, e.key, e.value))).collect();
            Value::Object(new_seq(vm, "java/util/HashMap$EntrySet", entries, false))
        }
        "iterator()Ljava/util/Iterator;" if set => Value::Object(new_iter(vm, ordered.iter().map(|e| e.key).collect())),
        "toArray()[Ljava/lang/Object;" | "toArray([Ljava/lang/Object;)[Ljava/lang/Object;" if set => {
            to_array(vm, ordered.iter().map(|e| e.key).collect())
        }
        "put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;" if !set => {
            let (k, v) = (arg(0), arg(1));
            if kind == MapKind::Tree && k.is_none() {
                return Err(throw(vm, "java/lang/NullPointerException", "TreeMap key"));
            }
            let mut d = take(vm, this);
            let previous = d.put(vm, k, v);
            restore(vm, this, d);
            Value::from_object(previous.flatten())
        }
        "add(Ljava/lang/Object;)Z" if set => {
            let mut d = take(vm, this);
            let added = d.put(vm, arg(0), None).is_none();
            restore(vm, this, d);
            Value::Boolean(added)
        }
        "remove(Ljava/lang/Object;)Ljava/lang/Object;" | "remove(Ljava/lang/Object;)Z" => {
            let mut d = take(vm, this);
            let removed = d.remove(vm, arg(0));
            restore(vm, this, d);
            if set || key.ends_with('Z') { Value::Boolean(removed.is_some()) } else { Value::from_object(removed.and_then(|e| e.value)) }
        }
        "clear()V" => {
            let mut d = take(vm, this);
            d.entries.clear();
            restore(vm, this, d);
            Value::Void
        }
        "putAll(Ljava/util/Map;)V" if !set => {
            let other = object_arg(vm, call, 0)?;
            let Some(entries) = map_entries(vm, other) else { return Ok(None) };
            let mut d = take(vm, this);
            for (k, v) in entries {
                d.put(vm, k, v);
            }
            restore(vm, this, d);
            Value::Void
        }
        _ => return Ok(None),
    }))
}

/// Takes the map data out so `put` can read other objects through `vm`.
fn take(vm: &mut Vm, id: ObjectId) -> MapData {
    let object = vm.heap.get_mut(id).expect("live");
    match std::mem::replace(&mut object.data, Data::Plain) {
        Data::Host(b) => *b.downcast::<MapData>().unwrap_or_else(|_| unreachable!("checked map data")),
        _ => unreachable!("checked map data"),
    }
}

fn restore(vm: &mut Vm, id: ObjectId, data: MapData) {
    vm.heap.get_mut(id).expect("live").data = Data::Host(Box::new(data));
    sync_map(vm, id);
}

fn seq_method(vm: &mut Vm, call: &Call, this: ObjectId, key: &str) -> JResult<Option<Value>> {
    let a = call.args;
    let (items, list) = {
        let s = vm.host_data::<Seq>(this).expect("checked");
        (s.items.clone(), s.list)
    };
    let out_of_bounds = |vm: &mut Vm, i: i32, len: usize| throw(vm, "java/lang/IndexOutOfBoundsException", &format!("Index: {i}, Size: {len}"));
    Ok(Some(match key {
        "size()I" => Value::Int(items.len() as i32),
        "isEmpty()Z" => Value::Boolean(items.is_empty()),
        "iterator()Ljava/util/Iterator;" => Value::Object(new_iter(vm, items)),
        "toArray()[Ljava/lang/Object;" | "toArray([Ljava/lang/Object;)[Ljava/lang/Object;" => to_array(vm, items),
        "contains(Ljava/lang/Object;)Z" => Value::Boolean(items.iter().any(|&i| java_equals(vm, i, a[0].object()))),
        "get(I)Ljava/lang/Object;" if list => {
            let i = a[0].as_i32();
            match usize::try_from(i).ok().and_then(|i| items.get(i)) {
                Some(item) => Value::from_object(*item),
                None => return Err(out_of_bounds(vm, i, items.len())),
            }
        }
        "add(Ljava/lang/Object;)Z" if list => {
            let item = a[0].object();
            vm.host_data_mut::<Seq>(this).expect("checked").items.push(item);
            let children = vm.host_data::<Seq>(this).expect("checked").items.iter().flatten().copied().collect();
            vm.heap.retain_in(this, children);
            Value::Boolean(true)
        }
        "set(ILjava/lang/Object;)Ljava/lang/Object;" if list => {
            let i = a[0].as_i32();
            let Some(index) = usize::try_from(i).ok().filter(|&i| i < items.len()) else {
                return Err(out_of_bounds(vm, i, items.len()));
            };
            let previous = std::mem::replace(&mut vm.host_data_mut::<Seq>(this).expect("checked").items[index], a[1].object());
            let children = vm.host_data::<Seq>(this).expect("checked").items.iter().flatten().copied().collect();
            vm.heap.retain_in(this, children);
            Value::from_object(previous)
        }
        _ => return Ok(None),
    }))
}

/// Constructors and instance methods of java.util classes.
pub(crate) fn instance(vm: &mut Vm, call: &Call, this: ObjectId, key: &str) -> JResult<Option<Value>> {
    if call.kind == CallKind::Constructor {
        let class = vm.object_class(this).map(|c| vm.class_name(c).to_string()).unwrap_or_default();
        let (kind, set) = match class.as_str() {
            "java/util/HashMap" => (MapKind::Hash, false),
            "java/util/LinkedHashMap" => (MapKind::Linked, false),
            "java/util/TreeMap" => (MapKind::Tree, false),
            "java/util/HashSet" => (MapKind::Hash, true),
            "java/util/LinkedHashSet" => (MapKind::Linked, true),
            "java/util/TreeSet" => (MapKind::Tree, true),
            "java/util/ArrayList" => {
                if !matches!(key, "<init>()V" | "<init>(I)V") {
                    return Ok(None);
                }
                vm.heap.get_mut(this).expect("live").data = Data::Host(Box::new(Seq { items: Vec::new(), list: true }));
                return Ok(Some(Value::Void));
            }
            _ => return Ok(None),
        };
        let mut data = MapData::new(kind, set);
        match key {
            "<init>()V" => {}
            // An initial capacity is rounded up to a power of two, as HashMap.tableSizeFor does.
            "<init>(I)V" if kind != MapKind::Tree => {
                let requested = call.args[0].as_i32();
                if requested < 0 {
                    return Err(throw(vm, "java/lang/IllegalArgumentException", &format!("Illegal initial capacity: {requested}")));
                }
                data.capacity = (requested.max(1) as usize).next_power_of_two();
            }
            _ => return Ok(None),
        }
        vm.heap.get_mut(this).expect("live").data = Data::Host(Box::new(data));
        return Ok(Some(Value::Void));
    }
    if vm.host_data::<MapData>(this).is_some() {
        return map_method(vm, call, this, key);
    }
    if vm.host_data::<Seq>(this).is_some() {
        return seq_method(vm, call, this, key);
    }
    if let Some(iter) = vm.host_data_mut::<Iter>(this) {
        return Ok(Some(match key {
            "hasNext()Z" => Value::Boolean(iter.next < iter.items.len()),
            "next()Ljava/lang/Object;" => match iter.items.get(iter.next).copied() {
                Some(item) => {
                    iter.next += 1;
                    Value::from_object(item)
                }
                None => return Err(no_element(vm)),
            },
            _ => return Ok(None),
        }));
    }
    if let Some(entry) = vm.host_data::<EntryData>(this) {
        return Ok(Some(match key {
            "getKey()Ljava/lang/Object;" => Value::from_object(entry.key),
            "getValue()Ljava/lang/Object;" => Value::from_object(entry.value),
            _ => return Ok(None),
        }));
    }
    Ok(None)
}

pub(crate) fn static_call(_vm: &mut Vm, _call: &Call, _key: &str) -> JResult<Option<Value>> {
    Ok(None)
}
