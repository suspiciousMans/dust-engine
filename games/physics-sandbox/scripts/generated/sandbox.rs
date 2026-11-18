
// Borrowing reads: `xs[i]`, `o.items[i].name` and `len(xs)` look through a
// chain of places by reference, so only the final element is ever cloned
// (reading by value used to clone the whole list on every access).
pub fn __ox_fld<'a>(v: &'a DynamicValue, name: &str) -> &'a DynamicValue {
    match v {
        DynamicValue::Struct(struct_name, fields) => fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("no field `{}` on struct `{}`", name, struct_name)),
        // `m.key` on a Map reads `m["key"]` (how http_serve delivers a `Request`).
        DynamicValue::Map(m) => m
            .get(&DynamicValue::Str(name.to_string()))
            .unwrap_or_else(|| panic!("no key `{}` in map", name)),
        other => panic!("type error: cannot access field `{}` on {:?}", name, other),
    }
}
pub fn __ox_idx<'a>(v: &'a DynamicValue, key: DynamicValue) -> std::borrow::Cow<'a, DynamicValue> {
    match v {
        DynamicValue::Map(entries) => std::borrow::Cow::Borrowed(
            entries
                .get(&key)
                .unwrap_or_else(|| panic!("no entry for key {:?}", key)),
        ),
        DynamicValue::List(items) | DynamicValue::Tuple(items) => {
            let i = key.as_i64();
            if i < 0 || i as usize >= items.len() {
                panic!("index {} out of bounds (len {})", i, items.len());
            }
            std::borrow::Cow::Borrowed(&items[i as usize])
        }
        other => std::borrow::Cow::Owned(other.index(key.as_i64())),
    }
}
pub fn __ox_len(v: &DynamicValue) -> DynamicValue {
    match v {
        DynamicValue::List(items) | DynamicValue::Tuple(items) => DynamicValue::Int(items.len() as i64),
        DynamicValue::Str(s) => DynamicValue::Int(s.chars().count() as i64),
        DynamicValue::Map(entries) => DynamicValue::Int(entries.len() as i64),
        other => panic!("type error: len() expects a List, String, or Map, got {:?}", other),
    }
}

// Mutable access to one element / field, for `xs[i] = v` and `o.p.x = v`.
pub fn __ox_vec_index_mut(items: &mut Vec<DynamicValue>, i: DynamicValue) -> &mut DynamicValue {
    let i = i.as_i64();
    let len = items.len();
    if i < 0 || i as usize >= len {
        panic!("index {} out of bounds (len {})", i, len);
    }
    &mut items[i as usize]
}
pub fn __ox_index_mut(v: &mut DynamicValue, i: DynamicValue) -> &mut DynamicValue {
    match v {
        // `m[key] = v` inserts the key when it is missing.
        DynamicValue::Map(entries) => {
            entries.entry_mut(i)
        }
        DynamicValue::List(items) => __ox_vec_index_mut(items, i),
        DynamicValue::Str(_) => panic!("type error: a String can't be assigned into by index"),
        DynamicValue::Tuple(_) => panic!("type error: a tuple can't be assigned into by index"),
        other => panic!("type error: cannot index into {:?}", other),
    }
}
pub fn __ox_field_mut<'a>(v: &'a mut DynamicValue, name: &str) -> &'a mut DynamicValue {
    match v {
        DynamicValue::Struct(struct_name, fields) => match fields.iter_mut().find(|(n, _)| n == name) {
            Some(entry) => &mut entry.1,
            None => panic!("no field `{}` on struct `{}`", name, struct_name),
        },
        other => panic!("type error: cannot access field `{}` on {:?}", name, other),
    }
}

// Stack guard: the generated `main` runs the program on a big-stack thread and
// records where that stack starts; every user function entry calls
// `__ox_stack_check`, so unbounded recursion becomes a readable panic (exit
// code 101) rather than a native stack-overflow abort. Base 0 (wasm, or the
// thread could not be created) disables the check.
const __OX_STACK_BUDGET: usize = (1 << 30) - (32 << 20);
thread_local! { static __OX_STACK_BASE: std::cell::Cell<usize> = std::cell::Cell::new(0); }
pub fn __ox_set_stack_base() {
    let marker = 0u8;
    __OX_STACK_BASE.with(|b| b.set(&marker as *const u8 as usize));
}
#[inline]
pub fn __ox_stack_check() {
    let base = __OX_STACK_BASE.with(|b| b.get());
    if base != 0 {
        let marker = 0u8;
        let here = &marker as *const u8 as usize;
        if base.wrapping_sub(here) > __OX_STACK_BUDGET {
            panic!("recursion limit exceeded: the call stack is full (unbounded recursion?)");
        }
    }
}

// Compile-time proof that values can cross threads: if a non-thread-safe field is
// ever added to DynamicValue, every generated program stops compiling here.
#[allow(dead_code)]
const _: fn() = || { fn assert_send_sync<T: Send + Sync>() {} assert_send_sync::<DynamicValue>(); };

// A shared, lockable cell for variables a closure captures and mutates. It keeps
// the `borrow()` / `borrow_mut()` spelling the generated code already uses, but is
// an `Arc<Mutex<T>>` so closures (and the values holding them) are Send + Sync.
// A panic in one thread must not poison the cell for the others.
pub struct OxCell<T>(std::sync::Arc<std::sync::Mutex<T>>);
impl<T> Clone for OxCell<T> {
    fn clone(&self) -> Self { OxCell(self.0.clone()) }
}
impl<T> OxCell<T> {
    pub fn new(v: T) -> Self { OxCell(std::sync::Arc::new(std::sync::Mutex::new(v))) }
    pub fn borrow(&self) -> std::sync::MutexGuard<'_, T> { self.0.lock().unwrap_or_else(|e| e.into_inner()) }
    pub fn borrow_mut(&self) -> std::sync::MutexGuard<'_, T> { self.0.lock().unwrap_or_else(|e| e.into_inner()) }
}

// A first-class function value: Rc gives cheap Clone regardless of the
// underlying closure's own Cloneness (dyn Fn can't derive it), and the
// manual Debug/PartialEq/PartialOrd impls below let DynamicValue keep
// deriving all three despite wrapping a trait object that implements none
// of them itself.
#[derive(Clone)]
pub struct OxFn(std::sync::Arc<dyn Fn(Vec<DynamicValue>) -> DynamicValue + Send + Sync>);

impl OxFn {
    pub fn new(f: impl Fn(Vec<DynamicValue>) -> DynamicValue + Send + Sync + 'static) -> Self {
        OxFn(std::sync::Arc::new(f))
    }
    pub fn call(&self, args: Vec<DynamicValue>) -> DynamicValue {
        (self.0)(args)
    }
}

impl std::fmt::Debug for OxFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<function>")
    }
}

impl PartialEq for OxFn {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

impl PartialOrd for OxFn {
    fn partial_cmp(&self, _other: &Self) -> Option<std::cmp::Ordering> {
        None
    }
}

// A List's elements, shared copy-on-write: cloning a list (which every
// by-value use of a variable, argument or struct field does) is O(1), and the
// first mutation of a shared list copies it. Derefs to `Vec<DynamicValue>`.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct OxList(std::sync::Arc<Vec<DynamicValue>>);
impl OxList {
    pub fn into_vec(self) -> Vec<DynamicValue> {
        match std::sync::Arc::try_unwrap(self.0) {
            Ok(v) => v,
            Err(shared) => (*shared).clone(),
        }
    }
}
impl std::iter::FromIterator<DynamicValue> for OxList {
    fn from_iter<I: IntoIterator<Item = DynamicValue>>(iter: I) -> Self {
        OxList(std::sync::Arc::new(iter.into_iter().collect()))
    }
}
impl From<Vec<DynamicValue>> for OxList {
    fn from(v: Vec<DynamicValue>) -> Self {
        OxList(std::sync::Arc::new(v))
    }
}
impl std::ops::Deref for OxList {
    type Target = Vec<DynamicValue>;
    fn deref(&self) -> &Vec<DynamicValue> {
        &self.0
    }
}
impl std::ops::DerefMut for OxList {
    fn deref_mut(&mut self) -> &mut Vec<DynamicValue> {
        std::sync::Arc::make_mut(&mut self.0)
    }
}

// A Map's entries: a hash-indexed, insertion-ordered table shared
// copy-on-write (cloning a map is O(1); the first write to a shared map
// copies it), like `OxList`. `entries` keeps insertion order (a removed
// entry leaves a `None` hole that the next rebuild squeezes out); `table` is
// an open-addressing index (linear probing, 0 = empty, n = entry n-1) keyed
// by a hash of the *canonical* key form, see `__ox_hash_into`/`__ox_key_eq`.
#[derive(Clone, Debug)]
pub struct OxMap(std::sync::Arc<OxMapData>);
#[derive(Clone, Debug, Default)]
pub struct OxMapData {
    entries: Vec<Option<(u64, DynamicValue, DynamicValue)>>,
    table: Vec<u32>,
    live: usize,
}
// Float keys: NaN is rejected; a float with an integral value (including
// -0.0) is the same key as the equal Int; any other float is its exact bits.
// (Map keys do NOT use `==`'s 1e-9 tolerance: it cannot back a hash.)
fn __ox_float_key(f: f64) -> Result<i64, u64> {
    if f.is_nan() {
        panic!("type error: NaN can't be used as a map key");
    }
    if f.fract() == 0.0 && f.abs() < 9.0e18 {
        Ok(f as i64)
    } else {
        Err(f.to_bits())
    }
}
fn __ox_hash_into(v: &DynamicValue, h: &mut std::collections::hash_map::DefaultHasher) {
    use std::hash::Hash;
    match v {
        DynamicValue::Int(i) => { 0u8.hash(h); i.hash(h); }
        DynamicValue::Float(f) => match __ox_float_key(*f) {
            Ok(i) => { 0u8.hash(h); i.hash(h); }
            Err(bits) => { 1u8.hash(h); bits.hash(h); }
        },
        DynamicValue::Str(s) => { 2u8.hash(h); s.hash(h); }
        DynamicValue::Bool(b) => { 3u8.hash(h); b.hash(h); }
        DynamicValue::None => { 4u8.hash(h); }
        DynamicValue::List(items) | DynamicValue::Tuple(items) => {
            (if matches!(v, DynamicValue::List(_)) { 5u8 } else { 6u8 }).hash(h);
            items.len().hash(h);
            for x in items.iter() { __ox_hash_into(x, h); }
        }
        DynamicValue::Struct(n, fields) => {
            7u8.hash(h);
            n.hash(h);
            for (fname, x) in fields.iter() { fname.hash(h); __ox_hash_into(x, h); }
        }
        DynamicValue::Enum(n, variant, payload) => {
            8u8.hash(h);
            n.hash(h);
            variant.hash(h);
            payload.len().hash(h);
            for x in payload.iter() { __ox_hash_into(x, h); }
        }
        DynamicValue::Map(_) | DynamicValue::Function(_) => panic!("type error: a Map or Function can't be used as a map key"),
    }
}
fn __ox_hash_key(v: &DynamicValue) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    __ox_hash_into(v, &mut h);
    h.finish()
}
fn __ox_key_eq(a: &DynamicValue, b: &DynamicValue) -> bool {
    type V = DynamicValue;
    match (a, b) {
        (V::Int(x), V::Int(y)) => x == y,
        (V::Float(x), V::Float(y)) => __ox_float_key(*x) == __ox_float_key(*y),
        (V::Int(x), V::Float(y)) | (V::Float(y), V::Int(x)) => __ox_float_key(*y) == Ok(*x),
        (V::Str(x), V::Str(y)) => x == y,
        (V::Bool(x), V::Bool(y)) => x == y,
        (V::None, V::None) => true,
        (V::List(x), V::List(y)) | (V::Tuple(x), V::Tuple(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| __ox_key_eq(p, q))
        }
        (V::Struct(an, af), V::Struct(bn, bf)) => {
            an == bn && af.len() == bf.len()
                && af.iter().zip(bf.iter()).all(|((n1, x), (n2, y))| n1 == n2 && __ox_key_eq(x, y))
        }
        (V::Enum(an, av, ap), V::Enum(bn, bv, bp)) => {
            an == bn && av == bv && ap.len() == bp.len() && ap.iter().zip(bp.iter()).all(|(x, y)| __ox_key_eq(x, y))
        }
        _ => false,
    }
}
impl OxMapData {
    fn find(&self, h: u64, key: &DynamicValue) -> Option<usize> {
        if self.table.is_empty() {
            return None;
        }
        let mask = self.table.len() - 1;
        let mut i = (h as usize) & mask;
        loop {
            let slot = self.table[i];
            if slot == 0 {
                return None;
            }
            if let Some((eh, k, _)) = &self.entries[(slot - 1) as usize] {
                if *eh == h && __ox_key_eq(k, key) {
                    return Some((slot - 1) as usize);
                }
            }
            i = (i + 1) & mask;
        }
    }
    fn rebuild(&mut self, want: usize) {
        self.entries.retain(|e| e.is_some());
        let mut cap = 8;
        while cap < want * 4 {
            cap *= 2;
        }
        self.table = vec![0; cap];
        let mask = cap - 1;
        for (idx, e) in self.entries.iter().enumerate() {
            if let Some((h, _, _)) = e {
                let mut i = (*h as usize) & mask;
                while self.table[i] != 0 {
                    i = (i + 1) & mask;
                }
                self.table[i] = (idx + 1) as u32;
            }
        }
    }
    fn push_new(&mut self, h: u64, key: DynamicValue, val: DynamicValue) -> usize {
        if (self.entries.len() + 1) * 2 > self.table.len() {
            self.rebuild(self.live + 1);
        }
        let mask = self.table.len() - 1;
        let mut i = (h as usize) & mask;
        while self.table[i] != 0 {
            i = (i + 1) & mask;
        }
        self.entries.push(Some((h, key, val)));
        self.table[i] = self.entries.len() as u32;
        self.live += 1;
        self.entries.len() - 1
    }
}
impl OxMap {
    pub fn new() -> OxMap {
        OxMap(std::sync::Arc::new(OxMapData::default()))
    }
    pub fn len(&self) -> usize {
        self.0.live
    }
    pub fn get(&self, key: &DynamicValue) -> Option<&DynamicValue> {
        let h = __ox_hash_key(key);
        self.0.find(h, key).map(|i| &self.0.entries[i].as_ref().unwrap().2)
    }
    pub fn contains(&self, key: &DynamicValue) -> bool {
        self.0.find(__ox_hash_key(key), key).is_some()
    }
    pub fn insert(&mut self, key: DynamicValue, val: DynamicValue) {
        *self.entry_mut(key) = val;
    }
    /// The value slot for `key`, inserting `None` first when it is missing.
    pub fn entry_mut(&mut self, key: DynamicValue) -> &mut DynamicValue {
        let h = __ox_hash_key(&key);
        let data = std::sync::Arc::make_mut(&mut self.0);
        let idx = match data.find(h, &key) {
            Some(i) => i,
            None => data.push_new(h, key, DynamicValue::None),
        };
        &mut data.entries[idx].as_mut().unwrap().2
    }
    pub fn remove(&mut self, key: &DynamicValue) {
        let h = __ox_hash_key(key);
        if self.0.find(h, key).is_none() {
            return;
        }
        let data = std::sync::Arc::make_mut(&mut self.0);
        if let Some(i) = data.find(h, key) {
            data.entries[i] = None;
            data.live -= 1;
            if data.live == 0 {
                data.entries.clear();
                data.table.clear();
            }
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = (&DynamicValue, &DynamicValue)> {
        self.0.entries.iter().filter_map(|e| e.as_ref().map(|(_, k, v)| (k, v)))
    }
    pub fn into_pairs(self) -> Vec<(DynamicValue, DynamicValue)> {
        let data = match std::sync::Arc::try_unwrap(self.0) {
            Ok(d) => d,
            Err(shared) => (*shared).clone(),
        };
        data.entries.into_iter().flatten().map(|(_, k, v)| (k, v)).collect()
    }
}
impl PartialEq for OxMap {
    fn eq(&self, other: &Self) -> bool {
        // Maps are equal when they bind the same keys to equal values, whatever
        // order the keys were inserted in (printing and iteration keep it).
        std::sync::Arc::ptr_eq(&self.0, &other.0)
            || (self.len() == other.len() && self.iter().all(|(k, v)| other.get(k).is_some_and(|ov| ov == v)))
    }
}
impl PartialOrd for OxMap {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.iter().collect::<Vec<_>>().partial_cmp(&other.iter().collect::<Vec<_>>())
    }
}

#[derive(Clone, Debug)]
pub enum DynamicValue {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    List(OxList),
    // A fixed-size positional group: `(1, "a")`. Unlike `List` it prints in
    // parentheses and is only ever indexed, destructured or matched.
    Tuple(OxList),
    // Vec<(field name, value)>, not a HashMap, so this still derives
    // PartialOrd (needed for </>/<=/>= between DynamicValues elsewhere)
    // and preserves declaration order for Debug/to_string output.
    Struct(String, Vec<(String, DynamicValue)>),
    // (enum type name, variant name, positional payload) — mirrors
    // `Struct`'s shape; `match` (see `emit_match`) dispatches on the
    // variant-name string at runtime.
    Enum(String, String, Vec<DynamicValue>),
    // A hash-indexed, insertion-ordered map (see `OxMap`).
    Map(OxMap),
    Function(OxFn),
    None,
}

impl std::ops::Add for DynamicValue {
    type Output = DynamicValue;
    fn add(self, other: DynamicValue) -> DynamicValue {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => DynamicValue::Int(a + b),
            (DynamicValue::Float(a), DynamicValue::Float(b)) => DynamicValue::Float(a + b),
            (DynamicValue::Str(a), DynamicValue::Str(b)) => DynamicValue::Str(a + &b),
            (l, r) => {
                let (lk, rk) = (l.kind_name(), r.kind_name());
                __ox_dispatch_op("__add__", l, Some(r)).unwrap_or_else(|| panic!("type error in +: cannot apply it to {} and {}", lk, rk))
            }
        }
    }
}

impl std::ops::Sub for DynamicValue {
    type Output = DynamicValue;
    fn sub(self, other: DynamicValue) -> DynamicValue {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => DynamicValue::Int(a - b),
            (DynamicValue::Float(a), DynamicValue::Float(b)) => DynamicValue::Float(a - b),
            (l, r) => {
                let (lk, rk) = (l.kind_name(), r.kind_name());
                __ox_dispatch_op("__sub__", l, Some(r)).unwrap_or_else(|| panic!("type error in -: cannot apply it to {} and {}", lk, rk))
            }
        }
    }
}

impl std::ops::Mul for DynamicValue {
    type Output = DynamicValue;
    fn mul(self, other: DynamicValue) -> DynamicValue {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => DynamicValue::Int(a * b),
            (DynamicValue::Float(a), DynamicValue::Float(b)) => DynamicValue::Float(a * b),
            (l, r) => {
                let (lk, rk) = (l.kind_name(), r.kind_name());
                __ox_dispatch_op("__mul__", l, Some(r)).unwrap_or_else(|| panic!("type error in *: cannot apply it to {} and {}", lk, rk))
            }
        }
    }
}

impl std::ops::Div for DynamicValue {
    type Output = DynamicValue;
    fn div(self, other: DynamicValue) -> DynamicValue {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => DynamicValue::Int(a / b),
            (DynamicValue::Float(a), DynamicValue::Float(b)) => DynamicValue::Float(a / b),
            (l, r) => {
                let (lk, rk) = (l.kind_name(), r.kind_name());
                __ox_dispatch_op("__div__", l, Some(r)).unwrap_or_else(|| panic!("type error in /: cannot apply it to {} and {}", lk, rk))
            }
        }
    }
}

impl std::ops::Rem for DynamicValue {
    type Output = DynamicValue;
    fn rem(self, other: DynamicValue) -> DynamicValue {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => DynamicValue::Int(a % b),
            (DynamicValue::Float(a), DynamicValue::Float(b)) => DynamicValue::Float(a % b),
            (l, r) => panic!("type error in %: cannot apply it to {} and {}", l.kind_name(), r.kind_name()),
        }
    }
}

impl std::ops::Neg for DynamicValue {
    type Output = DynamicValue;
    fn neg(self) -> DynamicValue {
        match self {
            DynamicValue::Int(a) => DynamicValue::Int(-a),
            DynamicValue::Float(a) => DynamicValue::Float(-a),
            other => __ox_dispatch_op("__neg__", other, None).unwrap_or_else(|| panic!("type error in unary -")),
        }
    }
}

impl PartialEq for DynamicValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => a == b,
            (DynamicValue::Float(a), DynamicValue::Float(b)) => (a - b).abs() < 1e-9,
            // `<` already compares Int with Float numerically; `==` must agree.
            (DynamicValue::Int(a), DynamicValue::Float(b)) | (DynamicValue::Float(b), DynamicValue::Int(a)) => (*a as f64 - b).abs() < 1e-9,
            (DynamicValue::Str(a), DynamicValue::Str(b)) => a == b,
            (DynamicValue::Bool(a), DynamicValue::Bool(b)) => a == b,
            (DynamicValue::List(a), DynamicValue::List(b)) => a == b,
            (DynamicValue::Tuple(a), DynamicValue::Tuple(b)) => a == b,
            (DynamicValue::Struct(an, af), DynamicValue::Struct(bn, bf)) => {
                // A struct that defines `__eq__` decides for itself; any other
                // struct is equal when every field is.
                if an == bn {
                    if let Some(answer) = __ox_dispatch_op("__eq__", self.clone(), Some(other.clone())) {
                        return answer.is_truthy();
                    }
                }
                an == bn && af == bf
            }
            (DynamicValue::Enum(an, av, ap), DynamicValue::Enum(bn, bv, bp)) => an == bn && av == bv && ap == bp,
            (DynamicValue::Map(a), DynamicValue::Map(b)) => a == b,
            (DynamicValue::None, DynamicValue::None) => true,
            _ => false,
        }
    }
}

impl PartialOrd for DynamicValue {
    // `<`, `<=`, `>` and `>=` on two Ints are the hottest comparisons in
    // loops; answer them without building an `Ordering`.
    #[inline]
    fn lt(&self, other: &Self) -> bool {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => a < b,
            _ => self.partial_cmp(other).is_some_and(|o| o.is_lt()),
        }
    }
    #[inline]
    fn le(&self, other: &Self) -> bool {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => a <= b,
            _ => self.partial_cmp(other).is_some_and(|o| o.is_le()),
        }
    }
    #[inline]
    fn gt(&self, other: &Self) -> bool {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => a > b,
            _ => self.partial_cmp(other).is_some_and(|o| o.is_gt()),
        }
    }
    #[inline]
    fn ge(&self, other: &Self) -> bool {
        match (self, other) {
            (DynamicValue::Int(a), DynamicValue::Int(b)) => a >= b,
            _ => self.partial_cmp(other).is_some_and(|o| o.is_ge()),
        }
    }
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        type V = DynamicValue;
        // Values of different kinds (other than Int with Float), different
        // structs or different enums have no order: `1 < "x"` used to be
        // `true` by an internal ranking of kinds, while `sort([1, "a"])`
        // panicked. Both are a type error now, as `+` on them is.
        fn unordered(a: &DynamicValue, b: &DynamicValue) -> ! {
            panic!("type error: cannot order {} and {} with < > <= >=", a.kind_name(), b.kind_name())
        }
        match (self, other) {
            (V::Int(a), V::Int(b)) => a.partial_cmp(b),
            (V::Float(a), V::Float(b)) => a.partial_cmp(b),
            // Int and Float compare by value, as `==` does.
            (V::Int(a), V::Float(b)) => (*a as f64).partial_cmp(b),
            (V::Float(a), V::Int(b)) => a.partial_cmp(&(*b as f64)),
            (V::Str(a), V::Str(b)) => a.partial_cmp(b),
            (V::Bool(a), V::Bool(b)) => a.partial_cmp(b),
            (V::List(a), V::List(b)) | (V::Tuple(a), V::Tuple(b)) => a.partial_cmp(b),
            (V::Struct(an, af), V::Struct(bn, bf)) => {
                // A struct that defines `__lt__` orders by it: less when
                // `self < other`, greater when `other < self`, else equal.
                // That one method gives `<`, `>`, `<=` and `>=`.
                if an == bn {
                    if let Some(less) = __ox_dispatch_op("__lt__", self.clone(), Some(other.clone())) {
                        if less.is_truthy() {
                            return Some(std::cmp::Ordering::Less);
                        }
                        let greater = __ox_dispatch_op("__lt__", other.clone(), Some(self.clone()))
                            .is_some_and(|g| g.is_truthy());
                        return Some(if greater { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal });
                    }
                }
                if an != bn {
                    unordered(self, other);
                }
                af.partial_cmp(bf)
            }
            (V::Enum(an, av, ap), V::Enum(bn, bv, bp)) => {
                if an != bn {
                    unordered(self, other);
                }
                // Variants order as declared (`Red < Green < Blue`), then by
                // payload; `__ox_variant_index` is generated per program.
                match (__ox_variant_index(an, av), __ox_variant_index(bn, bv)) {
                    (Some(ai), Some(bi)) => (ai, ap).partial_cmp(&(bi, bp)),
                    _ => (av, ap).partial_cmp(&(bv, bp)),
                }
            }
            (V::Map(a), V::Map(b)) => a.partial_cmp(b),
            (V::Function(a), V::Function(b)) => a.partial_cmp(b),
            (V::None, V::None) => Some(std::cmp::Ordering::Equal),
            _ => unordered(self, other),
        }
    }
}

impl DynamicValue {
    pub fn list(items: Vec<DynamicValue>) -> DynamicValue {
        DynamicValue::List(items.into())
    }

    pub fn tuple(items: Vec<DynamicValue>) -> DynamicValue {
        DynamicValue::Tuple(items.into())
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            DynamicValue::Bool(b) => *b,
            DynamicValue::None => false,
            _ => true,
        }
    }

    pub fn to_string(&self) -> String {
        match self {
            DynamicValue::Int(n) => n.to_string(),
            DynamicValue::Float(n) => {
                // The shortest digits that read back as the same value, with
                // `.0` added to a whole number (`3.0`). The `==` tolerance
                // must not leak in here: `1e-10` is not `0.0`.
                let s = format!("{}", n);
                if n.is_finite() && !s.contains('.') { format!("{s}.0") } else { s }
            }
            DynamicValue::Str(s) => s.clone(),
            DynamicValue::Bool(b) => {
                if *b { "true".into() } else { "false".into() }
            }
            DynamicValue::None => "None".into(),
            DynamicValue::List(items) => {
                let parts: Vec<String> = items.iter().map(|v| v.to_string()).collect();
                format!("[{}]", parts.join(", "))
            }
            DynamicValue::Tuple(items) => {
                let parts: Vec<String> = items.iter().map(|v| v.to_string()).collect();
                if items.len() == 1 {
                    format!("({},)", parts[0])
                } else {
                    format!("({})", parts.join(", "))
                }
            }
            DynamicValue::Struct(name, fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v.to_string()))
                    .collect();
                format!("{} {{ {} }}", name, parts.join(", "))
            }
            DynamicValue::Enum(_, variant, payload) => {
                if payload.is_empty() {
                    variant.clone()
                } else {
                    let parts: Vec<String> = payload.iter().map(|v| v.to_string()).collect();
                    format!("{}({})", variant, parts.join(", "))
                }
            }
            DynamicValue::Map(entries) => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.to_string(), v.to_string()))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            DynamicValue::Function(_) => "<function>".into(),
        }
    }

    /// A short name for the kind of value, for error messages.
    pub fn kind_name(&self) -> String {
        match self {
            DynamicValue::Int(_) => "Int".into(),
            DynamicValue::Float(_) => "Float".into(),
            DynamicValue::Str(_) => "String".into(),
            DynamicValue::Bool(_) => "Bool".into(),
            DynamicValue::List(_) => "List".into(),
            DynamicValue::Tuple(_) => "Tuple".into(),
            DynamicValue::Struct(n, _) => n.clone(),
            DynamicValue::Enum(n, _, _) => n.clone(),
            DynamicValue::Map(_) => "Map".into(),
            DynamicValue::Function(_) => "Function".into(),
            DynamicValue::None => "None".into(),
        }
    }

    pub fn enum_variant_name(&self) -> &str {
        match self {
            DynamicValue::Enum(_, variant, _) => variant.as_str(),
            // `None` (end of a chain, a missing lookup, `Option::None()`)
            // matches a `None()` pattern and no other variant, so it falls
            // through to a `_` arm instead of panicking.
            DynamicValue::None => "None",
            // Any other value (an Int from `find(..)`, a String, ...) is not
            // a variant of anything, so a variant pattern simply doesn't
            // match it and matching moves on to the next arm.
            _ => "",
        }
    }

    pub fn enum_payload(&self) -> &Vec<DynamicValue> {
        match self {
            DynamicValue::Enum(_, _, payload) => payload,
            other => panic!("type error: expected an enum value, got {:?}", other),
        }
    }

    pub fn as_i64(&self) -> i64 {
        match self {
            DynamicValue::Int(n) => *n,
            _ => panic!("type error: expected Int, got {:?}", self),
        }
    }

    pub fn as_f64(&self) -> f64 {
        match self {
            DynamicValue::Float(n) => *n,
            DynamicValue::Int(n) => *n as f64,
            _ => panic!("type error: expected Float, got {:?}", self),
        }
    }

    pub fn as_bool(&self) -> bool {
        match self {
            DynamicValue::Bool(b) => *b,
            _ => panic!("type error: expected Bool, got {:?}", self),
        }
    }

    pub fn as_string(&self) -> String {
        match self {
            DynamicValue::Str(s) => s.clone(),
            _ => panic!("type error: expected Str, got {:?}", self),
        }
    }

    pub fn index(&self, i: i64) -> DynamicValue {
        match self {
            DynamicValue::List(items) | DynamicValue::Tuple(items) => items
                .get(i as usize)
                .cloned()
                .unwrap_or_else(|| panic!("index {} out of bounds (len {})", i, items.len())),
            DynamicValue::Str(s) => {
                // ASCII (the common case) indexes by byte; otherwise walk the
                // characters without collecting them into a Vec.
                // Only the prefix up to `i` has to be ASCII for byte `i` to be
                // character `i`.
                let bytes = s.as_bytes();
                let found = if i >= 0 && (i as usize) < bytes.len() && bytes[..=i as usize].is_ascii() {
                    Some(s[i as usize..i as usize + 1].to_string())
                } else {
                    (i >= 0).then(|| i as usize).and_then(|i| s.chars().nth(i)).map(|c| c.to_string())
                };
                found.map(DynamicValue::Str).unwrap_or_else(|| {
                    panic!("index {} out of bounds (len {})", i, s.chars().count())
                })
            }
            other => panic!("type error: cannot index into {:?}", other),
        }
    }

    /// `target[key]` where the key may be any value: a Map is looked up by
    /// key, anything else is indexed by an Int.
    pub fn index_any(&self, key: DynamicValue) -> DynamicValue {
        match self {
            DynamicValue::Map(entries) => entries
                .get(&key)
                .cloned()
                .unwrap_or_else(|| panic!("no entry for key {:?}", key)),
            _ => self.index(key.as_i64()),
        }
    }

    pub fn tuple_items(&self) -> &Vec<DynamicValue> {
        match self {
            DynamicValue::Tuple(items) => items,
            other => panic!("type error: expected a tuple, got {:?}", other),
        }
    }

    pub fn is_struct_named(&self, name: &str) -> bool {
        matches!(self, DynamicValue::Struct(n, _) if n == name)
    }

    pub fn has_payload_len(&self, n: usize) -> bool {
        match self {
            DynamicValue::Enum(_, _, payload) => payload.len() == n,
            DynamicValue::None => n == 0,
            _ => false,
        }
    }

    pub fn is_tuple_of(&self, n: usize) -> bool {
        matches!(self, DynamicValue::Tuple(items) if items.len() == n)
    }

    pub fn into_list(self) -> Vec<DynamicValue> {
        match self {
            DynamicValue::List(items) => items.into_vec(),
            other => panic!("type error: expected List, got {:?}", other),
        }
    }

    pub fn field(&self, name: &str) -> DynamicValue {
        match self {
            DynamicValue::Struct(struct_name, fields) => fields
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("no field `{}` on struct `{}`", name, struct_name)),
            DynamicValue::Map(m) => m
                .get(&DynamicValue::Str(name.to_string()))
                .cloned()
                .unwrap_or_else(|| panic!("no key `{}` in map", name)),
            other => panic!("type error: cannot access field `{}` on {:?}", name, other),
        }
    }

    /// An Int becomes the Float of the same value; anything else is unchanged.
    /// Used where a Float is declared (struct fields, enum payloads) so that
    /// `S(f: 3)` and `s.f = 3` hold `3.0`, as arguments and returns already do.
    pub fn widen_int(self) -> DynamicValue {
        match self {
            DynamicValue::Int(n) => DynamicValue::Float(n as f64),
            other => other,
        }
    }

    /// Widens the Ints inside a value to Floats where `spec` says a Float is
    /// declared: `F` a Float, `-` leave alone, `L<spec>` every list element,
    /// `T(<spec>,<spec>..)` each tuple position. Used for annotated `let`s
    /// (`let xs: List<Float> = [1, 2]`).
    pub fn widen_by(self, spec: &str) -> DynamicValue {
        fn end_of(b: &[u8], i: usize) -> usize {
            match b[i] {
                b'L' => end_of(b, i + 1),
                b'T' => {
                    let mut j = i + 2;
                    loop {
                        j = end_of(b, j);
                        if b[j] == b')' {
                            return j + 1;
                        }
                        j += 1;
                    }
                }
                _ => i + 1,
            }
        }
        fn go(v: DynamicValue, b: &[u8], i: usize) -> DynamicValue {
            match (b[i], v) {
                (b'F', DynamicValue::Int(n)) => DynamicValue::Float(n as f64),
                (b'L', DynamicValue::List(items)) => {
                    DynamicValue::list(items.iter().map(|x| go(x.clone(), b, i + 1)).collect())
                }
                (b'T', DynamicValue::Tuple(items)) => {
                    let mut j = i + 2;
                    let mut out = Vec::with_capacity(items.len());
                    for x in items.iter() {
                        out.push(go(x.clone(), b, j));
                        j = end_of(b, j);
                        if b[j] == b',' {
                            j += 1;
                        }
                    }
                    DynamicValue::tuple(out)
                }
                (_, other) => other,
            }
        }
        go(self, spec.as_bytes(), 0)
    }

    pub fn set_field(&mut self, name: &str, value: DynamicValue) {
        match self {
            DynamicValue::Struct(struct_name, fields) => match fields.iter_mut().find(|(n, _)| n == name) {
                Some(entry) => {
                    entry.1 = if __ox_is_float_field(struct_name, name) { value.widen_int() } else { value }
                }
                None => panic!("no field `{}` on struct `{}`", name, struct_name),
            },
            other => panic!("type error: cannot set field `{}` on {:?}", name, other),
        }
    }

    /// Runtime side of `expr as Type`. `ty` is a Rust type string as
    /// produced by `rust_type_for`; anything that isn't a primitive or
    /// `List` (a struct or generic type param) passes through unchanged.
    pub fn cast_to(&self, ty: &str) -> DynamicValue {
        fn fail<T>(v: &DynamicValue, ty: &str) -> T {
            panic!("cannot cast {} to {}", v.to_string(), ty)
        }
        match ty {
            "i32" | "i64" => DynamicValue::Int(match self {
                DynamicValue::Int(n) => *n,
                DynamicValue::Float(f) => *f as i64,
                DynamicValue::Bool(b) => *b as i64,
                DynamicValue::Str(s) => s.trim().parse().unwrap_or_else(|_| fail(self, ty)),
                _ => fail(self, ty),
            }),
            "f32" | "f64" => DynamicValue::Float(match self {
                DynamicValue::Int(n) => *n as f64,
                DynamicValue::Float(f) => *f,
                DynamicValue::Bool(b) => *b as i64 as f64,
                DynamicValue::Str(s) => s.trim().parse().unwrap_or_else(|_| fail(self, ty)),
                _ => fail(self, ty),
            }),
            "String" => DynamicValue::Str(self.to_string()),
            "bool" => DynamicValue::Bool(match self {
                DynamicValue::Bool(b) => *b,
                DynamicValue::Int(n) => *n != 0,
                DynamicValue::Float(f) => *f != 0.0,
                DynamicValue::Str(s) if s == "true" => true,
                DynamicValue::Str(s) if s == "false" => false,
                _ => fail(self, ty),
            }),
            "Vec<DynamicValue>" => match self {
                DynamicValue::List(_) => self.clone(),
                _ => fail(self, ty),
            },
            _ => self.clone(),
        }
    }

    pub fn call_fn(&self, args: Vec<DynamicValue>) -> DynamicValue {
        match self {
            DynamicValue::Function(f) => f.call(args),
            other => panic!("type error: expected a function, got {:?}", other),
        }
    }
}

pub fn print(val: DynamicValue) {
    println!("{}", val.to_string());
}

pub fn assert_expr(cond: bool, msg: String) {
    assert!(cond, "{}", msg);
}

pub fn range(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    DynamicValue::List((a.as_i64()..b.as_i64()).map(DynamicValue::Int).collect())
}

pub fn tuple_arity(val: DynamicValue, n: i64) -> DynamicValue {
    match &val {
        DynamicValue::Tuple(items) if items.len() as i64 == n => DynamicValue::None,
        DynamicValue::Tuple(items) => panic!("type error: expected a tuple of {} element(s), got one with {}", n, items.len()),
        other => panic!("type error: expected a tuple of {} element(s), got {:?}", n, other),
    }
}

pub fn len(val: DynamicValue) -> DynamicValue {
    match val {
        DynamicValue::List(items) | DynamicValue::Tuple(items) => DynamicValue::Int(items.len() as i64),
        DynamicValue::Str(s) => DynamicValue::Int(s.chars().count() as i64),
        DynamicValue::Map(entries) => DynamicValue::Int(entries.len() as i64),
        other => panic!("type error: len() expects a List, String, or Map, got {:?}", other),
    }
}

// WASI/CLI builtins: expose std::env::args and stdin::read_line as first-class
// functions so oxidized programs can access them.
/// The program's command line. Under `oxidized run` the first element is the
/// `.ox` file that was run (passed in `OXIDIZED_PROGRAM`), not the temporary
/// binary in the build cache; a built binary run directly reports its own path.
pub fn __ox_args() -> DynamicValue {
    let mut args: Vec<DynamicValue> = std::env::args()
        .map(DynamicValue::Str)
        .collect();
    if let Ok(name) = std::env::var("OXIDIZED_PROGRAM") {
        if !name.is_empty() && !args.is_empty() {
            args[0] = DynamicValue::Str(name);
        }
    }
    DynamicValue::list(args)
}

pub fn __ox_read_line() -> DynamicValue {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let mut line = String::new();
    match stdin.lock().read_line(&mut line) {
        // 0 bytes read means end of input: `None`, so a program can tell it
        // apart from an empty line (`""`) and stop looping.
        Ok(0) => DynamicValue::None,
        Ok(_) => DynamicValue::Str(line.trim_end_matches(['\n', '\r']).to_string()),
        Err(e) => panic!("type error: read_line() failed: {}", e),
    }
}

// Functional-style push: returns a new list with `val` appended, rather
// than mutating `list` in place — fits the language's existing
// reassign-via-`let`/`=` idiom (e.g. `xs = push(xs, 4)`) without needing
// to thread `&mut` references through call codegen for one builtin.
pub fn push(list: DynamicValue, val: DynamicValue) -> DynamicValue {
    match list {
        DynamicValue::List(mut items) => {
            items.push(val);
            DynamicValue::List(items)
        }
        other => panic!("type error: push() expects a List, got {:?}", other),
    }
}

pub fn slice(val: DynamicValue, start: DynamicValue, end: DynamicValue) -> DynamicValue {
    let s = start.as_i64().max(0) as usize;
    let e = (end.as_i64().max(0) as usize).max(s);
    match val {
        DynamicValue::Str(str_val) => {
            if str_val.is_ascii() {
                let e = e.min(str_val.len());
                let s = s.min(e);
                DynamicValue::Str(str_val[s..e].to_string())
            } else {
                // Take characters s..e without collecting the whole string.
                DynamicValue::Str(str_val.chars().skip(s).take(e - s).collect())
            }
        }
        DynamicValue::List(items) => {
            let e = e.min(items.len());
            let s = s.min(e);
            DynamicValue::list(items[s..e].to_vec())
        }
        other => panic!("type error: slice() expects a List or String, got {:?}", other),
    }
}

pub fn map_new() -> DynamicValue {
    DynamicValue::Map(OxMap::new())
}

// Functional-style, like `push`: returns a new map with `key` bound to
// `val` (overwriting `key`'s old binding, if any) rather than mutating `m`
// in place.
pub fn map_set(m: DynamicValue, key: DynamicValue, val: DynamicValue) -> DynamicValue {
    match m {
        DynamicValue::Map(mut entries) => {
            entries.insert(key, val);
            DynamicValue::Map(entries)
        }
        other => panic!("type error: map_set() expects a Map, got {:?}", other),
    }
}

pub fn map_get(m: DynamicValue, key: DynamicValue) -> DynamicValue {
    match m {
        DynamicValue::Map(entries) => entries
            .get(&key)
            .cloned()
            .unwrap_or_else(|| panic!("map_get(): no entry for key {:?}", key)),
        other => panic!("type error: map_get() expects a Map, got {:?}", other),
    }
}

pub fn map_has(m: DynamicValue, key: DynamicValue) -> DynamicValue {
    match m {
        DynamicValue::Map(entries) => DynamicValue::Bool(entries.contains(&key)),
        other => panic!("type error: map_has() expects a Map, got {:?}", other),
    }
}

// Functional-style, like `push`/`map_set`: returns a new map with `key`
// (and its value) removed, a no-op if `key` isn't present.
pub fn map_remove(m: DynamicValue, key: DynamicValue) -> DynamicValue {
    match m {
        DynamicValue::Map(mut entries) => {
            entries.remove(&key);
            DynamicValue::Map(entries)
        }
        other => panic!("type error: map_remove() expects a Map, got {:?}", other),
    }
}

pub fn map_keys(m: DynamicValue) -> DynamicValue {
    match m {
        DynamicValue::Map(entries) => DynamicValue::List(entries.iter().map(|(k, _)| k.clone()).collect()),
        other => panic!("type error: map_keys() expects a Map, got {:?}", other),
    }
}

pub fn map_values(m: DynamicValue) -> DynamicValue {
    match m {
        DynamicValue::Map(entries) => DynamicValue::List(entries.iter().map(|(_, v)| v.clone()).collect()),
        other => panic!("type error: map_values() expects a Map, got {:?}", other),
    }
}

fn __ox_cmp(a: &DynamicValue, b: &DynamicValue) -> std::cmp::Ordering {
    use std::cmp::Ordering::Equal;
    match (a, b) {
        (DynamicValue::Int(x), DynamicValue::Int(y)) => x.cmp(y),
        (DynamicValue::Str(x), DynamicValue::Str(y)) => x.cmp(y),
        (DynamicValue::Bool(x), DynamicValue::Bool(y)) => x.cmp(y),
        (DynamicValue::Float(_), DynamicValue::Float(_) | DynamicValue::Int(_))
        | (DynamicValue::Int(_), DynamicValue::Float(_)) => a.as_f64().partial_cmp(&b.as_f64()).unwrap_or(Equal),
        // Structs of one type order by their `__lt__` (or field by field without one).
        (DynamicValue::Struct(x, _), DynamicValue::Struct(y, _)) if x == y => a.partial_cmp(b).unwrap_or(Equal),
        // Lists and tuples order element by element, and one enum's values
        // by declaration order, as `<` does.
        (DynamicValue::List(_), DynamicValue::List(_)) | (DynamicValue::Tuple(_), DynamicValue::Tuple(_)) => {
            a.partial_cmp(b).unwrap_or(Equal)
        }
        (DynamicValue::Enum(x, ..), DynamicValue::Enum(y, ..)) if x == y => a.partial_cmp(b).unwrap_or(Equal),
        _ => panic!("sort(): can't order {:?} against {:?}", a, b),
    }
}

/// A new list with the elements in ascending order (Ints and Floats together,
/// Strings, or Bools); stable. Panics on mixed or unorderable elements.
pub fn sort(list: DynamicValue) -> DynamicValue {
    match list {
        DynamicValue::List(mut items) => {
            items.sort_by(|a, b| __ox_cmp(a, b));
            DynamicValue::List(items)
        }
        other => panic!("type error: sort() expects a List, got {:?}", other),
    }
}

/// `sort_by(list, cmp)`: `cmp(a, b)` returns a negative Int when `a` comes
/// first, zero when they tie and a positive Int when `b` comes first.
pub fn sort_by(list: DynamicValue, cmp: DynamicValue) -> DynamicValue {
    match list {
        DynamicValue::List(mut items) => {
            items.sort_by(|a, b| cmp.call_fn(vec![a.clone(), b.clone()]).as_i64().cmp(&0));
            DynamicValue::List(items)
        }
        other => panic!("type error: sort_by() expects a List, got {:?}", other),
    }
}

pub fn reverse(val: DynamicValue) -> DynamicValue {
    match val {
        DynamicValue::List(mut items) => {
            items.reverse();
            DynamicValue::List(items)
        }
        DynamicValue::Str(s) => DynamicValue::Str(s.chars().rev().collect()),
        other => panic!("type error: reverse() expects a List or String, got {:?}", other),
    }
}

/// The sum of a list of numbers; `0` for an empty list. Ints and Floats may
/// be mixed (the total becomes a Float, as with `min`/`max`); anything else
/// is added with `+`, so overloaded `__add__` structs work too.
pub fn sum(list: DynamicValue) -> DynamicValue {
    match list {
        DynamicValue::List(items) => {
            let mut it = items.into_vec().into_iter();
            match it.next() {
                None => DynamicValue::Int(0),
                Some(first) => it.fold(first, |acc, x| match (acc, x) {
                    (DynamicValue::Int(a), DynamicValue::Float(b)) => DynamicValue::Float(a as f64 + b),
                    (DynamicValue::Float(a), DynamicValue::Int(b)) => DynamicValue::Float(a + b as f64),
                    (acc, x) => acc + x,
                }),
            }
        }
        other => panic!("type error: sum() expects a List, got {:?}", other),
    }
}

/// A String's characters, as a list of one-character Strings.
pub fn chars(s: DynamicValue) -> DynamicValue {
    DynamicValue::List(s.as_string().chars().map(|c| DynamicValue::Str(c.to_string())).collect())
}

/// The Unicode code point of a one-character String.
pub fn ord(s: DynamicValue) -> DynamicValue {
    let s = s.as_string();
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => DynamicValue::Int(c as i64),
        _ => panic!("ord() expects a one-character String, got {:?}", s),
    }
}

/// The one-character String for a Unicode code point.
pub fn chr(n: DynamicValue) -> DynamicValue {
    let n = n.as_i64();
    match if (0..=0x10FFFF).contains(&n) { std::char::from_u32(n as u32) } else { None } {
        Some(c) => DynamicValue::Str(c.to_string()),
        None => panic!("chr(): {} is not a valid code point", n),
    }
}

/// The position of the first element of a list (or substring of a String)
/// equal to `needle`, or -1.
pub fn index_of(hay: DynamicValue, needle: DynamicValue) -> DynamicValue {
    match hay {
        DynamicValue::List(items) => DynamicValue::Int(items.iter().position(|x| *x == needle).map_or(-1, |p| p as i64)),
        DynamicValue::Str(s) => {
            let needle = needle.as_string();
            DynamicValue::Int(s.find(&needle).map_or(-1, |b| s[..b].chars().count() as i64))
        }
        other => panic!("type error: index_of() expects a List or String, got {:?}", other),
    }
}

pub fn bit_and(a: DynamicValue, b: DynamicValue) -> DynamicValue { DynamicValue::Int(a.as_i64() & b.as_i64()) }
pub fn bit_or(a: DynamicValue, b: DynamicValue) -> DynamicValue { DynamicValue::Int(a.as_i64() | b.as_i64()) }
pub fn bit_xor(a: DynamicValue, b: DynamicValue) -> DynamicValue { DynamicValue::Int(a.as_i64() ^ b.as_i64()) }
pub fn bit_not(a: DynamicValue) -> DynamicValue { DynamicValue::Int(!a.as_i64()) }
pub fn shl(a: DynamicValue, n: DynamicValue) -> DynamicValue {
    let n = n.as_i64();
    if !(0..64).contains(&n) { panic!("shl(): shift amount {} is out of range 0..63", n); }
    DynamicValue::Int(a.as_i64() << n)
}
pub fn shr(a: DynamicValue, n: DynamicValue) -> DynamicValue {
    let n = n.as_i64();
    if !(0..64).contains(&n) { panic!("shr(): shift amount {} is out of range 0..63", n); }
    DynamicValue::Int(a.as_i64() >> n)
}

pub fn abs(x: DynamicValue) -> DynamicValue {
    match x {
        DynamicValue::Int(n) => DynamicValue::Int(n.abs()),
        DynamicValue::Float(n) => DynamicValue::Float(n.abs()),
        other => panic!("type error: abs() expects an Int or Float, got {:?}", other),
    }
}

pub fn min(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    match (&a, &b) {
        (DynamicValue::Int(x), DynamicValue::Int(y)) => DynamicValue::Int((*x).min(*y)),
        (DynamicValue::Float(_), _) | (_, DynamicValue::Float(_)) => {
            DynamicValue::Float(a.as_f64().min(b.as_f64()))
        }
        _ => panic!("type error: min() expects two Ints/Floats, got {:?} and {:?}", a, b),
    }
}

pub fn max(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    match (&a, &b) {
        (DynamicValue::Int(x), DynamicValue::Int(y)) => DynamicValue::Int((*x).max(*y)),
        (DynamicValue::Float(_), _) | (_, DynamicValue::Float(_)) => {
            DynamicValue::Float(a.as_f64().max(b.as_f64()))
        }
        _ => panic!("type error: max() expects two Ints/Floats, got {:?} and {:?}", a, b),
    }
}

pub fn sqrt(x: DynamicValue) -> DynamicValue {
    DynamicValue::Float(x.as_f64().sqrt())
}

pub fn pow(base: DynamicValue, exp: DynamicValue) -> DynamicValue {
    DynamicValue::Float(base.as_f64().powf(exp.as_f64()))
}

pub fn floor(x: DynamicValue) -> DynamicValue {
    DynamicValue::Int(x.as_f64().floor() as i64)
}

pub fn ceil(x: DynamicValue) -> DynamicValue {
    DynamicValue::Int(x.as_f64().ceil() as i64)
}

pub fn round(x: DynamicValue) -> DynamicValue {
    DynamicValue::Int(x.as_f64().round() as i64)
}

pub fn upper(s: DynamicValue) -> DynamicValue {
    DynamicValue::Str(s.as_string().to_uppercase())
}

pub fn lower(s: DynamicValue) -> DynamicValue {
    DynamicValue::Str(s.as_string().to_lowercase())
}

pub fn trim(s: DynamicValue) -> DynamicValue {
    DynamicValue::Str(s.as_string().trim().to_string())
}

pub fn split(s: DynamicValue, sep: DynamicValue) -> DynamicValue {
    let s = s.as_string();
    let sep = sep.as_string();
    if sep.is_empty() {
        DynamicValue::List(s.chars().map(|c| DynamicValue::Str(c.to_string())).collect())
    } else {
        DynamicValue::List(s.split(sep.as_str()).map(|p| DynamicValue::Str(p.to_string())).collect())
    }
}

pub fn join(list: DynamicValue, sep: DynamicValue) -> DynamicValue {
    let sep = sep.as_string();
    match list {
        DynamicValue::List(items) => {
            let parts: Vec<String> = items.iter().map(|v| v.to_string()).collect();
            DynamicValue::Str(parts.join(&sep))
        }
        other => panic!("type error: join() expects a List, got {:?}", other),
    }
}

pub fn contains(haystack: DynamicValue, needle: DynamicValue) -> DynamicValue {
    match haystack {
        DynamicValue::Str(s) => DynamicValue::Bool(s.contains(&needle.as_string())),
        DynamicValue::List(items) => DynamicValue::Bool(items.contains(&needle)),
        other => panic!("type error: contains() expects a List or String, got {:?}", other),
    }
}

pub fn starts_with(s: DynamicValue, prefix: DynamicValue) -> DynamicValue {
    DynamicValue::Bool(s.as_string().starts_with(&prefix.as_string()))
}

pub fn ends_with(s: DynamicValue, suffix: DynamicValue) -> DynamicValue {
    DynamicValue::Bool(s.as_string().ends_with(&suffix.as_string()))
}

pub fn replace(s: DynamicValue, old: DynamicValue, new: DynamicValue) -> DynamicValue {
    DynamicValue::Str(s.as_string().replace(&old.as_string(), &new.as_string()))
}

fn __ox_result_ok(val: DynamicValue) -> DynamicValue {
    DynamicValue::Enum("Result".to_string(), "Ok".to_string(), vec![val])
}

fn __ox_result_err(msg: String) -> DynamicValue {
    DynamicValue::Enum("Result".to_string(), "Err".to_string(), vec![DynamicValue::Str(msg)])
}

pub fn parse_int(s: DynamicValue) -> DynamicValue {
    let text = s.as_string();
    match text.trim().parse::<i64>() {
        Ok(n) => __ox_result_ok(DynamicValue::Int(n)),
        Err(_) => __ox_result_err(format!("cannot parse \"{text}\" as an Int")),
    }
}

pub fn parse_float(s: DynamicValue) -> DynamicValue {
    let text = s.as_string();
    match text.trim().parse::<f64>() {
        Ok(n) => __ox_result_ok(DynamicValue::Float(n)),
        Err(_) => __ox_result_err(format!("cannot parse \"{text}\" as a Float")),
    }
}

/// `int(x)` — cast to Int via `as i64`. Equivalent to `x as Int`.
pub fn __ox_int(v: DynamicValue) -> DynamicValue {
    v.cast_to("i64")
}

/// `float(x)` — cast to Float via `as f64`. Equivalent to `x as Float`.
pub fn __ox_float(v: DynamicValue) -> DynamicValue {
    v.cast_to("f64")
}

/// `str(x)` — cast to String via `.to_string()`. Equivalent to `x as String`.
pub fn __ox_str(v: DynamicValue) -> DynamicValue {
    DynamicValue::Str(v.to_string())
}

/// `bool(x)` — cast to Bool. Int → `!= 0`, Float → `!= 0.0`, Str → `!is_empty()`.
pub fn __ox_bool(v: DynamicValue) -> DynamicValue {
    match v {
        DynamicValue::Bool(b) => DynamicValue::Bool(b),
        DynamicValue::Int(n) => DynamicValue::Bool(n != 0),
        DynamicValue::Float(f) => DynamicValue::Bool(f != 0.0),
        DynamicValue::Str(s) => DynamicValue::Bool(!s.is_empty()),
        _ => panic!("type error: bool() expects Int, Float, String, or Bool, got {:?}", v),
    }
}


pub fn checked_div(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    let is_zero = match &b {
        DynamicValue::Int(0) => true,
        DynamicValue::Float(x) => *x == 0.0,
        _ => false,
    };
    if is_zero {
        __ox_result_err("division by zero".to_string())
    } else if matches!((&a, &b), (DynamicValue::Int(i64::MIN), DynamicValue::Int(-1))) {
        __ox_result_err("integer overflow in /".to_string())
    } else {
        __ox_result_ok(a / b)
    }
}

// Unlike `map_get`, doesn't panic when `key` is missing.
pub fn map_try_get(m: DynamicValue, key: DynamicValue) -> DynamicValue {
    match m {
        DynamicValue::Map(entries) => match entries.get(&key).cloned() {
            Some(v) => __ox_result_ok(v),
            None => __ox_result_err(format!("no entry for key {}", key.to_string())),
        },
        other => panic!("type error: map_try_get() expects a Map, got {:?}", other),
    }
}

// Fallible counterparts of the functions that panic: same arguments, but a
// bad input gives `Result::Err(message)` instead of a panic.
pub fn try_index(target: DynamicValue, i: DynamicValue) -> DynamicValue {
    let i = i.as_i64();
    let len = match &target {
        DynamicValue::List(items) | DynamicValue::Tuple(items) => items.len(),
        DynamicValue::Str(s) => s.chars().count(),
        other => panic!("type error: try_index() expects a List, Tuple or String, got {:?}", other),
    };
    if i < 0 || i as usize >= len {
        return __ox_result_err(format!("index {i} out of bounds (len {len})"));
    }
    __ox_result_ok(target.index(i))
}

pub fn try_slice(val: DynamicValue, start: DynamicValue, end: DynamicValue) -> DynamicValue {
    let (s, e) = (start.as_i64(), end.as_i64());
    let len = match &val {
        DynamicValue::List(items) => items.len(),
        DynamicValue::Str(t) => t.chars().count(),
        other => panic!("type error: try_slice() expects a List or String, got {:?}", other),
    } as i64;
    if s < 0 || e < s || e > len {
        return __ox_result_err(format!("slice {s}..{e} out of range (len {len})"));
    }
    __ox_result_ok(slice(val, start, end))
}

pub fn try_ord(s: DynamicValue) -> DynamicValue {
    let s = s.as_string();
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => __ox_result_ok(DynamicValue::Int(c as i64)),
        _ => __ox_result_err(format!("ord() expects a one-character String, got \"{s}\"")),
    }
}

pub fn try_chr(n: DynamicValue) -> DynamicValue {
    let n = n.as_i64();
    match if (0..=0x10FFFF).contains(&n) { std::char::from_u32(n as u32) } else { None } {
        Some(c) => __ox_result_ok(DynamicValue::Str(c.to_string())),
        None => __ox_result_err(format!("{n} is not a valid code point")),
    }
}

pub fn checked_add(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    match a.as_i64().checked_add(b.as_i64()) {
        Some(n) => __ox_result_ok(DynamicValue::Int(n)),
        None => __ox_result_err("integer overflow in +".to_string()),
    }
}

pub fn checked_sub(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    match a.as_i64().checked_sub(b.as_i64()) {
        Some(n) => __ox_result_ok(DynamicValue::Int(n)),
        None => __ox_result_err("integer overflow in -".to_string()),
    }
}

pub fn checked_mul(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    match a.as_i64().checked_mul(b.as_i64()) {
        Some(n) => __ox_result_ok(DynamicValue::Int(n)),
        None => __ox_result_err("integer overflow in *".to_string()),
    }
}

/// Turns the exit value of `main` into a process exit code: an `Err`
/// escaping `main` prints `error: <message>` to stderr and exits 1.
fn __ox_main_result(v: DynamicValue) -> i32 {
    match v {
        DynamicValue::Enum(name, variant, payload) if name == "Result" && variant == "Err" => {
            eprintln!("error: {}", payload[0].to_string());
            1
        }
        _ => 0,
    }
}

fn __ox_expect_wrapped(r: &DynamicValue, caller: &str) {
    let ok = matches!(r, DynamicValue::None)
        || matches!(r, DynamicValue::Enum(name, ..) if name == "Result" || name == "Option");
    if !ok {
        panic!("type error: {caller}() expects a Result or Option, got {:?}", r);
    }
}

/// True for `Ok(..)` / `Some(..)`; false for `Err(..)`, `Option::None()` and the `None` literal.
fn __ox_is_present(r: &DynamicValue) -> bool {
    matches!(r, DynamicValue::Enum(_, v, _) if v == "Ok" || v == "Some")
}

pub fn is_some(r: DynamicValue) -> DynamicValue {
    __ox_expect_wrapped(&r, "is_some");
    DynamicValue::Bool(__ox_is_present(&r))
}

pub fn is_none(r: DynamicValue) -> DynamicValue {
    __ox_expect_wrapped(&r, "is_none");
    DynamicValue::Bool(!__ox_is_present(&r))
}

fn __ox_expect_result(r: &DynamicValue, caller: &str) {
    if !matches!(r, DynamicValue::Enum(name, ..) if name == "Result") {
        panic!("type error: {caller}() expects a Result, got {:?}", r);
    }
}

pub fn is_ok(r: DynamicValue) -> DynamicValue {
    __ox_expect_result(&r, "is_ok");
    match r {
        DynamicValue::Enum(_, variant, _) => DynamicValue::Bool(variant == "Ok"),
        _ => unreachable!(),
    }
}

pub fn is_err(r: DynamicValue) -> DynamicValue {
    __ox_expect_result(&r, "is_err");
    match r {
        DynamicValue::Enum(_, variant, _) => DynamicValue::Bool(variant == "Err"),
        _ => unreachable!(),
    }
}

pub fn unwrap(r: DynamicValue) -> DynamicValue {
    __ox_expect_wrapped(&r, "unwrap");
    match r {
        DynamicValue::Enum(_, variant, mut payload) if variant == "Ok" || variant == "Some" => payload.remove(0),
        DynamicValue::Enum(n, _, payload) if n == "Result" => {
            panic!("called unwrap() on an Err: {}", payload[0].to_string())
        }
        _ => panic!("called unwrap() on a None"),
    }
}

pub fn unwrap_or(r: DynamicValue, default: DynamicValue) -> DynamicValue {
    __ox_expect_wrapped(&r, "unwrap_or");
    match r {
        DynamicValue::Enum(_, variant, mut payload) if variant == "Ok" || variant == "Some" => payload.remove(0),
        _ => default,
    }
}

/// `unwrap_err(r)`: the payload of an `Err`; panics on an `Ok` (mirror of `unwrap`).
pub fn unwrap_err(r: DynamicValue) -> DynamicValue {
    __ox_expect_result(&r, "unwrap_err");
    match r {
        DynamicValue::Enum(_, variant, mut payload) if variant == "Err" => payload.remove(0),
        DynamicValue::Enum(_, _, payload) => {
            panic!("called unwrap_err() on an Ok: {}", payload[0].to_string())
        }
        _ => unreachable!(),
    }
}

/// `map_err(r, f)`: an `Err(e)` becomes `Err(f(e))`; an `Ok` is unchanged.
pub fn map_err(r: DynamicValue, f: DynamicValue) -> DynamicValue {
    __ox_expect_result(&r, "map_err");
    match (r, f) {
        (DynamicValue::Enum(n, v, payload), DynamicValue::Function(f)) if v == "Err" => {
            let e = f.call(vec![payload[0].clone()]);
            let e = match e { DynamicValue::Str(s) => s, other => other.to_string() };
            DynamicValue::Enum(n, v, vec![DynamicValue::Str(e)])
        }
        (r, DynamicValue::Function(_)) => r,
        (_, other) => panic!("type error: map_err() expects a function, got {:?}", other),
    }
}

/// `ok_or(v, msg)`: `None` becomes `Err(msg)`, any other value `Ok(v)`.
pub fn ok_or(v: DynamicValue, msg: DynamicValue) -> DynamicValue {
    match v {
        DynamicValue::None => __ox_result_err(msg.to_string()),
        other => __ox_result_ok(other),
    }
}

/// `unwrap_or_else(r, f)`: the `Ok` payload, or `f(message)` for an `Err`.
pub fn unwrap_or_else(r: DynamicValue, f: DynamicValue) -> DynamicValue {
    __ox_expect_wrapped(&r, "unwrap_or_else");
    match (r, f) {
        (DynamicValue::Enum(_, v, mut payload), _) if v == "Ok" || v == "Some" => payload.remove(0),
        (DynamicValue::Enum(_, _, payload), DynamicValue::Function(f)) => {
            f.call(vec![payload.first().cloned().unwrap_or(DynamicValue::None)])
        }
        (DynamicValue::None, DynamicValue::Function(f)) => f.call(vec![DynamicValue::None]),
        (_, other) => panic!("type error: unwrap_or_else() expects a function, got {:?}", other),
    }
}

fn __ox_map_from(pairs: Vec<(DynamicValue, DynamicValue)>) -> DynamicValue {
    let mut m = OxMap::new();
    for (k, v) in pairs {
        m.insert(k, v);
    }
    DynamicValue::Map(m)
}
// ---------------------------------------------------------------- stdlib
fn __ox_str_arg(v: &DynamicValue, who: &str) -> String {
    match v {
        DynamicValue::Str(s) => s.clone(),
        other => panic!("type error: {}() expects a String, got {:?}", who, other),
    }
}

fn __ox_io_result(r: std::io::Result<DynamicValue>, path: &str) -> DynamicValue {
    match r {
        Ok(v) => __ox_result_ok(v),
        Err(e) => __ox_result_err(format!("{}: {}", path, e)),
    }
}

pub fn __ox_read_file(path: DynamicValue) -> DynamicValue {
    let p = __ox_str_arg(&path, "read_file");
    __ox_io_result(std::fs::read_to_string(&p).map(DynamicValue::Str), &p)
}

pub fn __ox_write_file(path: DynamicValue, text: DynamicValue) -> DynamicValue {
    let p = __ox_str_arg(&path, "write_file");
    let t = __ox_str_arg(&text, "write_file");
    __ox_io_result(__ox_write_atomic(&p, t.as_bytes()).map(|_| DynamicValue::None), &p)
}

// Writes `data` to a temp file beside `path`, flushes it to disk, then renames
// it over `path`, so a crash mid-write leaves the old file intact rather than
// a truncated one. The temp file is removed if anything fails.
fn __ox_write_atomic(path: &str, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    // std::process::id() panics on wasm; there the counter alone is unique (single-threaded).
    let pid = if cfg!(target_family = "wasm") { 0 } else { std::process::id() };
    let tmp = format!("{}.oxtmp{}_{}", path, pid, SEQ.fetch_add(1, Ordering::SeqCst));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub fn __ox_append_file(path: DynamicValue, text: DynamicValue) -> DynamicValue {
    use std::io::Write;
    let p = __ox_str_arg(&path, "append_file");
    let t = __ox_str_arg(&text, "append_file");
    let r = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .and_then(|mut f| f.write_all(t.as_bytes()));
    __ox_io_result(r.map(|_| DynamicValue::None), &p)
}

pub fn __ox_file_exists(path: DynamicValue) -> DynamicValue {
    let p = __ox_str_arg(&path, "file_exists");
    DynamicValue::Bool(std::path::Path::new(&p).exists())
}

pub fn __ox_list_dir(path: DynamicValue) -> DynamicValue {
    let p = __ox_str_arg(&path, "list_dir");
    let r = std::fs::read_dir(&p).and_then(|rd| {
        let mut names = Vec::new();
        for entry in rd {
            names.push(entry?.file_name().to_string_lossy().to_string());
        }
        names.sort();
        Ok(DynamicValue::list(names.into_iter().map(DynamicValue::Str).collect()))
    });
    __ox_io_result(r, &p)
}

pub fn __ox_remove_file(path: DynamicValue) -> DynamicValue {
    let p = __ox_str_arg(&path, "remove_file");
    __ox_io_result(std::fs::remove_file(&p).map(|_| DynamicValue::None), &p)
}

pub fn __ox_make_dir(path: DynamicValue) -> DynamicValue {
    let p = __ox_str_arg(&path, "make_dir");
    __ox_io_result(std::fs::create_dir_all(&p).map(|_| DynamicValue::None), &p)
}

pub fn __ox_env(name: DynamicValue) -> DynamicValue {
    let n = __ox_str_arg(&name, "env");
    if n.is_empty() || n.contains('=') || n.contains('\0') {
        return __ox_result_err(format!("invalid environment variable name `{}`", n));
    }
    match std::env::var(&n) {
        Ok(v) => __ox_result_ok(DynamicValue::Str(v)),
        Err(std::env::VarError::NotPresent) => __ox_result_err(format!("environment variable `{}` is not set", n)),
        Err(_) => __ox_result_err(format!("environment variable `{}` is not valid unicode", n)),
    }
}

pub fn __ox_exit(code: DynamicValue) -> DynamicValue {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code.as_i64() as i32)
}

// Wall-clock milliseconds since the Unix epoch.
pub fn __ox_now_ms() -> DynamicValue {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    DynamicValue::Int(d.as_millis() as i64)
}

// Monotonic milliseconds since the program first asked (never goes backwards).
pub fn __ox_mono_ms() -> DynamicValue {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    DynamicValue::Int(start.elapsed().as_millis() as i64)
}

pub fn __ox_sleep_ms(ms: DynamicValue) -> DynamicValue {
    let n = ms.as_i64();
    if n > 0 {
        std::thread::sleep(std::time::Duration::from_millis(n as u64));
    }
    DynamicValue::None
}

// ------------------------------------------------------------------ text
pub fn __ox_split_lines(s: DynamicValue) -> DynamicValue {
    let s = __ox_str_arg(&s, "split_lines");
    DynamicValue::list(s.lines().map(|l| DynamicValue::Str(l.to_string())).collect())
}

fn __ox_pad(s: DynamicValue, width: DynamicValue, pad: DynamicValue, left: bool) -> DynamicValue {
    let who = if left { "pad_left" } else { "pad_right" };
    let s = __ox_str_arg(&s, who);
    let pad: Vec<char> = __ox_str_arg(&pad, who).chars().collect();
    let have = s.chars().count();
    let want = width.as_i64().max(0) as usize;
    if have >= want || pad.is_empty() {
        return DynamicValue::Str(s);
    }
    let fill: String = (0..want - have).map(|i| pad[i % pad.len()]).collect();
    DynamicValue::Str(if left { fill + &s } else { s + &fill })
}

pub fn __ox_pad_left(s: DynamicValue, width: DynamicValue, pad: DynamicValue) -> DynamicValue {
    __ox_pad(s, width, pad, true)
}

pub fn __ox_pad_right(s: DynamicValue, width: DynamicValue, pad: DynamicValue) -> DynamicValue {
    __ox_pad(s, width, pad, false)
}

pub fn __ox_repeat(s: DynamicValue, n: DynamicValue) -> DynamicValue {
    let s = __ox_str_arg(&s, "repeat");
    DynamicValue::Str(s.repeat(n.as_i64().max(0) as usize))
}

pub fn __ox_format_float(x: DynamicValue, digits: DynamicValue) -> DynamicValue {
    let d = digits.as_i64().clamp(0, 100) as usize;
    DynamicValue::Str(format!("{:.*}", d, x.as_f64()))
}

pub fn __ox_trim_start(s: DynamicValue) -> DynamicValue {
    DynamicValue::Str(__ox_str_arg(&s, "trim_start").trim_start().to_string())
}

pub fn __ox_trim_end(s: DynamicValue) -> DynamicValue {
    DynamicValue::Str(__ox_str_arg(&s, "trim_end").trim_end().to_string())
}

// ------------------------------------------------------------------ JSON
const __OX_JSON_MAX_DEPTH: usize = 200;

struct __OxJsonParser<'a> {
    src: &'a str,
    b: &'a [u8],
    i: usize,
}

impl<'a> __OxJsonParser<'a> {
    fn err<T>(&self, msg: &str) -> Result<T, String> {
        let upto = &self.b[..self.i.min(self.b.len())];
        let line = upto.iter().filter(|&&c| c == b'\n').count() + 1;
        let col = match upto.iter().rposition(|&c| c == b'\n') {
            Some(p) => self.src[p + 1..self.i.min(self.b.len())].chars().count() + 1,
            None => self.src[..self.i.min(self.b.len())].chars().count() + 1,
        };
        Err(format!("invalid JSON: {} at line {} column {}", msg, line, col))
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<DynamicValue, String> {
        self.ws();
        if self.i >= self.b.len() {
            return self.err("unexpected end of input");
        }
        match self.b[self.i] {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => Ok(DynamicValue::Str(self.string()?)),
            b't' => self.literal("true", DynamicValue::Bool(true)),
            b'f' => self.literal("false", DynamicValue::Bool(false)),
            b'n' => self.literal("null", DynamicValue::None),
            b'-' | b'0'..=b'9' => self.number(),
            _ => self.err("unexpected character"),
        }
    }

    fn literal(&mut self, word: &str, v: DynamicValue) -> Result<DynamicValue, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            self.err(&format!("expected `{}`", word))
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Result<DynamicValue, String> {
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        if self.i < self.b.len() && self.b[self.i] == b'0' {
            self.i += 1;
            if self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                return self.err("leading zeros are not allowed");
            }
        } else if self.digits() == 0 {
            return self.err("expected a digit");
        }
        let mut is_float = false;
        if self.i < self.b.len() && self.b[self.i] == b'.' {
            is_float = true;
            self.i += 1;
            if self.digits() == 0 {
                return self.err("expected a digit after `.`");
            }
        }
        if self.i < self.b.len() && (self.b[self.i] == b'e' || self.b[self.i] == b'E') {
            is_float = true;
            self.i += 1;
            if self.i < self.b.len() && (self.b[self.i] == b'+' || self.b[self.i] == b'-') {
                self.i += 1;
            }
            if self.digits() == 0 {
                return self.err("expected a digit in the exponent");
            }
        }
        let text = &self.src[start..self.i];
        if !is_float {
            if let Ok(n) = text.parse::<i64>() {
                return Ok(DynamicValue::Int(n));
            }
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(DynamicValue::Float(f)),
            _ => self.err("number out of range"),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.i + 4 > self.b.len() {
            return self.err("truncated \\u escape");
        }
        let mut n = 0u32;
        for k in 0..4 {
            match (self.b[self.i + k] as char).to_digit(16) {
                Some(d) => n = n * 16 + d,
                None => return self.err("invalid \\u escape"),
            }
        }
        self.i += 4;
        Ok(n)
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            if self.i >= self.b.len() {
                return self.err("unterminated string");
            }
            let c = self.b[self.i];
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    if self.i >= self.b.len() {
                        return self.err("unterminated string");
                    }
                    let e = self.b[self.i];
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                if self.b[self.i..].starts_with(b"\\u") {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return self.err("invalid low surrogate");
                                    }
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    return self.err("lone high surrogate");
                                }
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return self.err("lone low surrogate");
                            } else {
                                hi
                            };
                            match char::from_u32(cp) {
                                Some(ch) => out.push(ch),
                                None => return self.err("invalid code point"),
                            }
                        }
                        _ => {
                            self.i -= 1;
                            return self.err("invalid escape");
                        }
                    }
                }
                0..=0x1f => return self.err("control character in string"),
                _ => {
                    // copy a whole UTF-8 character (the source is a valid &str)
                    let ch = self.src[self.i..].chars().next().unwrap();
                    out.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<DynamicValue, String> {
        if depth >= __OX_JSON_MAX_DEPTH {
            return self.err("nesting is too deep");
        }
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == b']' {
            self.i += 1;
            return Ok(DynamicValue::list(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.ws();
            if self.i >= self.b.len() {
                return self.err("unterminated array");
            }
            match self.b[self.i] {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Ok(DynamicValue::list(items));
                }
                _ => return self.err("expected `,` or `]`"),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<DynamicValue, String> {
        if depth >= __OX_JSON_MAX_DEPTH {
            return self.err("nesting is too deep");
        }
        self.i += 1;
        let mut entries: Vec<(DynamicValue, DynamicValue)> = Vec::new();
        let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == b'}' {
            self.i += 1;
            return Ok(__ox_map_from(entries));
        }
        loop {
            self.ws();
            if self.i >= self.b.len() || self.b[self.i] != b'"' {
                return self.err("expected a string key");
            }
            let key = self.string()?;
            self.ws();
            if self.i >= self.b.len() || self.b[self.i] != b':' {
                return self.err("expected `:`");
            }
            self.i += 1;
            let v = self.value(depth + 1)?;
            match index.get(&key) {
                Some(&at) => entries[at].1 = v,
                None => {
                    index.insert(key.clone(), entries.len());
                    entries.push((DynamicValue::Str(key), v));
                }
            }
            self.ws();
            if self.i >= self.b.len() {
                return self.err("unterminated object");
            }
            match self.b[self.i] {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Ok(__ox_map_from(entries));
                }
                _ => return self.err("expected `,` or `}`"),
            }
        }
    }
}

// json_parse(text) -> Result: objects -> Map, arrays -> List, integers -> Int,
// other numbers -> Float, strings, bools, null -> None.
pub fn __ox_json_parse(text: DynamicValue) -> DynamicValue {
    let text = __ox_str_arg(&text, "json_parse");
    let mut p = __OxJsonParser { src: &text, b: text.as_bytes(), i: 0 };
    let result = p.value(0).and_then(|v| {
        p.ws();
        if p.i < p.b.len() {
            p.err("unexpected trailing characters")
        } else {
            Ok(v)
        }
    });
    match result {
        Ok(v) => __ox_result_ok(v),
        Err(m) => __ox_result_err(m),
    }
}

fn __ox_json_escape(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn __ox_json_write(v: &DynamicValue, pretty: bool, level: usize, out: &mut String) -> Result<(), String> {
    if level > __OX_JSON_MAX_DEPTH * 2 {
        return Err("value is nested too deeply to serialize".to_string());
    }
    let nl = |out: &mut String, lvl: usize| {
        if pretty {
            out.push('\n');
            for _ in 0..lvl {
                out.push_str("  ");
            }
        }
    };
    match v {
        DynamicValue::None => out.push_str("null"),
        DynamicValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        DynamicValue::Int(n) => out.push_str(&n.to_string()),
        DynamicValue::Float(f) => {
            if !f.is_finite() {
                return Err("cannot serialize a non-finite Float (NaN or infinity) as JSON".to_string());
            }
            out.push_str(&format!("{:?}", f));
        }
        DynamicValue::Str(s) => __ox_json_escape(s, out),
        DynamicValue::List(items) | DynamicValue::Tuple(items) => {
            if items.is_empty() {
                out.push_str("[]");
            } else {
                out.push('[');
                for (k, item) in items.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    nl(out, level + 1);
                    __ox_json_write(item, pretty, level + 1, out)?;
                }
                nl(out, level);
                out.push(']');
            }
        }
        DynamicValue::Map(entries) => {
            if entries.len() == 0 {
                out.push_str("{}");
            } else {
                out.push('{');
                for (k, (key, val)) in entries.iter().enumerate() {
                    let key = match key {
                        DynamicValue::Str(s) => s,
                        other => return Err(format!("JSON object keys must be Strings, got {:?}", other)),
                    };
                    if k > 0 {
                        out.push(',');
                    }
                    nl(out, level + 1);
                    __ox_json_escape(key, out);
                    out.push(':');
                    if pretty {
                        out.push(' ');
                    }
                    __ox_json_write(val, pretty, level + 1, out)?;
                }
                nl(out, level);
                out.push('}');
            }
        }
        DynamicValue::Struct(_, fields) => {
            if fields.is_empty() {
                out.push_str("{}");
            } else {
                out.push('{');
                for (k, (name, val)) in fields.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    nl(out, level + 1);
                    __ox_json_escape(name, out);
                    out.push(':');
                    if pretty {
                        out.push(' ');
                    }
                    __ox_json_write(val, pretty, level + 1, out)?;
                }
                nl(out, level);
                out.push('}');
            }
        }
        DynamicValue::Enum(name, variant, _) => {
            return Err(format!("cannot serialize the enum value {}::{} as JSON", name, variant));
        }
        DynamicValue::Function(_) => return Err("cannot serialize a function as JSON".to_string()),
    }
    Ok(())
}

fn __ox_json_emit(v: DynamicValue, pretty: bool) -> DynamicValue {
    let mut out = String::new();
    match __ox_json_write(&v, pretty, 0, &mut out) {
        Ok(()) => __ox_result_ok(DynamicValue::Str(out)),
        Err(m) => __ox_result_err(m),
    }
}

pub fn __ox_json_stringify(v: DynamicValue) -> DynamicValue {
    __ox_json_emit(v, false)
}

pub fn __ox_json_pretty(v: DynamicValue) -> DynamicValue {
    __ox_json_emit(v, true)
}

// ------------------------------------------------------------------ HTTP
// Minimal blocking HTTP/1.1 GET over std::net::TcpStream. Plain http only:
// there is no TLS, redirects are not followed, and it is unavailable on wasm.
fn __ox_http_fetch(url: &str) -> Result<DynamicValue, String> {
    use std::io::{Read, Write};
    use std::net::ToSocketAddrs;
    if cfg!(target_family = "wasm") {
        return Err("http_get is not supported under wasm (no sockets)".to_string());
    }
    let rest = match url.strip_prefix("http://") {
        Some(r) => r,
        None => {
            return Err(if url.starts_with("https://") {
                "https is not supported: http_get speaks plain http only (no TLS)".to_string()
            } else {
                format!("unsupported or malformed url `{}` (expected http://host[:port]/path)", url)
            });
        }
    };
    let (authority, path) = match rest.find(|c| c == '/' || c == '?') {
        Some(p) if rest.as_bytes()[p] == b'/' => (&rest[..p], rest[p..].to_string()),
        Some(p) => (&rest[..p], format!("/{}", &rest[p..])),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() || authority.contains(char::is_whitespace) || authority.contains('@') {
        return Err(format!("malformed url `{}`", url));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(n) => (h, n),
            Err(_) => return Err(format!("invalid port `{}` in url `{}`", p, url)),
        },
        None => (authority, 80u16),
    };
    if host.is_empty() || path.contains(char::is_whitespace) {
        return Err(format!("malformed url `{}`", url));
    }
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {}", host, e))?
        .next()
        .ok_or_else(|| format!("cannot resolve {}", host))?;
    let timeout = std::time::Duration::from_secs(20);
    let mut stream = std::net::TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("cannot connect to {}:{}: {}", host, port, e))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let host_header = if port == 80 { host.to_string() } else { format!("{}:{}", host, port) };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: oxidized\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        path, host_header
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("request failed: {}", e))?;
    let mut raw = Vec::new();
    let mut limited = (&mut stream).take(64 * 1024 * 1024);
    if let Err(e) = limited.read_to_end(&mut raw) {
        if raw.is_empty() {
            return Err(format!("read failed: {}", e));
        }
    }
    let split = match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => p,
        None => return Err("malformed response: no end of headers".to_string()),
    };
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body_raw = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let version_ok = parts.next().map_or(false, |v| v.starts_with("HTTP/"));
    let status = match (version_ok, parts.next().and_then(|s| s.parse::<i64>().ok())) {
        (true, Some(n)) => n,
        _ => return Err(format!("malformed status line `{}`", status_line)),
    };
    let mut headers: Vec<(DynamicValue, DynamicValue)> = Vec::new();
    let mut chunked = false;
    let mut length: Option<usize> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_lowercase();
            let v = v.trim().to_string();
            if k == "transfer-encoding" && v.to_lowercase().contains("chunked") {
                chunked = true;
            }
            if k == "content-length" {
                length = v.parse::<usize>().ok();
            }
            match headers.iter_mut().find(|(hk, _)| matches!(hk, DynamicValue::Str(s) if *s == k)) {
                Some((_, hv)) => {
                    let joined = format!("{}, {}", hv.to_string(), v);
                    *hv = DynamicValue::Str(joined);
                }
                None => headers.push((DynamicValue::Str(k), DynamicValue::Str(v))),
            }
        }
    }
    let body_bytes: Vec<u8> = if chunked {
        let mut out = Vec::new();
        let mut pos = 0usize;
        loop {
            let eol = match body_raw[pos..].windows(2).position(|w| w == b"\r\n") {
                Some(p) => pos + p,
                None => return Err("malformed chunked body".to_string()),
            };
            let size_text = String::from_utf8_lossy(&body_raw[pos..eol]).to_string();
            let size_text = size_text.split(';').next().unwrap_or("").trim().to_string();
            let size = usize::from_str_radix(&size_text, 16).map_err(|_| "malformed chunk size".to_string())?;
            pos = eol + 2;
            if size == 0 {
                break;
            }
            if pos + size > body_raw.len() {
                return Err("truncated chunked body".to_string());
            }
            out.extend_from_slice(&body_raw[pos..pos + size]);
            pos += size + 2;
            if pos > body_raw.len() {
                return Err("truncated chunked body".to_string());
            }
        }
        out
    } else if let Some(n) = length {
        body_raw[..n.min(body_raw.len())].to_vec()
    } else {
        body_raw.to_vec()
    };
    let body = String::from_utf8_lossy(&body_bytes).to_string();
    Ok(__ox_map_from(vec![
        (DynamicValue::Str("status".to_string()), DynamicValue::Int(status)),
        (DynamicValue::Str("headers".to_string()), __ox_map_from(headers)),
        (DynamicValue::Str("body".to_string()), DynamicValue::Str(body)),
    ]))
}

pub fn __ox_http_get(url: DynamicValue) -> DynamicValue {
    let u = __ox_str_arg(&url, "http_get");
    match __ox_http_fetch(&u) {
        Ok(v) => __ox_result_ok(v),
        Err(m) => __ox_result_err(m),
    }
}
// ------------------------------------------------------------ http_serve
// HTTP/1.1 server on 127.0.0.1 with keep-alive support. Each request is
// handed to the handler as a Map {method, path, query, headers, body}; the
// handler returns a String (sent as 200 text/plain) or a Map
// {status, body, headers}. Connections loop requests until Connection: close,
// HTTP/1.0, idle timeout (5s), or 100 requests per connection.
// Blocks while serving; returns Err only if binding fails.
// Err(status) is the HTTP status to answer with: 400 malformed, 408 stalled
// or timed out, 413 body too large, 431 headers too large.
fn __ox_http_read_request(stream: &mut std::net::TcpStream) -> Result<(String, String, Vec<(String, String)>, String, bool), i64> {
    use std::io::Read;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let started = std::time::Instant::now();
    let timed_out = |e: &std::io::Error| matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut);
    let head_end;
    loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            head_end = p;
            break;
        }
        if buf.len() > 65536 {
            return Err(431);
        }
        // Overall budget: a client dribbling one byte at a time can't hold a thread forever.
        if started.elapsed() > std::time::Duration::from_secs(15) {
            return Err(408);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(400),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(if timed_out(&e) { 408 } else { 400 }),
        }
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(400)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(400)?.to_string();
    let target = parts.next().ok_or(400)?.to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some(i) = line.find(':') {
            headers.push((line[..i].trim().to_lowercase(), line[i + 1..].trim().to_string()));
        }
    }
    let clen: usize = match headers.iter().find(|(k, _)| k.as_str() == "content-length") {
        Some((_, v)) => v.parse().map_err(|_| 400)?,
        None => 0,
    };
    if clen > 16 * 1024 * 1024 {
        return Err(413);
    }
    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
    while body.len() < clen {
        if started.elapsed() > std::time::Duration::from_secs(60) {
            return Err(408);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(400),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(if timed_out(&e) { 408 } else { 400 }),
        }
    }
    body.truncate(clen);
    
    // Determine if we should keep the connection alive.
    // Default: HTTP/1.1 keeps alive, HTTP/1.0 closes.
    let http_version = request_line.split_whitespace().nth(2).unwrap_or("HTTP/1.1");
    let mut keep_alive = http_version.contains("1.1");
    
    // Check Connection header: explicit "close" means no keep-alive.
    if let Some((_, conn_val)) = headers.iter().find(|(k, _)| k == "connection") {
        let conn_lower = conn_val.to_lowercase();
        if conn_lower.contains("close") {
            keep_alive = false;
        }
    }
    
    Ok((method, target, headers, String::from_utf8_lossy(&body).to_string(), keep_alive))
}

fn __ox_http_build_req(method: &str, target: &str, headers: &[(String, String)], body: &str) -> DynamicValue {
    let (path, query) = match target.find('?') {
        Some(i) => (&target[..i], &target[i + 1..]),
        None => (target, ""),
    };
    let hm = __ox_map_from(
        headers
            .iter()
            .map(|(k, v)| (DynamicValue::Str(k.clone()), DynamicValue::Str(v.clone())))
            .collect(),
    );
    __ox_map_from(vec![
        (DynamicValue::Str("method".to_string()), DynamicValue::Str(method.to_string())),
        (DynamicValue::Str("path".to_string()), DynamicValue::Str(path.to_string())),
        (DynamicValue::Str("query".to_string()), DynamicValue::Str(query.to_string())),
        (DynamicValue::Str("headers".to_string()), hm),
        (DynamicValue::Str("body".to_string()), DynamicValue::Str(body.to_string())),
    ])
}

fn __ox_http_reply(v: DynamicValue) -> (i64, Vec<(String, String)>, String) {
    match v {
        DynamicValue::Str(s) => (200, Vec::new(), s),
        DynamicValue::Struct(ref name, ref fields) if name == "Response" => {
            let status = fields
                .iter()
                .find(|(n, _)| n == "status")
                .and_then(|(_, v)| {
                    if let DynamicValue::Int(i) = v {
                        Some(*i)
                    } else {
                        None
                    }
                })
                .unwrap_or(200);
            let body = fields
                .iter()
                .find(|(n, _)| n == "body")
                .and_then(|(_, v)| {
                    if let DynamicValue::Str(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let mut hdrs: Vec<(String, String)> = Vec::new();
            if let Some((_, DynamicValue::Map(hm))) = fields.iter().find(|(n, _)| n == "headers") {
                for (k, v) in hm.iter() {
                    if let DynamicValue::Str(k) = k {
                        match v {
                            DynamicValue::Str(v) => hdrs.push((k.clone(), v.clone())),
                            DynamicValue::List(items) => {
                                for it in items.iter() {
                                    if let DynamicValue::Str(s) = it {
                                        hdrs.push((k.clone(), s.clone()));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            (status, hdrs, body)
        }
        DynamicValue::Map(m) => {
            let status = match m.get(&DynamicValue::Str("status".to_string())) {
                Some(DynamicValue::Int(i)) => *i,
                _ => 200,
            };
            let body = match m.get(&DynamicValue::Str("body".to_string())) {
                Some(DynamicValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            let mut hdrs: Vec<(String, String)> = Vec::new();
            if let Some(DynamicValue::Map(hm)) = m.get(&DynamicValue::Str("headers".to_string())) {
                for (k, v) in hm.iter() {
                    if let DynamicValue::Str(k) = k {
                        match v {
                            DynamicValue::Str(v) => hdrs.push((k.clone(), v.clone())),
                            DynamicValue::List(items) => {
                                for it in items.iter() {
                                    if let DynamicValue::Str(s) = it {
                                        hdrs.push((k.clone(), s.clone()));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            (status, hdrs, body)
        }
        _ => (500, Vec::new(), "handler must return a String, Response struct, or a Map".to_string()),
    }
}

fn __ox_http_write_response(stream: &mut std::net::TcpStream, status: i64, headers: Vec<(String, String)>, body: &str, keep_alive: bool) {
    use std::io::Write;
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let mut out = format!("HTTP/1.1 {} {}\r\n", status, reason);
    let mut has_ct = false;
    for (k, v) in &headers {
        if k.to_lowercase() == "content-type" {
            has_ct = true;
        }
        let k: String = k.chars().filter(|c| *c != '\r' && *c != '\n').collect();
        let v: String = v.chars().filter(|c| *c != '\r' && *c != '\n').collect();
        out.push_str(&format!("{}: {}\r\n", k, v));
    }
    if !has_ct {
        out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    }
    out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    if keep_alive {
        out.push_str("Connection: keep-alive\r\n");
    } else {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    out.push_str(body);
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.flush();
}

type __OxReply = (i64, Vec<(String, String)>, String);
type __OxJob = (String, String, Vec<(String, String)>, String, std::sync::mpsc::Sender<__OxReply>);

// Concurrency model: one thread per connection does the socket I/O (read,
// parse, write), so a slow or stalled client never blocks anyone else. The
// handler itself always runs on the calling thread, one request at a time,
// because the value model (Rc) is not thread-safe. A panic inside the handler
// becomes a 500 and the server keeps running.
pub fn __ox_http_serve(port: DynamicValue, handler: DynamicValue) -> DynamicValue {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    if cfg!(target_family = "wasm") {
        return __ox_result_err("http_serve is not supported under wasm (no sockets)".to_string());
    }
    let port_n: u16 = match port {
        DynamicValue::Int(n) if (0..=65535).contains(&n) => n as u16,
        _ => return __ox_result_err("http_serve: port must be an Int in 0..=65535".to_string()),
    };
    let f = match handler {
        DynamicValue::Function(f) => f,
        _ => return __ox_result_err("http_serve: handler must be a function".to_string()),
    };
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port_n)) {
        Ok(l) => l,
        Err(e) => return __ox_result_err(format!("http_serve: cannot bind 127.0.0.1:{}: {}", port_n, e)),
    };
    eprintln!("http_serve: listening on http://127.0.0.1:{}", port_n);
    let (tx, rx) = mpsc::channel::<__OxJob>();
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let mut stream = match conn {
                Ok(s) => s,
                Err(_) => continue,
            };
            if active.load(Ordering::SeqCst) >= 256 {
                __ox_http_write_response(&mut stream, 503, Vec::new(), "server busy", false);
                continue;
            }
            active.fetch_add(1, Ordering::SeqCst);
            let tx = tx.clone();
            let active = active.clone();
            std::thread::spawn(move || {
                // Keep-alive loop: max 100 requests per connection, 5s idle timeout between requests.
                let mut req_count = 0;
                loop {
                    // Set timeout for reading requests: 5s idle between requests.
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                    match __ox_http_read_request(&mut stream) {
                        Ok((method, target, headers, body, keep_alive_req)) => {
                            req_count += 1;
                            let (rtx, rrx) = mpsc::channel::<__OxReply>();
                            if tx.send((method, target, headers, body, rtx)).is_ok() {
                                match rrx.recv() {
                                    Ok((status, hdrs, text)) => {
                                        // Send response, respecting keep_alive flag and max requests.
                                        let should_close = !keep_alive_req || req_count >= 100;
                                        __ox_http_write_response(&mut stream, status, hdrs, &text, !should_close);
                                        if should_close {
                                            break;
                                        }
                                        // Continue loop to read next request (with 5s timeout).
                                    }
                                    Err(_) => {
                                        __ox_http_write_response(&mut stream, 500, Vec::new(), "internal error", false);
                                        break;
                                    }
                                }
                            } else {
                                break;
                            }
                        }
                        Err(code) => {
                            // Read error: if it's a timeout (408) after a request, silently close.
                            // If it's a real protocol error, send an error response.
                            if code != 408 || req_count == 0 {
                                __ox_http_write_response(&mut stream, code, Vec::new(), "request rejected", false);
                            }
                            break;
                        }
                    }
                }
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    for (method, target, headers, body, rtx) in rx {
        let t0 = std::time::Instant::now();
        let req = __ox_http_build_req(&method, &target, &headers, &body);
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.call(vec![req])));
        let reply = match out {
            Ok(v) => __ox_http_reply(v),
            Err(_) => (500, Vec::new(), "internal server error".to_string()),
        };
        eprintln!("{} {} -> {} ({} ms)", method, target, reply.0, t0.elapsed().as_millis());
        let _ = rtx.send(reply);
    }
    __ox_result_ok(DynamicValue::None)
}
// ----------------------------------------------------------------- routing
// Percent-decodes `s` (`+` becomes a space only when `plus` is set, i.e. in
// query strings). Malformed escapes are kept literally.
fn __ox_pct_decode(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        if c == b'+' && plus {
            out.push(b' ');
        } else {
            out.push(c);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// route_match("/api/items/:id", "/api/items/7") -> Ok({"id": "7"}); a
// non-matching path is Err("no match"). `:name` binds one segment (decoded),
// a final `*` binds the rest of the path under the key "*". Empty segments
// (leading, trailing or doubled slashes) are ignored, so "/a/" matches "/a".
pub fn __ox_route_match(pattern: DynamicValue, path: DynamicValue) -> DynamicValue {
    let pat = __ox_str_arg(&pattern, "route_match");
    let pth = __ox_str_arg(&path, "route_match");
    let psegs: Vec<&str> = pat.split('/').filter(|s| !s.is_empty()).collect();
    let tsegs: Vec<&str> = pth.split('/').filter(|s| !s.is_empty()).collect();
    let mut params: Vec<(DynamicValue, DynamicValue)> = Vec::new();
    for (i, ps) in psegs.iter().enumerate() {
        if *ps == "*" && i == psegs.len() - 1 {
            let rest = if i <= tsegs.len() { tsegs[i..].join("/") } else { String::new() };
            params.push((DynamicValue::Str("*".to_string()), DynamicValue::Str(__ox_pct_decode(&rest, false))));
            return __ox_result_ok(__ox_map_from(params));
        }
        if i >= tsegs.len() {
            return __ox_result_err("no match".to_string());
        }
        if let Some(name) = ps.strip_prefix(':') {
            params.push((DynamicValue::Str(name.to_string()), DynamicValue::Str(__ox_pct_decode(tsegs[i], false))));
        } else if *ps != tsegs[i] {
            return __ox_result_err("no match".to_string());
        }
    }
    if tsegs.len() != psegs.len() {
        return __ox_result_err("no match".to_string());
    }
    __ox_result_ok(__ox_map_from(params))
}

// query_parse("a=1&b=x%20y") -> {"a": "1", "b": "x y"}. Values are Strings;
// a key without `=` maps to ""; a repeated key keeps the last value.
pub fn __ox_query_parse(query: DynamicValue) -> DynamicValue {
    let q = __ox_str_arg(&query, "query_parse");
    let mut pairs: Vec<(DynamicValue, DynamicValue)> = Vec::new();
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.find('=') {
            Some(i) => (&part[..i], &part[i + 1..]),
            None => (part, ""),
        };
        pairs.push((DynamicValue::Str(__ox_pct_decode(k, true)), DynamicValue::Str(__ox_pct_decode(v, true))));
    }
    __ox_map_from(pairs)
}
// ---------------------------------------------------------- cookies / crypto
// cookie_parse("a=1; b=x%20y") -> {"a": "1", "b": "x y"}. Values are decoded
// Strings; pairs without `=` are skipped; the first duplicate wins.
pub fn __ox_cookie_parse(header: DynamicValue) -> DynamicValue {
    let h = __ox_str_arg(&header, "cookie_parse");
    let mut pairs: Vec<(DynamicValue, DynamicValue)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for part in h.split(';') {
        let part = part.trim();
        if let Some(i) = part.find('=') {
            let k = part[..i].trim().to_string();
            if k.is_empty() || seen.contains(&k) {
                continue;
            }
            seen.push(k.clone());
            pairs.push((DynamicValue::Str(k), DynamicValue::Str(__ox_pct_decode(part[i + 1..].trim(), false))));
        }
    }
    __ox_map_from(pairs)
}

fn __ox_sha256_bytes(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[4 * i..4 * i + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

fn __ox_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// sha256("abc") -> 64-char lowercase hex digest of the UTF-8 bytes.
pub fn __ox_sha256(s: DynamicValue) -> DynamicValue {
    let t = __ox_str_arg(&s, "sha256");
    DynamicValue::Str(__ox_hex(&__ox_sha256_bytes(t.as_bytes())))
}

// hmac_sha256(key, message) -> hex MAC (RFC 2104). Sign cookies with it and
// compare with const_time_eq.
pub fn __ox_hmac_sha256(key: DynamicValue, msg: DynamicValue) -> DynamicValue {
    let k = __ox_str_arg(&key, "hmac_sha256");
    let m = __ox_str_arg(&msg, "hmac_sha256");
    let mut kb = k.into_bytes();
    if kb.len() > 64 {
        kb = __ox_sha256_bytes(&kb).to_vec();
    }
    kb.resize(64, 0);
    let mut inner: Vec<u8> = kb.iter().map(|b| b ^ 0x36).collect();
    inner.extend_from_slice(m.as_bytes());
    let ih = __ox_sha256_bytes(&inner);
    let mut outer: Vec<u8> = kb.iter().map(|b| b ^ 0x5c).collect();
    outer.extend_from_slice(&ih);
    DynamicValue::Str(__ox_hex(&__ox_sha256_bytes(&outer)))
}

// const_time_eq(a, b) -> Bool; does not stop at the first differing byte.
pub fn __ox_const_time_eq(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    let x = __ox_str_arg(&a, "const_time_eq");
    let y = __ox_str_arg(&b, "const_time_eq");
    let mut diff = (x.len() ^ y.len()) as u8;
    for (p, q) in x.bytes().zip(y.bytes()) {
        diff |= p ^ q;
    }
    DynamicValue::Bool(diff == 0)
}

// random_hex(n) -> 2n hex chars. Each 8 bytes come from SHA-256 over fresh
// OS-seeded std RandomState output, the clock and a counter. Fine for session
// ids and CSRF tokens; not a replacement for a vetted CSPRNG for key material.
pub fn __ox_random_hex(n: DynamicValue) -> DynamicValue {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let want = match n {
        DynamicValue::Int(i) if (0..=4096).contains(&i) => i as usize,
        _ => panic!("random_hex() expects an Int in 0..=4096"),
    };
    if let Some(bytes) = __ox_os_random(want) {
        return DynamicValue::Str(__ox_hex(&bytes));
    }
    let mut out: Vec<u8> = Vec::with_capacity(want + 32);
    while out.len() < want {
        let mut seed: Vec<u8> = Vec::new();
        for _ in 0..4 {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(CTR.fetch_add(1, Ordering::SeqCst));
            seed.extend_from_slice(&h.finish().to_le_bytes());
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        seed.extend_from_slice(&nanos.to_le_bytes());
        out.extend_from_slice(&__ox_sha256_bytes(&seed));
    }
    out.truncate(want);
    DynamicValue::Str(__ox_hex(&out))
}
// ----------------------------------------------------------- http_serve_pool
// http_serve_pool(port, workers, handler): like http_serve, but N worker
// threads run the handler in parallel. `handler` is any function value (a
// named fn or a closure); the same function is shared by every worker, so
// values it captures are shared too (read-only captures are free; for
// mutable state use shared_get/shared_set/shared_update or with_lock).
type __OxPoolReply = (i64, Vec<(String, String)>, String);
type __OxPoolJob = (String, String, Vec<(String, String)>, String, std::sync::mpsc::Sender<__OxPoolReply>);

pub fn __ox_http_serve_pool(port: DynamicValue, workers: DynamicValue, handler: DynamicValue) -> DynamicValue {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    let handler = match handler {
        DynamicValue::Function(f) => f,
        other => return __ox_result_err(format!("http_serve_pool: the handler must be a function, got {:?}", other)),
    };
    if cfg!(target_family = "wasm") {
        return __ox_result_err("http_serve_pool is not supported under wasm (no sockets)".to_string());
    }
    let port_n: u16 = match port {
        DynamicValue::Int(n) if (0..=65535).contains(&n) => n as u16,
        _ => return __ox_result_err("http_serve_pool: port must be an Int in 0..=65535".to_string()),
    };
    let n_workers: usize = match workers {
        DynamicValue::Int(n) if (1..=64).contains(&n) => n as usize,
        _ => return __ox_result_err("http_serve_pool: workers must be an Int in 1..=64".to_string()),
    };
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port_n)) {
        Ok(l) => l,
        Err(e) => return __ox_result_err(format!("http_serve_pool: cannot bind 127.0.0.1:{}: {}", port_n, e)),
    };
    eprintln!("http_serve_pool: listening on http://127.0.0.1:{} with {} workers", port_n, n_workers);
    let (tx, rx) = mpsc::channel::<__OxPoolJob>();
    let rx = Arc::new(Mutex::new(rx));
    for id in 0..n_workers {
        let rx = Arc::clone(&rx);
        let handler = handler.clone();
        let spawned = std::thread::Builder::new().stack_size(1 << 30).spawn(move || {
            __ox_set_stack_base();
            loop {
                // Hold the queue lock only while waiting for a job, never while running one.
                let job = match rx.lock() {
                    Ok(guard) => guard.recv(),
                    Err(_) => return,
                };
                let (method, target, headers, body, rtx) = match job {
                    Ok(j) => j,
                    Err(_) => return,
                };
                let t0 = std::time::Instant::now();
                let req = __ox_http_build_req(&method, &target, &headers, &body);
                let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.call(vec![req])));
                let reply = match out {
                    Ok(v) => __ox_http_reply(v),
                    Err(_) => (500, Vec::new(), "internal server error".to_string()),
                };
                eprintln!("{} {} -> {} ({} ms) [w{}]", method, target, reply.0, t0.elapsed().as_millis(), id);
                let _ = rtx.send(reply);
            }
        });
        if let Err(e) = spawned {
            return __ox_result_err(format!("http_serve_pool: cannot start worker {}: {}", id, e));
        }
    }
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let mut stream = match conn {
            Ok(s) => s,
            Err(_) => continue,
        };
        if active.load(Ordering::SeqCst) >= 256 {
            __ox_http_write_response(&mut stream, 503, Vec::new(), "server busy", false);
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let tx = tx.clone();
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            // Keep-alive loop: max 100 requests per connection, 5s idle timeout between requests.
            let mut req_count = 0;
            loop {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                match __ox_http_read_request(&mut stream) {
                    Ok((method, target, headers, body, keep_alive_req)) => {
                        req_count += 1;
                        let (rtx, rrx) = mpsc::channel::<__OxPoolReply>();
                        if tx.send((method, target, headers, body, rtx)).is_ok() {
                            match rrx.recv() {
                                Ok((status, hdrs, text)) => {
                                    let should_close = !keep_alive_req || req_count >= 100;
                                    __ox_http_write_response(&mut stream, status, hdrs, &text, !should_close);
                                    if should_close {
                                        break;
                                    }
                                }
                                Err(_) => {
                                    __ox_http_write_response(&mut stream, 500, Vec::new(), "internal error", false);
                                    break;
                                }
                            }
                        } else {
                            break;
                        }
                    }
                    Err(code) => {
                        if code != 408 || req_count == 0 {
                            __ox_http_write_response(&mut stream, code, Vec::new(), "request rejected", false);
                        }
                        break;
                    }
                }
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    __ox_result_ok(DynamicValue::None)
}

// with_lock(name, f): runs `f()` while holding the process-wide lock called
// `name`, and returns its result. Use it around read-modify-write sections
// (e.g. load items.json, change, save) when several workers share state.
// The lock is released even if `f` panics. It is NOT re-entrant: calling
// with_lock with the same name from inside `f` deadlocks.
pub fn __ox_with_lock(name: DynamicValue, f: DynamicValue) -> DynamicValue {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let key = __ox_str_arg(&name, "with_lock");
    let func = match f {
        DynamicValue::Function(func) => func,
        other => panic!("type error: with_lock() expects a function, got {:?}", other),
    };
    let lock = {
        let mut table = LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(table.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
    };
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    func.call(Vec::new())
}

// assert_eq(a, b) / assert_ne(a, b): panic with both values when the check fails.
pub fn __ox_assert_eq(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    if a != b {
        let (l, r) = (a.to_string(), b.to_string());
        if l == r {
            panic!("assert_eq failed: left={} ({:?}), right={} ({:?})", l, a, r, b);
        }
        panic!("assert_eq failed: left={}, right={}", l, r);
    }
    DynamicValue::None
}

pub fn __ox_assert_ne(a: DynamicValue, b: DynamicValue) -> DynamicValue {
    if a == b {
        panic!("assert_ne failed: both sides are {}", a.to_string());
    }
    DynamicValue::None
}

// ------------------------------------------------------------------ SQLite
// Databases through the system SQLite library, loaded at RUNTIME with
// LoadLibraryA/GetProcAddress (hand-declared, no crates, nothing to link), so a
// program that never calls db_* needs nothing and a missing library is an
// ordinary `Err` from db_open. Values always travel as bound parameters,
// never spliced into SQL text. Windows only in this build.
#[allow(dead_code)]
mod __ox_sqlite {
    pub enum Val {
        Null,
        Int(i64),
        Float(f64),
        Text(String),
    }
    pub type Row = Vec<(String, Val)>;

    #[cfg(windows)]
    mod imp {
        use super::{Row, Val};
        use std::ffi::{c_char, c_int, c_void, CStr, CString};
        use std::sync::{Mutex, OnceLock};

        extern "system" {
            fn LoadLibraryA(name: *const c_char) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        }

        type Db = *mut c_void;
        type Stmt = *mut c_void;
        struct Api {
            initialize: unsafe extern "C" fn() -> c_int,
            open_v2: unsafe extern "C" fn(*const c_char, *mut Db, c_int, *const c_char) -> c_int,
            close_v2: unsafe extern "C" fn(Db) -> c_int,
            prepare_v2: unsafe extern "C" fn(Db, *const c_char, c_int, *mut Stmt, *mut *const c_char) -> c_int,
            step: unsafe extern "C" fn(Stmt) -> c_int,
            finalize: unsafe extern "C" fn(Stmt) -> c_int,
            errmsg: unsafe extern "C" fn(Db) -> *const c_char,
            bind_param_count: unsafe extern "C" fn(Stmt) -> c_int,
            bind_int64: unsafe extern "C" fn(Stmt, c_int, i64) -> c_int,
            bind_double: unsafe extern "C" fn(Stmt, c_int, f64) -> c_int,
            bind_text: unsafe extern "C" fn(Stmt, c_int, *const c_char, c_int, isize) -> c_int,
            bind_null: unsafe extern "C" fn(Stmt, c_int) -> c_int,
            column_count: unsafe extern "C" fn(Stmt) -> c_int,
            column_name: unsafe extern "C" fn(Stmt, c_int) -> *const c_char,
            column_type: unsafe extern "C" fn(Stmt, c_int) -> c_int,
            column_int64: unsafe extern "C" fn(Stmt, c_int) -> i64,
            column_double: unsafe extern "C" fn(Stmt, c_int) -> f64,
            column_text: unsafe extern "C" fn(Stmt, c_int) -> *const u8,
            column_blob: unsafe extern "C" fn(Stmt, c_int) -> *const u8,
            column_bytes: unsafe extern "C" fn(Stmt, c_int) -> c_int,
            total_changes: unsafe extern "C" fn(Db) -> c_int,
            last_insert_rowid: unsafe extern "C" fn(Db) -> i64,
            busy_timeout: unsafe extern "C" fn(Db, c_int) -> c_int,
        }

        unsafe fn load_api(path: &str) -> Result<Api, String> {
            let c = CString::new(path).map_err(|_| "path contains a NUL byte".to_string())?;
            let h = LoadLibraryA(c.as_ptr());
            if h.is_null() {
                return Err(format!("cannot load `{}`", path));
            }
            macro_rules! sym {
                ($name:literal) => {{
                    let p = GetProcAddress(h, concat!($name, "\0").as_ptr() as *const c_char);
                    if p.is_null() {
                        return Err(format!("`{}` has no function {} (is it a real SQLite 3?)", path, $name));
                    }
                    std::mem::transmute(p)
                }};
            }
            let api = Api {
                initialize: sym!("sqlite3_initialize"),
                open_v2: sym!("sqlite3_open_v2"),
                close_v2: sym!("sqlite3_close_v2"),
                prepare_v2: sym!("sqlite3_prepare_v2"),
                step: sym!("sqlite3_step"),
                finalize: sym!("sqlite3_finalize"),
                errmsg: sym!("sqlite3_errmsg"),
                bind_param_count: sym!("sqlite3_bind_parameter_count"),
                bind_int64: sym!("sqlite3_bind_int64"),
                bind_double: sym!("sqlite3_bind_double"),
                bind_text: sym!("sqlite3_bind_text"),
                bind_null: sym!("sqlite3_bind_null"),
                column_count: sym!("sqlite3_column_count"),
                column_name: sym!("sqlite3_column_name"),
                column_type: sym!("sqlite3_column_type"),
                column_int64: sym!("sqlite3_column_int64"),
                column_double: sym!("sqlite3_column_double"),
                column_text: sym!("sqlite3_column_text"),
                column_blob: sym!("sqlite3_column_blob"),
                column_bytes: sym!("sqlite3_column_bytes"),
                total_changes: sym!("sqlite3_total_changes"),
                last_insert_rowid: sym!("sqlite3_last_insert_rowid"),
                busy_timeout: sym!("sqlite3_busy_timeout"),
            };
            // Some builds (e.g. Python's) omit auto-initialization: calling anything else first crashes.
            let rc = (api.initialize)();
            if rc != 0 {
                return Err(format!("`{}`: sqlite3_initialize failed with code {}", path, rc));
            }
            Ok(api)
        }

        // Where to look: OXIDIZED_SQLITE_DLL (exclusively, if set), then the normal DLL search
        // path (app dir, system dirs, PATH), then any Python 3 install's DLLs folder.
        fn find_api() -> Result<Api, String> {
            if let Ok(p) = std::env::var("OXIDIZED_SQLITE_DLL") {
                return unsafe { load_api(&p) }.map_err(|e| format!("OXIDIZED_SQLITE_DLL: {}", e));
            }
            let mut tried: Vec<String> = vec!["sqlite3.dll".to_string()];
            if let Ok(local) = std::env::var("LOCALAPPDATA") {
                let root = std::path::Path::new(&local).join("Programs").join("Python");
                if let Ok(rd) = std::fs::read_dir(&root) {
                    let mut dirs: Vec<_> = rd.flatten().map(|e| e.path()).collect();
                    dirs.sort();
                    dirs.reverse();
                    for d in dirs {
                        let dll = d.join("DLLs").join("sqlite3.dll");
                        if dll.exists() {
                            tried.push(dll.display().to_string());
                        }
                    }
                }
            }
            for t in &tried {
                if let Ok(api) = unsafe { load_api(t) } {
                    return Ok(api);
                }
            }
            Err(format!(
                "SQLite library not found (tried: {}). Install a 64-bit sqlite3.dll on PATH or set OXIDIZED_SQLITE_DLL to its full path",
                tried.join(", ")
            ))
        }

        fn api() -> Result<&'static Api, String> {
            static API: OnceLock<Result<Api, String>> = OnceLock::new();
            API.get_or_init(find_api).as_ref().map_err(|e| e.clone())
        }

        // Open connections, by handle (index + 1). 0 marks a closed slot.
        static DBS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

        fn lookup(handle: i64) -> Result<Db, String> {
            let table = DBS.lock().unwrap_or_else(|e| e.into_inner());
            if handle >= 1 {
                if let Some(&p) = table.get(handle as usize - 1) {
                    if p != 0 {
                        return Ok(p as Db);
                    }
                }
            }
            Err(format!("invalid or closed database handle {}", handle))
        }

        unsafe fn msg(a: &Api, db: Db) -> String {
            let p = (a.errmsg)(db);
            if p.is_null() { "unknown SQLite error".to_string() } else { CStr::from_ptr(p).to_string_lossy().to_string() }
        }

        pub fn open(path: &str) -> Result<i64, String> {
            let a = api()?;
            let c = CString::new(path).map_err(|_| "path contains a NUL byte".to_string())?;
            unsafe {
                let mut db: Db = std::ptr::null_mut();
                // READWRITE | CREATE | FULLMUTEX
                let rc = (a.open_v2)(c.as_ptr(), &mut db, 0x2 | 0x4 | 0x10000, std::ptr::null());
                if rc != 0 {
                    let m = if db.is_null() { format!("SQLite error code {}", rc) } else { msg(a, db) };
                    if !db.is_null() {
                        (a.close_v2)(db);
                    }
                    return Err(format!("cannot open `{}`: {}", path, m));
                }
                (a.busy_timeout)(db, 5000);
                let mut table = DBS.lock().unwrap_or_else(|e| e.into_inner());
                table.push(db as usize);
                Ok(table.len() as i64)
            }
        }

        pub fn close(handle: i64) -> Result<(), String> {
            let a = api()?;
            let db = lookup(handle)?;
            unsafe {
                (a.close_v2)(db);
            }
            DBS.lock().unwrap_or_else(|e| e.into_inner())[handle as usize - 1] = 0;
            Ok(())
        }

        pub fn last_insert_id(handle: i64) -> Result<i64, String> {
            let a = api()?;
            let db = lookup(handle)?;
            Ok(unsafe { (a.last_insert_rowid)(db) })
        }

        unsafe fn bind_all(a: &Api, db: Db, stmt: Stmt, params: &[Val]) -> Result<(), String> {
            let want = (a.bind_param_count)(stmt) as usize;
            if want != params.len() {
                return Err(format!("the SQL has {} parameter(s) but {} value(s) were given", want, params.len()));
            }
            for (i, p) in params.iter().enumerate() {
                let idx = (i + 1) as c_int;
                let rc = match p {
                    Val::Null => (a.bind_null)(stmt, idx),
                    Val::Int(n) => (a.bind_int64)(stmt, idx, *n),
                    Val::Float(f) => (a.bind_double)(stmt, idx, *f),
                    // SQLITE_TRANSIENT (-1): SQLite copies the text before bind returns.
                    Val::Text(s) => (a.bind_text)(stmt, idx, s.as_ptr() as *const c_char, s.len() as c_int, -1),
                };
                if rc != 0 {
                    return Err(msg(a, db));
                }
            }
            Ok(())
        }

        unsafe fn read_row(a: &Api, stmt: Stmt) -> Row {
            let n = (a.column_count)(stmt);
            let mut row: Row = Vec::with_capacity(n as usize);
            for i in 0..n {
                let name = CStr::from_ptr((a.column_name)(stmt, i)).to_string_lossy().to_string();
                let v = match (a.column_type)(stmt, i) {
                    1 => Val::Int((a.column_int64)(stmt, i)),
                    2 => Val::Float((a.column_double)(stmt, i)),
                    3 => {
                        let p = (a.column_text)(stmt, i);
                        let len = (a.column_bytes)(stmt, i) as usize;
                        Val::Text(String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).to_string())
                    }
                    4 => {
                        // BLOBs come back as lowercase hex text.
                        let len = (a.column_bytes)(stmt, i) as usize;
                        let p = (a.column_blob)(stmt, i);
                        let bytes: &[u8] = if len == 0 || p.is_null() { &[] } else { std::slice::from_raw_parts(p, len) };
                        Val::Text(bytes.iter().map(|b| format!("{:02x}", b)).collect())
                    }
                    _ => Val::Null,
                };
                row.push((name, v));
            }
            row
        }

        /// Runs `sql`. `collect` = a query: exactly one statement, rows returned.
        /// Otherwise any number of statements (params only with a single one).
        fn run(handle: i64, sql: &str, params: &[Val], collect: bool) -> Result<(i64, Vec<Row>), String> {
            let a = api()?;
            let db = lookup(handle)?;
            let csql = CString::new(sql).map_err(|_| "SQL contains a NUL byte".to_string())?;
            unsafe {
                let end = csql.as_ptr().add(csql.as_bytes().len());
                let mut cur = csql.as_ptr();
                let before = (a.total_changes)(db);
                let mut rows: Vec<Row> = Vec::new();
                let mut count = 0usize;
                loop {
                    let mut stmt: Stmt = std::ptr::null_mut();
                    let mut tail: *const c_char = cur;
                    let rc = (a.prepare_v2)(db, cur, (end as usize - cur as usize) as c_int, &mut stmt, &mut tail);
                    if rc != 0 {
                        return Err(msg(a, db));
                    }
                    let consumed = tail as usize > cur as usize;
                    if !stmt.is_null() {
                        count += 1;
                        let rest = std::slice::from_raw_parts(tail as *const u8, end as usize - tail as usize);
                        let more = rest.iter().any(|b| !b.is_ascii_whitespace() && *b != b';');
                        if (collect || !params.is_empty()) && (count > 1 || more) {
                            (a.finalize)(stmt);
                            return Err(if collect {
                                "db_query takes exactly one statement".to_string()
                            } else {
                                "parameters can only be used with a single statement".to_string()
                            });
                        }
                        let bound = if count == 1 && !params.is_empty() {
                            bind_all(a, db, stmt, params)
                        } else if (a.bind_param_count)(stmt) != 0 {
                            Err(format!("the SQL has {} parameter(s) but none were given", (a.bind_param_count)(stmt)))
                        } else {
                            Ok(())
                        };
                        if let Err(e) = bound {
                            (a.finalize)(stmt);
                            return Err(e);
                        }
                        loop {
                            match (a.step)(stmt) {
                                100 => {
                                    if collect {
                                        rows.push(read_row(a, stmt));
                                    }
                                }
                                101 => break,
                                _ => {
                                    let m = msg(a, db);
                                    (a.finalize)(stmt);
                                    return Err(m);
                                }
                            }
                        }
                        (a.finalize)(stmt);
                    }
                    if !consumed || tail as usize >= end as usize {
                        break;
                    }
                    cur = tail;
                }
                if collect && count == 0 {
                    return Err("db_query needs a statement".to_string());
                }
                Ok((((a.total_changes)(db) - before) as i64, rows))
            }
        }

        pub fn exec(handle: i64, sql: &str, params: &[Val]) -> Result<i64, String> {
            run(handle, sql, params, false).map(|(n, _)| n)
        }
        pub fn query(handle: i64, sql: &str, params: &[Val]) -> Result<Vec<Row>, String> {
            run(handle, sql, params, true).map(|(_, r)| r)
        }
    }

    #[cfg(not(windows))]
    mod imp {
        use super::{Row, Val};
        const NO: &str = "databases are only supported by Windows builds of oxidized";
        pub fn open(_: &str) -> Result<i64, String> { Err(NO.to_string()) }
        pub fn close(_: i64) -> Result<(), String> { Err(NO.to_string()) }
        pub fn last_insert_id(_: i64) -> Result<i64, String> { Err(NO.to_string()) }
        pub fn exec(_: i64, _: &str, _: &[Val]) -> Result<i64, String> { Err(NO.to_string()) }
        pub fn query(_: i64, _: &str, _: &[Val]) -> Result<Vec<Row>, String> { Err(NO.to_string()) }
    }

    pub use self::imp::{close, exec, last_insert_id, open, query};
}

fn __ox_db_params(v: &DynamicValue, who: &str) -> Result<Vec<__ox_sqlite::Val>, String> {
    use self::__ox_sqlite::Val;
    let items = match v {
        DynamicValue::List(items) => items,
        other => return Err(format!("{}: the parameters must be a List, got {:?}", who, other)),
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, it) in items.iter().enumerate() {
        out.push(match it {
            DynamicValue::Int(n) => Val::Int(*n),
            DynamicValue::Float(f) => Val::Float(*f),
            DynamicValue::Str(s) => Val::Text(s.clone()),
            DynamicValue::Bool(b) => Val::Int(*b as i64),
            DynamicValue::None => Val::Null,
            other => return Err(format!("{}: parameter {} has an unsupported type ({:?}); use Int, Float, String, Bool or None", who, i + 1, other)),
        });
    }
    Ok(out)
}

fn __ox_db_handle(v: &DynamicValue, who: &str) -> Result<i64, String> {
    match v {
        DynamicValue::Int(n) => Ok(*n),
        other => Err(format!("{}: the database handle must be an Int, got {:?}", who, other)),
    }
}

// db_open(path) -> Ok(handle); ":memory:" gives a private in-memory database.
pub fn __ox_db_open(path: DynamicValue) -> DynamicValue {
    let p = __ox_str_arg(&path, "db_open");
    match __ox_sqlite::open(&p) {
        Ok(h) => __ox_result_ok(DynamicValue::Int(h)),
        Err(e) => __ox_result_err(format!("db_open: {}", e)),
    }
}

pub fn __ox_db_close(db: DynamicValue) -> DynamicValue {
    match __ox_db_handle(&db, "db_close").and_then(__ox_sqlite::close) {
        Ok(()) => __ox_result_ok(DynamicValue::None),
        Err(e) => __ox_result_err(format!("db_close: {}", e)),
    }
}

// db_exec(db, sql, params) -> Ok(rows changed). `?` placeholders bind `params`.
pub fn __ox_db_exec(db: DynamicValue, sql: DynamicValue, params: DynamicValue) -> DynamicValue {
    let r = (|| {
        let h = __ox_db_handle(&db, "db_exec")?;
        let s = __ox_str_arg(&sql, "db_exec");
        let p = __ox_db_params(&params, "db_exec")?;
        __ox_sqlite::exec(h, &s, &p)
    })();
    match r {
        Ok(n) => __ox_result_ok(DynamicValue::Int(n)),
        Err(e) => __ox_result_err(format!("db_exec: {}", e)),
    }
}

// db_query(db, sql, params) -> Ok(List of Maps column -> value).
pub fn __ox_db_query(db: DynamicValue, sql: DynamicValue, params: DynamicValue) -> DynamicValue {
    use self::__ox_sqlite::Val;
    let r = (|| {
        let h = __ox_db_handle(&db, "db_query")?;
        let s = __ox_str_arg(&sql, "db_query");
        let p = __ox_db_params(&params, "db_query")?;
        __ox_sqlite::query(h, &s, &p)
    })();
    match r {
        Ok(rows) => {
            let list: Vec<DynamicValue> = rows
                .into_iter()
                .map(|row| {
                    __ox_map_from(
                        row.into_iter()
                            .map(|(k, v)| {
                                (
                                    DynamicValue::Str(k),
                                    match v {
                                        Val::Null => DynamicValue::None,
                                        Val::Int(n) => DynamicValue::Int(n),
                                        Val::Float(f) => DynamicValue::Float(f),
                                        Val::Text(t) => DynamicValue::Str(t),
                                    },
                                )
                            })
                            .collect(),
                    )
                })
                .collect();
            __ox_result_ok(DynamicValue::List(list.into()))
        }
        Err(e) => __ox_result_err(format!("db_query: {}", e)),
    }
}

pub fn __ox_db_last_insert_id(db: DynamicValue) -> DynamicValue {
    match __ox_db_handle(&db, "db_last_insert_id").and_then(__ox_sqlite::last_insert_id) {
        Ok(n) => __ox_result_ok(DynamicValue::Int(n)),
        Err(e) => __ox_result_err(format!("db_last_insert_id: {}", e)),
    }
}

// ---------------------------------------------------------- binary I/O
// read_bytes(path) -> Result<List<Int>> reads a file and returns its bytes as a list of integers.
pub fn __ox_read_bytes(path: DynamicValue) -> DynamicValue {
    let p = __ox_str_arg(&path, "read_bytes");
    match std::fs::read(&p) {
        Ok(data) => {
            let bytes: Vec<DynamicValue> = data.iter().map(|&b| DynamicValue::Int(b as i64)).collect();
            __ox_result_ok(DynamicValue::List(bytes.into()))
        }
        Err(e) => __ox_result_err(format!("read_bytes: cannot read '{}': {}", p, e)),
    }
}

// write_bytes(path, bytes) -> Result<None> atomically writes bytes to a file.
pub fn __ox_write_bytes(path: DynamicValue, bytes: DynamicValue) -> DynamicValue {
    let p = __ox_str_arg(&path, "write_bytes");
    let byte_list = match bytes {
        DynamicValue::List(items) => {
            let vec_ref = &items.0;
            let mut out = Vec::new();
            for item in vec_ref.iter() {
                match item {
                    DynamicValue::Int(n) if (0..=255).contains(n) => out.push(*n as u8),
                    _ => return __ox_result_err("write_bytes: list must contain integers in 0..=255".to_string()),
                }
            }
            out
        }
        _ => return __ox_result_err("write_bytes: bytes must be a List".to_string()),
    };
    
    match __ox_write_atomic(&p, &byte_list) {
        Ok(()) => __ox_result_ok(DynamicValue::None),
        Err(e) => __ox_result_err(format!("write_bytes: cannot write '{}': {}", p, e)),
    }
}

// hex_encode(bytes) -> String encodes a list of bytes as a hex string.
pub fn __ox_hex_encode(bytes: DynamicValue) -> DynamicValue {
    match bytes {
        DynamicValue::List(items) => {
            let vec_ref = &items.0;
            let mut hex = String::new();
            for item in vec_ref.iter() {
                match item {
                    DynamicValue::Int(n) if (0..=255).contains(n) => {
                        hex.push_str(&format!("{:02x}", n));
                    }
                    _ => return DynamicValue::Str("hex_encode: list must contain integers in 0..=255".to_string()),
                }
            }
            DynamicValue::Str(hex)
        }
        _ => DynamicValue::Str("hex_encode: input must be a List".to_string()),
    }
}

// hex_decode(hex) -> Result<List<Int>> decodes a hex string to a list of bytes.
pub fn __ox_hex_decode(hex: DynamicValue) -> DynamicValue {
    let h = __ox_str_arg(&hex, "hex_decode");
    if h.len() % 2 != 0 {
        return __ox_result_err("hex_decode: input length must be even".to_string());
    }
    let mut bytes = Vec::new();
    for i in (0..h.len()).step_by(2) {
        let hex_pair = &h[i..i+2];
        match u8::from_str_radix(hex_pair, 16) {
            Ok(b) => bytes.push(DynamicValue::Int(b as i64)),
            Err(_) => return __ox_result_err(format!("hex_decode: invalid hex at position {}", i)),
        }
    }
    __ox_result_ok(DynamicValue::List(bytes.into()))
}

// base64_encode(bytes) -> String encodes a list of bytes as base64.
pub fn __ox_base64_encode(bytes: DynamicValue) -> DynamicValue {
    match bytes {
        DynamicValue::List(items) => {
            let vec_ref = &items.0;
            let mut byte_vec = Vec::new();
            for item in vec_ref.iter() {
                match item {
                    DynamicValue::Int(n) if (0..=255).contains(n) => byte_vec.push(*n as u8),
                    _ => return DynamicValue::Str("base64_encode: list must contain integers in 0..=255".to_string()),
                }
            }
            const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut result = String::new();
            for chunk in byte_vec.chunks(3) {
                let b0 = chunk[0];
                let b1 = chunk.get(1).copied().unwrap_or(0);
                let b2 = chunk.get(2).copied().unwrap_or(0);
                let c0 = (b0 >> 2) as usize;
                let c1 = (((b0 & 0x03) << 4) | (b1 >> 4)) as usize;
                let c2 = (((b1 & 0x0f) << 2) | (b2 >> 6)) as usize;
                let c3 = (b2 & 0x3f) as usize;
                result.push(ALPHABET[c0] as char);
                result.push(ALPHABET[c1] as char);
                result.push(if chunk.len() > 1 { ALPHABET[c2] as char } else { '=' });
                result.push(if chunk.len() > 2 { ALPHABET[c3] as char } else { '=' });
            }
            DynamicValue::Str(result)
        }
        _ => DynamicValue::Str("base64_encode: input must be a List".to_string()),
    }
}

// base64_decode(s) -> Result<List<Int>> decodes a base64 string to a list of bytes.
pub fn __ox_base64_decode(s: DynamicValue) -> DynamicValue {
    let s_str = __ox_str_arg(&s, "base64_decode");
    if s_str.len() % 4 != 0 {
        return __ox_result_err("base64_decode: input length must be multiple of 4".to_string());
    }
    let mut result = Vec::new();
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in s_str.as_bytes().chunks(4) {
        let decode_char = |c: u8| -> Result<u8, &'static str> {
            match c {
                b'=' => Ok(0),
                _ => {
                    if let Some(pos) = alphabet.find(c as char) {
                        Ok(pos as u8)
                    } else {
                        Err("invalid base64 character")
                    }
                }
            }
        };
        let c0 = match decode_char(chunk[0]) {
            Ok(v) => v,
            Err(_) => return __ox_result_err("base64_decode: invalid character".to_string()),
        };
        let c1 = match decode_char(chunk[1]) {
            Ok(v) => v,
            Err(_) => return __ox_result_err("base64_decode: invalid character".to_string()),
        };
        let c2 = match decode_char(chunk[2]) {
            Ok(v) => v,
            Err(_) => return __ox_result_err("base64_decode: invalid character".to_string()),
        };
        let c3 = match decode_char(chunk[3]) {
            Ok(v) => v,
            Err(_) => return __ox_result_err("base64_decode: invalid character".to_string()),
        };
        let b0 = (c0 << 2) | (c1 >> 4);
        result.push(DynamicValue::Int(b0 as i64));
        if chunk[2] != b'=' {
            let b1 = ((c1 & 0x0f) << 4) | (c2 >> 2);
            result.push(DynamicValue::Int(b1 as i64));
        }
        if chunk[3] != b'=' {
            let b2 = ((c2 & 0x03) << 6) | c3;
            result.push(DynamicValue::Int(b2 as i64));
        }
    }
    __ox_result_ok(DynamicValue::List(result.into()))
}

pub fn __ox_sort_by_key(xs: DynamicValue, f: DynamicValue) -> DynamicValue {
    let lst = match xs {
        DynamicValue::List(l) => (*l).clone(),
        other => panic!("type error: sort_by_key() expects a List, got {:?}", other),
    };
    
    if lst.is_empty() {
        return DynamicValue::List(OxList::from(lst));
    }
    
    let f = match f {
        DynamicValue::Function(func) => func,
        other => panic!("type error: sort_by_key() expects a function, got {:?}", other),
    };
    
    // Build (key, original_value, original_index) tuples
    let mut indexed: Vec<(DynamicValue, DynamicValue, usize)> = Vec::new();
    for (i, item) in lst.iter().enumerate() {
        let key = f.0(vec![item.clone()]);
        indexed.push((key, item.clone(), i));
    }
    
    // Check that all keys are the same sortable type
    if !indexed.is_empty() {
        let first_key_kind = match &indexed[0].0 {
            DynamicValue::Int(_) => "int_or_float",
            DynamicValue::Float(_) => "int_or_float",
            DynamicValue::Str(_) => "string",
            DynamicValue::Bool(_) => "bool",
            other => panic!("type error: sort_by_key() keys must be Int, Float, String or Bool, got {:?}", other),
        };
        
        for (key, _, _) in &indexed {
            match key {
                DynamicValue::Int(_) | DynamicValue::Float(_) if first_key_kind == "int_or_float" => {},
                DynamicValue::Str(_) if first_key_kind == "string" => {},
                DynamicValue::Bool(_) if first_key_kind == "bool" => {},
                other => panic!("type error: sort_by_key() found mixed key types: {:?}", other),
            }
        }
    }
    
    // Sort by key (stable sort is the default)
    match &indexed[0].0 {
        DynamicValue::Int(_) | DynamicValue::Float(_) => {
            indexed.sort_by(|a, b| {
                let a_f = match &a.0 {
                    DynamicValue::Int(n) => *n as f64,
                    DynamicValue::Float(f) => *f,
                    _ => unreachable!(),
                };
                let b_f = match &b.0 {
                    DynamicValue::Int(n) => *n as f64,
                    DynamicValue::Float(f) => *f,
                    _ => unreachable!(),
                };
                let cmp = a_f.partial_cmp(&b_f).unwrap_or(std::cmp::Ordering::Equal);
                // Maintain stability by comparing original indices on equality
                if cmp == std::cmp::Ordering::Equal {
                    a.2.cmp(&b.2)
                } else {
                    cmp
                }
            });
        },
        DynamicValue::Str(_) => {
            indexed.sort_by(|a, b| {
                let a_str = match &a.0 {
                    DynamicValue::Str(s) => s,
                    _ => unreachable!(),
                };
                let b_str = match &b.0 {
                    DynamicValue::Str(s) => s,
                    _ => unreachable!(),
                };
                let cmp = a_str.cmp(b_str);
                if cmp == std::cmp::Ordering::Equal {
                    a.2.cmp(&b.2)
                } else {
                    cmp
                }
            });
        },
        DynamicValue::Bool(_) => {
            indexed.sort_by(|a, b| {
                let a_bool = match &a.0 {
                    DynamicValue::Bool(b) => *b,
                    _ => unreachable!(),
                };
                let b_bool = match &b.0 {
                    DynamicValue::Bool(b) => *b,
                    _ => unreachable!(),
                };
                let cmp = a_bool.cmp(&b_bool);
                if cmp == std::cmp::Ordering::Equal {
                    a.2.cmp(&b.2)
                } else {
                    cmp
                }
            });
        },
        _ => unreachable!(),
    }
    
    // Extract just the values in sorted order
    let sorted: Vec<DynamicValue> = indexed.into_iter().map(|(_, v, _)| v).collect();
    DynamicValue::List(OxList::from(sorted))
}

fn __ox_days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn __ox_civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn __ox_days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 },
    }
}

// (year, month, day, hour, minute, second, ms, weekday 0=Sunday, yearday 1-based)
fn __ox_time_split(ms: i64) -> (i64, i64, i64, i64, i64, i64, i64, i64, i64) {
    let days = ms.div_euclid(86_400_000);
    let mid = ms.rem_euclid(86_400_000);
    let (y, mo, d) = __ox_civil_from_days(days);
    let weekday = (days + 4).rem_euclid(7);
    let yearday = days - __ox_days_from_civil(y, 1, 1) + 1;
    (y, mo, d, mid / 3_600_000, (mid / 60_000) % 60, (mid / 1000) % 60, mid % 1000, weekday, yearday)
}

pub fn __ox_time_parts(ms: DynamicValue) -> DynamicValue {
    let total = match ms {
        DynamicValue::Int(n) => n,
        other => panic!("type error: time_parts() expects an Int (milliseconds since the Unix epoch), got {:?}", other),
    };
    if total.abs() > 8_000_000_000_000_000 {
        panic!("time_parts(): {} ms is out of the supported range", total);
    }
    let (y, mo, d, h, mi, s, msec, wd, yd) = __ox_time_split(total);
    __ox_map_from(vec![
        (DynamicValue::Str("year".to_string()), DynamicValue::Int(y)),
        (DynamicValue::Str("month".to_string()), DynamicValue::Int(mo)),
        (DynamicValue::Str("day".to_string()), DynamicValue::Int(d)),
        (DynamicValue::Str("hour".to_string()), DynamicValue::Int(h)),
        (DynamicValue::Str("minute".to_string()), DynamicValue::Int(mi)),
        (DynamicValue::Str("second".to_string()), DynamicValue::Int(s)),
        (DynamicValue::Str("ms".to_string()), DynamicValue::Int(msec)),
        (DynamicValue::Str("weekday".to_string()), DynamicValue::Int(wd)),
        (DynamicValue::Str("yearday".to_string()), DynamicValue::Int(yd)),
    ])
}

pub fn __ox_time_format(ms: DynamicValue, fmt: DynamicValue) -> DynamicValue {
    const WD: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const MN: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    let total = match ms {
        DynamicValue::Int(n) => n,
        other => panic!("type error: time_format() expects an Int (milliseconds since the Unix epoch), got {:?}", other),
    };
    let f = match fmt {
        DynamicValue::Str(s) => s,
        other => panic!("type error: time_format() expects a String format, got {:?}", other),
    };
    if total.abs() > 8_000_000_000_000_000 {
        panic!("time_format(): {} ms is out of the supported range", total);
    }
    let (y, mo, d, h, mi, s, msec, wd, yd) = __ox_time_split(total);
    let cs: Vec<char> = f.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] != '%' || i + 1 >= cs.len() {
            out.push(cs[i]);
            i += 1;
            continue;
        }
        let c = cs[i + 1];
        if c == '3' && i + 2 < cs.len() && cs[i + 2] == 'f' {
            out.push_str(&format!("{:03}", msec));
            i += 3;
            continue;
        }
        match c {
            'Y' => out.push_str(&format!("{:04}", y)),
            'y' => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            'm' => out.push_str(&format!("{:02}", mo)),
            'd' => out.push_str(&format!("{:02}", d)),
            'e' => out.push_str(&format!("{:2}", d)),
            'H' => out.push_str(&format!("{:02}", h)),
            'M' => out.push_str(&format!("{:02}", mi)),
            'S' => out.push_str(&format!("{:02}", s)),
            'j' => out.push_str(&format!("{:03}", yd)),
            'a' => out.push_str(&WD[wd as usize][..3]),
            'A' => out.push_str(WD[wd as usize]),
            'b' => out.push_str(&MN[(mo - 1) as usize][..3]),
            'B' => out.push_str(MN[(mo - 1) as usize]),
            '%' => out.push('%'),
            other => {
                out.push('%');
                out.push(other);
            }
        }
        i += 2;
    }
    DynamicValue::Str(out)
}

pub fn __ox_time_parse_iso(s: DynamicValue) -> DynamicValue {
    let text = match s {
        DynamicValue::Str(t) => t,
        other => panic!("type error: time_parse_iso() expects a String, got {:?}", other),
    };
    let b = text.as_bytes();
    let err = |why: &str| __ox_result_err(format!("time_parse_iso: {} in {:?}", why, text));
    let digits = |from: usize, n: usize| -> Option<i64> {
        if from + n > b.len() {
            return None;
        }
        let mut v: i64 = 0;
        for k in from..from + n {
            if !b[k].is_ascii_digit() {
                return None;
            }
            v = v * 10 + (b[k] - b'0') as i64;
        }
        Some(v)
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return err("expected YYYY-MM-DD");
    }
    let (y, mo, d) = match (digits(0, 4), digits(5, 2), digits(8, 2)) {
        (Some(y), Some(mo), Some(d)) => (y, mo, d),
        _ => return err("non-digit in the date"),
    };
    if !(1..=12).contains(&mo) {
        return err("month out of range");
    }
    if d < 1 || d > __ox_days_in_month(y, mo) {
        return err("day out of range");
    }
    let (mut h, mut mi, mut sec, mut msec) = (0i64, 0i64, 0i64, 0i64);
    if b.len() > 10 {
        if b[10] != b'T' || b.len() < 20 || b[13] != b':' || b[16] != b':' {
            return err("expected THH:MM:SS");
        }
        match (digits(11, 2), digits(14, 2), digits(17, 2)) {
            (Some(a), Some(c), Some(e)) => { h = a; mi = c; sec = e; }
            _ => return err("non-digit in the time"),
        }
        if h > 23 || mi > 59 || sec > 59 {
            return err("time out of range");
        }
        let mut p = 19;
        if p < b.len() && b[p] == b'.' {
            p += 1;
            let start = p;
            while p < b.len() && b[p].is_ascii_digit() {
                p += 1;
            }
            if p == start {
                return err("empty fraction");
            }
            let mut frac = 0i64;
            for k in 0..3 {
                frac = frac * 10 + if start + k < p { (b[start + k] - b'0') as i64 } else { 0 };
            }
            msec = frac;
        }
        if p + 1 != b.len() || b[p] != b'Z' {
            return err("expected a trailing Z");
        }
    }
    let days = __ox_days_from_civil(y, mo, d);
    __ox_result_ok(DynamicValue::Int(days * 86_400_000 + h * 3_600_000 + mi * 60_000 + sec * 1000 + msec))
}

// ================================================================ shared utilities
fn __ox_hex_encode_internal(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()
}

fn __ox_hex_decode_internal(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut result = Vec::new();
    for i in (0..s.len()).step_by(2) {
        let byte = u8::from_str_radix(&s[i..i + 2], 16).ok()?;
        result.push(byte);
    }
    Some(result)
}

fn __ox_const_time_cmp(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ================================================================ pure Rust SHA256 for PBKDF2 and random
fn __ox_sha256_bytes_pure(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[4 * i..4 * i + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

// HMAC-SHA256 helper: compute HMAC(key, message)
fn __ox_hmac_sha256_internal(key: &[u8], message: &[u8]) -> [u8; 32] {
    // If key is longer than 64 bytes, hash it first
    let mut k = key.to_vec();
    if k.len() > 64 {
        k = __ox_sha256_bytes_pure(&k).to_vec();
    }
    // Pad key to 64 bytes
    k.resize(64, 0);
    
    // XOR key with ipad (0x36) and opad (0x5c)
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    
    // Inner: SHA256(ipad || message)
    let mut inner_msg = ipad.to_vec();
    inner_msg.extend_from_slice(message);
    let inner_hash = __ox_sha256_bytes_pure(&inner_msg);
    
    // Outer: SHA256(opad || inner_hash)
    let mut outer_msg = opad.to_vec();
    outer_msg.extend_from_slice(&inner_hash);
    __ox_sha256_bytes_pure(&outer_msg)
}

// PBKDF2-HMAC-SHA256 in pure Rust (for Unix or as Windows fallback)
fn __ox_pbkdf2_hmac_sha256_pure(password: &[u8], salt: &[u8], iterations: u64, bytes: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes as usize);
    let mut i = 1u32;
    
    while (out.len() as u32) < bytes {
        // U_1 = HMAC-SHA256(password, salt || i)
        let mut salt_with_counter = salt.to_vec();
        salt_with_counter.extend_from_slice(&i.to_be_bytes());
        let mut u = __ox_hmac_sha256_internal(password, &salt_with_counter).to_vec();
        let mut t = u.clone();
        
        // U_j = HMAC-SHA256(password, U_(j-1)) for j = 2 to iterations
        for _ in 1..iterations {
            u = __ox_hmac_sha256_internal(password, &u).to_vec();
            
            // XOR into accumulator
            for j in 0..u.len() {
                t[j] ^= u[j];
            }
        }
        
        out.extend_from_slice(&t);
        i = i.wrapping_add(1);
    }
    
    out.truncate(bytes as usize);
    out
}

// ================================================================ Windows bcrypt (when available)
#[cfg(windows)]
mod __ox_bcrypt {
    use std::ffi::{c_char, c_uint, c_void, CString};
    use std::sync::OnceLock;

    extern "system" {
        fn LoadLibraryA(name: *const c_char) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    pub type NTSTATUS = i32;
    pub type BcryptAlgHandle = *mut c_void;

    type BCryptOpenAlgorithmProviderFn = unsafe extern "system" fn(
        *mut BcryptAlgHandle,
        *const u16,
        *const u16,
        c_uint,
    ) -> NTSTATUS;

    type BCryptCloseAlgorithmProviderFn = unsafe extern "system" fn(BcryptAlgHandle, c_uint) -> NTSTATUS;

    type BCryptDeriveKeyPBKDF2Fn = unsafe extern "system" fn(
        BcryptAlgHandle,
        *const u8,
        c_uint,
        *const u8,
        c_uint,
        u64,
        *mut u8,
        c_uint,
        c_uint,
    ) -> NTSTATUS;

    type BCryptGenRandomFn = unsafe extern "system" fn(
        BcryptAlgHandle,
        *mut u8,
        c_uint,
        c_uint,
    ) -> NTSTATUS;

    pub struct BcryptApi {
        #[allow(dead_code)]
        pub lib: *mut c_void,
        pub open: BCryptOpenAlgorithmProviderFn,
        pub close: BCryptCloseAlgorithmProviderFn,
        pub derive: BCryptDeriveKeyPBKDF2Fn,
        pub gen_random: BCryptGenRandomFn,
    }

    unsafe impl Send for BcryptApi {}
    unsafe impl Sync for BcryptApi {}

    unsafe fn load_api(dll_path: &str) -> Result<BcryptApi, String> {
        let c = CString::new(dll_path).map_err(|_| "invalid DLL path".to_string())?;
        let lib = LoadLibraryA(c.as_ptr());
        if lib.is_null() {
            return Err("failed to load bcrypt.dll".to_string());
        }

        let open_bytes = b"BCryptOpenAlgorithmProvider\0";
        let close_bytes = b"BCryptCloseAlgorithmProvider\0";
        let derive_bytes = b"BCryptDeriveKeyPBKDF2\0";
        let gen_random_bytes = b"BCryptGenRandom\0";

        let open_ptr = GetProcAddress(lib, open_bytes.as_ptr() as *const c_char);
        let close_ptr = GetProcAddress(lib, close_bytes.as_ptr() as *const c_char);
        let derive_ptr = GetProcAddress(lib, derive_bytes.as_ptr() as *const c_char);
        let gen_random_ptr = GetProcAddress(lib, gen_random_bytes.as_ptr() as *const c_char);

        if open_ptr.is_null() || close_ptr.is_null() || derive_ptr.is_null() || gen_random_ptr.is_null() {
            return Err("failed to load bcrypt functions".to_string());
        }

        Ok(BcryptApi {
            lib,
            open: std::mem::transmute(open_ptr),
            close: std::mem::transmute(close_ptr),
            derive: std::mem::transmute(derive_ptr),
            gen_random: std::mem::transmute(gen_random_ptr),
        })
    }

    static BCRYPT: OnceLock<Result<BcryptApi, String>> = OnceLock::new();

    pub fn get() -> Result<&'static BcryptApi, String> {
        BCRYPT
            .get_or_init(|| unsafe {
                let dll_path = std::env::var("OXIDIZED_BCRYPT_DLL").unwrap_or_else(|_| "bcrypt.dll".to_string());
                load_api(&dll_path)
            })
            .as_ref()
            .map_err(|e| e.clone())
    }

    pub fn utf16_nul(s: &str) -> Vec<u16> {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    }
}

// ================================================================ Unix random bytes from /dev/urandom
#[cfg(unix)]
fn __ox_os_random_bytes(n: usize) -> Option<Vec<u8>> {
    if n == 0 {
        return Some(Vec::new());
    }
    // Read exactly n bytes from /dev/urandom using File::open + read_exact
    use std::io::Read;
    let mut file = match std::fs::File::open("/dev/urandom") {
        Ok(f) => f,
        Err(_) => return None,
    };
    let mut buf = vec![0u8; n];
    match file.read_exact(&mut buf) {
        Ok(_) => Some(buf),
        Err(_) => None,
    }
}

#[cfg(not(any(windows, unix)))]
fn __ox_os_random_bytes(_n: usize) -> Option<Vec<u8>> {
    None // no OS randomness available (e.g. wasm)
}

#[cfg(windows)]
fn __ox_os_random_bytes(n: usize) -> Option<Vec<u8>> {
    if n == 0 {
        return Some(Vec::new());
    }
    
    // Try bcrypt first
    if let Some(api) = __ox_bcrypt::get().ok() {
        let mut buf = vec![0u8; n];
        let status = unsafe { (api.gen_random)(std::ptr::null_mut(), buf.as_mut_ptr(), n as u32, 0x2) };
        if status == 0 {
            return Some(buf);
        }
    }
    
    // Fallback to SystemFunction036 (RtlGenRandom) from advapi32
    unsafe {
        use std::ffi::{c_char, c_void};
        
        extern "system" {
            fn LoadLibraryA(name: *const c_char) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        }
        
        let advapi32_name = b"advapi32.dll\0";
        let lib = LoadLibraryA(advapi32_name.as_ptr() as *const c_char);
        if lib.is_null() {
            return None;
        }
        
        let fn_name = b"SystemFunction036\0";
        let sys_fn = GetProcAddress(lib, fn_name.as_ptr() as *const c_char);
        if sys_fn.is_null() {
            return None;
        }
        
        let mut buf = vec![0u8; n];
        let system_fn: extern "system" fn(*mut u8, u32) -> u32 = std::mem::transmute(sys_fn);
        let status = system_fn(buf.as_mut_ptr(), n as u32);
        
        if status != 0 {
            Some(buf)
        } else {
            None
        }
    }
}

// ================================================================ pbkdf2_sha256
pub fn __ox_pbkdf2_sha256(
    password: DynamicValue,
    salt: DynamicValue,
    iterations: DynamicValue,
    bytes: DynamicValue,
) -> DynamicValue {
    let password = __ox_str_arg(&password, "pbkdf2_sha256");
    let salt = __ox_str_arg(&salt, "pbkdf2_sha256");

    let iterations = match iterations {
        DynamicValue::Int(i) if (1..=10_000_000).contains(&i) => i as u64,
        _ => return __ox_result_err("pbkdf2_sha256: iterations must be in 1..=10_000_000".to_string()),
    };

    let bytes = match bytes {
        DynamicValue::Int(b) if (1..=1024).contains(&b) => b as u32,
        _ => return __ox_result_err("pbkdf2_sha256: bytes must be in 1..=1024".to_string()),
    };

    // On Windows, try bcrypt.dll first (faster); fall back to pure Rust.
    #[cfg(windows)]
    {
        if let Ok(api) = __ox_bcrypt::get() {
            unsafe {
                let mut alg_handle: __ox_bcrypt::BcryptAlgHandle = std::ptr::null_mut();
                let alg_id = __ox_bcrypt::utf16_nul("SHA256");

                let status = (api.open)(
                    &mut alg_handle,
                    alg_id.as_ptr(),
                    std::ptr::null(),
                    0x8,
                );

                if status == 0 {
                    let mut derived_key = vec![0u8; bytes as usize];
                    let password_bytes = password.as_bytes();
                    let salt_bytes = salt.as_bytes();

                    let status = (api.derive)(
                        alg_handle,
                        password_bytes.as_ptr() as *mut u8,
                        password_bytes.len() as u32,
                        salt_bytes.as_ptr() as *mut u8,
                        salt_bytes.len() as u32,
                        iterations,
                        derived_key.as_mut_ptr(),
                        bytes,
                        0,
                    );

                    let _ = (api.close)(alg_handle, 0);

                    if status == 0 {
                        return __ox_result_ok(DynamicValue::Str(__ox_hex_encode_internal(&derived_key)));
                    }
                }
            }
        }
    }

    // Pure Rust fallback (Unix or Windows DLL missing)
    let derived = __ox_pbkdf2_hmac_sha256_pure(password.as_bytes(), salt.as_bytes(), iterations, bytes);
    __ox_result_ok(DynamicValue::Str(__ox_hex_encode_internal(&derived)))
}

// ================================================================ random_bytes_hex
pub fn __ox_random_bytes_hex(n: DynamicValue) -> DynamicValue {
    let n = match n {
        DynamicValue::Int(i) if (0..=4096).contains(&i) => i as usize,
        _ => return __ox_result_err("random_bytes_hex: n must be in 0..=4096".to_string()),
    };

    if let Some(buf) = __ox_os_random_bytes(n) {
        __ox_result_ok(DynamicValue::Str(__ox_hex_encode_internal(&buf)))
    } else {
        __ox_result_err("random_bytes_hex: OS random generation failed".to_string())
    }
}

// ================================================================ password_hash
pub fn __ox_password_hash(password: DynamicValue) -> DynamicValue {
    let password = __ox_str_arg(&password, "password_hash");

    let salt_bytes = match __ox_os_random_bytes(16) {
        Some(b) => b,
        None => panic!("password_hash: operating system random bytes failed; refusing to return a fake hash"),
    };

    let salt_hex = __ox_hex_encode_internal(&salt_bytes);
    let iterations = 120000u64;
    let bytes = 32u32;

    // Try bcrypt.dll on Windows, fall back to pure Rust
    #[cfg(windows)]
    {
        if let Ok(api) = __ox_bcrypt::get() {
            unsafe {
                let mut alg_handle: __ox_bcrypt::BcryptAlgHandle = std::ptr::null_mut();
                let alg_id = __ox_bcrypt::utf16_nul("SHA256");

                let status = (api.open)(
                    &mut alg_handle,
                    alg_id.as_ptr(),
                    std::ptr::null(),
                    0x8,
                );

                if status == 0 {
                    let mut derived_key = vec![0u8; bytes as usize];
                    let password_bytes = password.as_bytes();

                    let status = (api.derive)(
                        alg_handle,
                        password_bytes.as_ptr() as *mut u8,
                        password_bytes.len() as u32,
                        salt_bytes.as_ptr() as *mut u8,
                        salt_bytes.len() as u32,
                        iterations,
                        derived_key.as_mut_ptr(),
                        bytes,
                        0,
                    );

                    let _ = (api.close)(alg_handle, 0);

                    if status == 0 {
                        let hash_hex = __ox_hex_encode_internal(&derived_key);
                        return DynamicValue::Str(format!(
                            "pbkdf2-sha256${}${}${}",
                            iterations, salt_hex, hash_hex
                        ));
                    }
                }
            }
        }
    }

    // Pure Rust fallback
    let derived = __ox_pbkdf2_hmac_sha256_pure(password.as_bytes(), &salt_bytes, iterations, bytes);
    let hash_hex = __ox_hex_encode_internal(&derived);
    DynamicValue::Str(format!(
        "pbkdf2-sha256${}${}${}",
        iterations, salt_hex, hash_hex
    ))
}

// ================================================================ password_verify
pub fn __ox_password_verify(password: DynamicValue, stored: DynamicValue) -> DynamicValue {
    let password = __ox_str_arg(&password, "password_verify");
    let stored = __ox_str_arg(&stored, "password_verify");

    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
        return DynamicValue::Bool(false);
    }

    let iterations = match parts[1].parse::<u64>() {
        Ok(i) if i > 0 && i <= 10_000_000 => i,
        _ => return DynamicValue::Bool(false),
    };

    let salt_hex = parts[2];
    let stored_hash_hex = parts[3];

    let salt_bytes = match __ox_hex_decode_internal(salt_hex) {
        Some(b) => b,
        None => return DynamicValue::Bool(false),
    };

    let stored_hash = match __ox_hex_decode_internal(stored_hash_hex) {
        Some(h) => h,
        None => return DynamicValue::Bool(false),
    };

    let bytes = stored_hash.len() as u32;
    if bytes == 0 || bytes > 1024 {
        return DynamicValue::Bool(false);
    }

    // Try bcrypt.dll on Windows
    #[cfg(windows)]
    {
        if let Ok(api) = __ox_bcrypt::get() {
            unsafe {
                let mut alg_handle: __ox_bcrypt::BcryptAlgHandle = std::ptr::null_mut();
                let alg_id = __ox_bcrypt::utf16_nul("SHA256");

                let status = (api.open)(
                    &mut alg_handle,
                    alg_id.as_ptr(),
                    std::ptr::null(),
                    0x8,
                );

                if status == 0 {
                    let mut derived_key = vec![0u8; bytes as usize];
                    let password_bytes = password.as_bytes();

                    let status = (api.derive)(
                        alg_handle,
                        password_bytes.as_ptr() as *mut u8,
                        password_bytes.len() as u32,
                        salt_bytes.as_ptr() as *mut u8,
                        salt_bytes.len() as u32,
                        iterations,
                        derived_key.as_mut_ptr(),
                        bytes,
                        0,
                    );

                    let _ = (api.close)(alg_handle, 0);

                    if status == 0 {
                        return DynamicValue::Bool(__ox_const_time_cmp(&derived_key, &stored_hash));
                    }
                }
            }
        }
    }

    // Pure Rust fallback
    let derived = __ox_pbkdf2_hmac_sha256_pure(password.as_bytes(), &salt_bytes, iterations, bytes);
    DynamicValue::Bool(__ox_const_time_cmp(&derived, &stored_hash))
}

// OS random bytes for random_hex(); None when unavailable so the caller can fall back.
pub fn __ox_os_random(n: usize) -> Option<Vec<u8>> {
    __ox_os_random_bytes(n)
}

type __OxSharedTable = std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<DynamicValue>>>>;

fn __ox_shared_cell(name: &str) -> std::sync::Arc<std::sync::Mutex<DynamicValue>> {
    static TABLE: std::sync::OnceLock<__OxSharedTable> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut t = table.lock().unwrap_or_else(|e| e.into_inner());
    std::sync::Arc::clone(
        t.entry(name.to_string())
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(DynamicValue::None))),
    )
}

pub fn __ox_shared_get(name: DynamicValue) -> DynamicValue {
    let key = __ox_str_arg(&name, "shared_get");
    let cell = __ox_shared_cell(&key);
    let guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    guard.clone()
}

pub fn __ox_shared_set(name: DynamicValue, value: DynamicValue) -> DynamicValue {
    let key = __ox_str_arg(&name, "shared_set");
    let cell = __ox_shared_cell(&key);
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    *guard = value;
    DynamicValue::None
}

pub fn __ox_shared_update(name: DynamicValue, f: DynamicValue) -> DynamicValue {
    let key = __ox_str_arg(&name, "shared_update");
    let func = match f {
        DynamicValue::Function(func) => func,
        other => panic!("type error: shared_update() expects a function, got {:?}", other),
    };
    let cell = __ox_shared_cell(&key);
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    let next = func.call(vec![guard.clone()]);
    *guard = next.clone();
    next
}


#[allow(unused_imports)]
use crate::__oxidized_natives::*;

#[allow(unused_variables, unreachable_code)]
fn __ox_dispatch_op(method: &str, a: DynamicValue, b: Option<DynamicValue>) -> Option<DynamicValue> {
    let name = match &a { DynamicValue::Struct(n, _) => n.clone(), _ => return None };
    match (name.as_str(), method) {
        _ => None,
    }
}

#[allow(unused_variables, unreachable_code, unused_mut, dead_code)]
fn __ox_call_method(name: &str, recv: &mut DynamicValue, args: Vec<DynamicValue>) -> DynamicValue {
    let sname = match &*recv { DynamicValue::Struct(n, _) => n.clone(), other => panic!("type error: cannot call method `{}` on {:?}", name, other) };
    match (sname.as_str(), name) {
        (s, m) => panic!("struct `{}` has no method `{}`", s, m),
    }
}

#[allow(dead_code)]
fn __ox_variant_index(enum_name: &str, variant: &str) -> Option<usize> {
    match (enum_name, variant) {
        ("Result", "Ok") => Some(0),
        ("Result", "Err") => Some(1),
        ("Option", "Some") => Some(0),
        ("Option", "None") => Some(1),
        _ => None,
    }
}

#[allow(dead_code)]
fn __ox_is_float_field(struct_name: &str, field: &str) -> bool {
    match (struct_name, field) {
        ("Spawn", "b") => true,
        ("Spawn", "g") => true,
        ("Spawn", "r") => true,
        ("Spawn", "size") => true,
        ("Spawn", "x") => true,
        ("Spawn", "y") => true,
        ("Spawn", "z") => true,
        ("State", "spawn_timer") => true,
        ("State", "time") => true,
        _ => false,
    }
}

#[allow(non_snake_case, unused_variables, unused_mut, unused_assignments, unreachable_code, dead_code)]
fn update_game(state: DynamicValue, dt: f64, count: i64) -> DynamicValue {
    __ox_stack_check();
    let time = __ox_fld(&state, "time").clone() + DynamicValue::Float(dt as f64);
    let spawn_timer = __ox_fld(&state, "spawn_timer").clone() + DynamicValue::Float(dt as f64);
    let mut spawns = DynamicValue::list(vec![]);
    if (DynamicValue::Int(count as i64) < DynamicValue::Int(120)) && (spawn_timer.clone() > DynamicValue::Float(0.4f64)) {
        let angle = DynamicValue::Float(host_random() as f64) * DynamicValue::Float(6.2832f64);
        let rad = DynamicValue::Float(3f64) + (DynamicValue::Float(host_random() as f64) * DynamicValue::Float(5f64));
        let x = DynamicValue::Float(host_cos(angle.as_f64()) as f64) * rad.clone();
        let z = DynamicValue::Float(host_sin(angle.as_f64()) as f64) * rad.clone();
        let y = DynamicValue::Float(6f64) + (DynamicValue::Float(host_random() as f64) * DynamicValue::Float(4f64));
        let size = DynamicValue::Float(0.5f64) + (DynamicValue::Float(host_random() as f64) * DynamicValue::Float(0.8f64));
        let r: f64 = host_random();
        let g: f64 = host_random();
        let b: f64 = host_random();
        let s = DynamicValue::Struct("Spawn".to_string(), vec![("x".to_string(), (x).widen_int()), ("y".to_string(), (y).widen_int()), ("z".to_string(), (z).widen_int()), ("size".to_string(), (size).widen_int()), ("r".to_string(), (DynamicValue::Float(r as f64)).widen_int()), ("g".to_string(), (DynamicValue::Float(g as f64)).widen_int()), ("b".to_string(), (DynamicValue::Float(b as f64)).widen_int())]);
        spawns = push(spawns.clone(), s);
        } else {
        }
    let next = DynamicValue::Struct("State".to_string(), vec![("time".to_string(), (time).widen_int()), ("spawn_timer".to_string(), ({
let __match_scrutinee = DynamicValue::Bool(__ox_len(&spawns) > DynamicValue::Int(0));
if __match_scrutinee == DynamicValue::Bool(true) {
DynamicValue::Float(0f64)
} else if true {
spawn_timer.clone()
} else { panic!("no match arm matched the scrutinee: {:?}", __match_scrutinee) }
}).widen_int())]);
    return DynamicValue::tuple(vec![next, spawns.clone()]);
}
#[allow(non_snake_case, unused_variables, unused_mut, unused_assignments, unreachable_code, dead_code)]
fn describe_game(time: f64, count: i64) -> () {
    __ox_stack_check();
    print(DynamicValue::Str(format!("{} {} {} {}", (DynamicValue::Str("Physics Sandbox | objects: ".to_string())).to_string(), (DynamicValue::Int(count as i64)).to_string(), (DynamicValue::Str(" | time: ".to_string())).to_string(), (DynamicValue::Float(time as f64)).to_string())));
}


