use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};

use indexmap::IndexMap;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};

// FxHash-style hasher for short string context keys. ~3-5x faster than
// SipHash13 for trusted, non-adversarial keys. Not HashDoS resistant;
// used only for `BaseContext`/`RenderContext` internal storage.

const FX_SEED: usize = 0xcbf29ce484222325; // FNV offset basis.
const FX_ROTATE: u32 = 5;
const FX_MUL: usize = 0x517cc1b727220a95;

#[derive(Default, Clone, Copy)]
pub struct FastHasher {
    state: usize,
}

impl FastHasher {
    #[inline(always)]
    fn add_to_hash(&mut self, word: usize) {
        self.state = (self.state.rotate_left(FX_ROTATE) ^ word).wrapping_mul(FX_MUL);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        // Initialise so empty keys don't all hash to 0.
        if self.state == 0 {
            self.state = FX_SEED;
        }
        while bytes.len() >= 8 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[..8]);
            self.add_to_hash(usize::from_ne_bytes(buf));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(&bytes[..4]);
            self.add_to_hash(u32::from_ne_bytes(buf) as usize);
            bytes = &bytes[4..];
        }
        if bytes.len() >= 2 {
            let mut buf = [0u8; 2];
            buf.copy_from_slice(&bytes[..2]);
            self.add_to_hash(u16::from_ne_bytes(buf) as usize);
            bytes = &bytes[2..];
        }
        if let Some(&b) = bytes.first() {
            self.add_to_hash(b as usize);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        if self.state == 0 {
            self.state = FX_SEED;
        }
        self.add_to_hash(i as usize);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.state as u64
    }
}

/// FastHasher-backed HashMap for context dict storage.
type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;

/// Any value that can appear in a Django template context. Python types
/// not mappable to a Rust variant stay `Py<PyAny>`. `SafeString` uses
/// `Arc<str>` for O(1) clone in filter-constant hot paths.
#[derive(Clone, Debug)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    /// Marked safe; not auto-escaped. Arc<str> for O(1) clone.
    SafeString(std::sync::Arc<str>),
    List(Vec<Value>),
    Dict(IndexMap<String, Value>),
    /// Escape hatch: arbitrary Python object behind the GIL.
    PyObject(Py<PyAny>),
}

impl Value {
    #[inline]
    pub fn safe(s: impl AsRef<str>) -> Self {
        Value::SafeString(std::sync::Arc::from(s.as_ref()))
    }

    /// `Some(&str)` for String/SafeString, else `None`.
    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s.as_str()),
            Value::SafeString(s) => Some(s.as_ref()),
            _ => None,
        }
    }
}

/// `String` -> `Arc<str>` for `Value::SafeString`. One copy now;
/// subsequent clones are O(1).
#[inline]
pub fn safestring_from_string(s: String) -> std::sync::Arc<str> {
    std::sync::Arc::from(s)
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::None, Value::None) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            // String/SafeString compare by content (safe-ness is a
            // rendering concern, not semantic).
            (Value::String(a), Value::String(b)) => a == b,
            (Value::String(a), Value::SafeString(b)) => a.as_str() == b.as_ref(),
            (Value::SafeString(a), Value::String(b)) => a.as_ref() == b.as_str(),
            (Value::SafeString(a), Value::SafeString(b)) => a.as_ref() == b.as_ref(),
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Dict(a), Value::Dict(b)) => a == b,
            // PyObjects equal iff identical.
            (Value::PyObject(a), Value::PyObject(b)) => a.is(b),
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::None => write!(f, "None"),
            Value::Bool(true) => write!(f, "True"),
            Value::Bool(false) => write!(f, "False"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "{s}"),
            Value::SafeString(s) => f.write_str(s),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    // Django uses repr-style: strings quoted with single quotes.
                    match v {
                        Value::String(s) => write!(f, "'{s}'")?,
                        Value::SafeString(s) => write!(f, "'{}'", s.as_ref())?,
                        _ => write!(f, "{v}")?,
                    }
                }
                write!(f, "]")
            }
            Value::Dict(map) => {
                write!(f, "{{")?;
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "'{k}': {v}")?;
                }
                write!(f, "}}")
            }
            Value::PyObject(obj) => {
                // Call Python's str() to get the string representation.
                // This requires the GIL, which we acquire here.
                let s = Python::attach(|py| {
                    obj.bind(py)
                        .str()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "<PyObject>".to_owned())
                });
                write!(f, "{}", s)
            }
        }
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::String(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::String(v.to_owned())
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Self {
        Value::List(v.into_iter().map(Into::into).collect())
    }
}

impl From<HashMap<String, Value>> for Value {
    fn from(m: HashMap<String, Value>) -> Self {
        Value::Dict(m.into_iter().collect())
    }
}

impl From<IndexMap<String, Value>> for Value {
    fn from(m: IndexMap<String, Value>) -> Self {
        Value::Dict(m)
    }
}

impl<'py> From<&Bound<'py, PyAny>> for Value {
    fn from(obj: &Bound<'py, PyAny>) -> Self {
        // Hot path: dispatch on exact type before any attribute lookups
        // so common cases (str/int/bool) skip `getattr("__html__")` and
        // Promise import. `is_exact_instance_of` is a single pointer
        // compare. Subclasses fall through to the slow path.

        if obj.is_none() {
            return Value::None;
        }

        // PyBool first: PyBool is a subclass of PyInt.
        if obj.is_exact_instance_of::<PyBool>() {
            return Value::Bool(obj.extract::<bool>().unwrap_or(false));
        }
        if obj.is_exact_instance_of::<PyInt>() {
            if let Ok(v) = obj.extract::<i64>() {
                return Value::Int(v);
            }
        }
        if obj.is_exact_instance_of::<PyFloat>() {
            if let Ok(v) = obj.extract::<f64>() {
                return Value::Float(v);
            }
        }
        if obj.is_exact_instance_of::<PyString>() {
            // Exact `str` can't be SafeString (a subclass); skip __html__.
            if let Ok(v) = obj.extract::<String>() {
                return Value::String(v);
            }
        }
        if obj.is_exact_instance_of::<PyDict>()
            || obj.is_exact_instance_of::<PyList>()
            || obj.is_exact_instance_of::<PyTuple>()
        {
            // Lazy: defer to Python's lookup protocol.
            return Value::PyObject(obj.clone().unbind());
        }

        // Slow path: SafeString, gettext_lazy, model instances, etc.

        let is_safe = obj.getattr("__html__").is_ok();

        if let Ok(s) = obj.cast::<PyString>() {
            if let Ok(v) = s.extract::<String>() {
                if is_safe {
                    return Value::SafeString(std::sync::Arc::from(v));
                }
                return Value::String(v);
            }
        }

        if let Ok(b) = obj.cast::<PyBool>() {
            return Value::Bool(b.is_true());
        }
        if let Ok(i) = obj.cast::<PyInt>() {
            if let Ok(v) = i.extract::<i64>() {
                return Value::Int(v);
            }
        }
        if let Ok(f) = obj.cast::<PyFloat>() {
            if let Ok(v) = f.extract::<f64>() {
                return Value::Float(v);
            }
        }

        // Dict/list/tuple subclasses stay lazy.
        if obj.is_instance_of::<PyList>()
            || obj.is_instance_of::<PyTuple>()
            || obj.is_instance_of::<PyDict>()
        {
            return Value::PyObject(obj.clone().unbind());
        }

        // Django lazy strings (Promise): have __str__ but aren't PyString.
        let is_promise = obj
            .py()
            .import("django.utils.functional")
            .and_then(|m| m.getattr("Promise"))
            .and_then(|cls| obj.is_instance(&cls))
            .unwrap_or(false);

        if is_promise {
            if let Ok(s) = obj.str() {
                if let Ok(v) = s.extract::<String>() {
                    // Re-check __html__ on the resolved string (gettext_lazy
                    // may wrap SafeData transparently).
                    let resolved_safe = is_safe || s.as_any().getattr("__html__").is_ok();
                    if resolved_safe {
                        return Value::SafeString(std::sync::Arc::from(v));
                    }
                    return Value::String(v);
                }
            }
        }

        Value::PyObject(obj.clone().unbind())
    }
}

/// Convert a `Value` back into a Python object.
impl Value {
    pub fn to_pyobject(&self, py: Python<'_>) -> Py<PyAny> {
        match self {
            Value::None => py.None(),
            Value::Bool(b) => b.into_pyobject(py).unwrap().to_owned().into_any().unbind(),
            Value::Int(n) => n.into_pyobject(py).unwrap().into_any().unbind(),
            Value::Float(n) => n.into_pyobject(py).unwrap().into_any().unbind(),
            Value::String(s) => s.into_pyobject(py).unwrap().into_any().unbind(),
            Value::SafeString(s) => {
                // mark_safe so SafeData status survives the round-trip.
                let as_str: &str = s.as_ref();
                if let Ok(mark_safe) = py
                    .import("django.utils.safestring")
                    .and_then(|m| m.getattr("mark_safe"))
                {
                    if let Ok(result) = mark_safe.call1((as_str,)) {
                        return result.unbind();
                    }
                }
                as_str.into_pyobject(py).unwrap().into_any().unbind()
            }
            Value::List(items) => {
                let list = PyList::new(py, items.iter().map(|v| v.to_pyobject(py))).unwrap();
                list.into_any().unbind()
            }
            Value::Dict(map) => {
                let dict = PyDict::new(py);
                for (k, v) in map {
                    dict.set_item(k, v.to_pyobject(py)).unwrap();
                }
                dict.into_any().unbind()
            }
            Value::PyObject(obj) => obj.clone_ref(py),
        }
    }
}

/// API-boundary type with default RandomState hasher.
pub type ContextDict = HashMap<String, Value>;

/// Internal dict storage using FastHasher.
type InternalDict = FastMap<String, Value>;

/// External -> internal dict. Rare path (push/with/reset).
#[inline]
fn to_internal(d: ContextDict) -> InternalDict {
    let mut out: InternalDict = FastMap::with_capacity_and_hasher(d.len(), Default::default());
    out.extend(d);
    out
}

#[inline]
fn to_external(d: InternalDict) -> ContextDict {
    let mut out: ContextDict = HashMap::with_capacity(d.len());
    out.extend(d);
    out
}

/// Bottom-of-stack builtins. Matches Django's `{"True": True, ...}`.
fn builtins() -> InternalDict {
    let mut m = InternalDict::with_capacity_and_hasher(3, Default::default());
    m.insert("True".into(), Value::Bool(true));
    m.insert("False".into(), Value::Bool(false));
    m.insert("None".into(), Value::None);
    m
}

/// Top-down context stack. Mirrors `django.template.context.BaseContext`.
/// Use manual `push`/`pop` or the `scope(|ctx| ...)` closure form for
/// guaranteed cleanup (an RAII guard would clash with the borrow checker).
#[derive(Clone, Debug)]
pub struct BaseContext {
    pub dicts: Vec<InternalDict>,
}

impl BaseContext {
    pub fn new() -> Self {
        Self {
            dicts: vec![builtins()],
        }
    }

    pub fn with_values(values: ContextDict) -> Self {
        Self {
            dicts: vec![builtins(), to_internal(values)],
        }
    }

    /// Matches Django's `_reset_dicts`.
    pub fn reset_dicts(&mut self, values: Option<ContextDict>) {
        self.dicts.clear();
        self.dicts.push(builtins());
        if let Some(d) = values {
            self.dicts.push(to_internal(d));
        }
    }

    #[inline]
    pub fn push(&mut self) {
        self.dicts.push(InternalDict::default());
    }

    #[inline]
    pub fn push_with(&mut self, values: ContextDict) {
        self.dicts.push(to_internal(values));
    }

    /// Panics if only the builtins layer remains (matches Django's
    /// `ContextPopException`).
    pub fn pop(&mut self) -> ContextDict {
        if self.dicts.len() <= 1 {
            panic!("pop() called on BaseContext with only the builtins layer remaining");
        }
        to_external(self.dicts.pop().expect("dicts is non-empty"))
    }

    /// Push, run `f`, pop. Always pops, even on panic.
    pub fn scope<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.push();
        let result = f(self);
        self.pop();
        result
    }

    pub fn scope_with<F, R>(&mut self, values: ContextDict, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.push_with(values);
        let result = f(self);
        self.pop();
        result
    }

    /// Top-down lookup. `None` if not found. Use `get_or_err` to get a
    /// KeyError-style result. Hot path: top scope probed by direct index
    /// (avoids DoubleEndedIterator bookkeeping); the lower-scope walk
    /// is a cold helper.
    #[inline]
    pub fn get(&self, key: &str) -> Option<&Value> {
        let n = self.dicts.len();
        if n == 0 {
            return None;
        }
        // In `{% for %}` loops the loop var lives in the top scope.
        if let Some(v) = self.dicts[n - 1].get(key) {
            return Some(v);
        }
        if n == 1 {
            return None;
        }
        self.get_in_lower_scopes(key, n - 1)
    }

    #[cold]
    #[inline(never)]
    fn get_in_lower_scopes(&self, key: &str, start: usize) -> Option<&Value> {
        let mut i = start;
        while i > 0 {
            i -= 1;
            if let Some(v) = self.dicts[i].get(key) {
                return Some(v);
            }
        }
        None
    }

    #[inline]
    pub fn get_or_default<'a>(&'a self, key: &str, default: &'a Value) -> &'a Value {
        self.get(key).unwrap_or(default)
    }

    /// Like Django's `__getitem__` (raises KeyError).
    #[inline]
    pub fn get_or_err(&self, key: &str) -> Result<&Value, ContextKeyError> {
        self.get(key).ok_or_else(|| ContextKeyError(key.to_owned()))
    }

    /// Set in the topmost dict. Matches `__setitem__`.
    #[inline]
    pub fn set(&mut self, key: impl Into<String>, value: Value) {
        self.dicts
            .last_mut()
            .expect("dicts always has at least one layer")
            .insert(key.into(), value);
    }

    /// Mutable lookup in the topmost dict only (no walk). Used by
    /// `{% for %}` to mutate `forloop` in place per iteration.
    #[inline]
    pub fn get_in_topmost_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.dicts
            .last_mut()
            .expect("dicts always has at least one layer")
            .get_mut(key)
    }

    /// Mutable lookup searching all scopes (top-down). Returns a
    /// mutable reference from the first scope that contains `key`.
    /// Used by `{% ifchanged %}` to store state in the forloop dict.
    #[inline]
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        let n = self.dicts.len();
        for i in (0..n).rev() {
            if self.dicts[i].contains_key(key) {
                return self.dicts[i].get_mut(key);
            }
        }
        None
    }

    /// Matches `BaseContext.__contains__`.
    #[inline]
    pub fn contains(&self, key: &str) -> bool {
        let n = self.dicts.len();
        for i in (0..n).rev() {
            if self.dicts[i].contains_key(key) {
                return true;
            }
        }
        false
    }

    /// Set in the highest scope that already contains the key, else
    /// the topmost. Matches `BaseContext.set_upward`.
    pub fn set_upward(&mut self, key: &str, value: Value) {
        for d in self.dicts.iter_mut().rev() {
            if d.contains_key(key) {
                d.insert(key.to_owned(), value);
                return;
            }
        }
        self.dicts
            .last_mut()
            .expect("dicts always has at least one layer")
            .insert(key.to_owned(), value);
    }

    /// Matches `BaseContext.setdefault`.
    pub fn setdefault(&mut self, key: &str, default: Value) -> &Value {
        if self.contains(key) {
            return self.get(key).unwrap();
        }
        self.dicts
            .last_mut()
            .expect("dicts always has at least one layer")
            .insert(key.to_owned(), default);
        self.dicts.last().unwrap().get(key).unwrap()
    }

    /// Higher scopes override lower. Matches `BaseContext.flatten`.
    pub fn flatten(&self) -> HashMap<String, Value> {
        let mut result: HashMap<String, Value> = HashMap::new();
        for d in &self.dicts {
            for (k, v) in d {
                result.insert(k.clone(), v.clone());
            }
        }
        result
    }

    /// Matches `BaseContext.new`.
    pub fn new_child(&self, values: Option<ContextDict>) -> Self {
        match values {
            Some(v) => Self::with_values(v),
            None => Self::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.dicts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dicts.len() <= 1
    }

    /// Unique keys from top to bottom (higher shadows lower).
    pub fn keys(&self) -> Vec<&str> {
        let mut seen = HashMap::new();
        for d in self.dicts.iter().rev() {
            for k in d.keys() {
                seen.entry(k.as_str()).or_insert(());
            }
        }
        seen.into_keys().collect()
    }
}

impl Default for BaseContext {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for BaseContext {
    fn eq(&self, other: &Self) -> bool {
        self.dicts == other.dicts
    }
}

#[derive(Debug, Clone)]
pub struct ContextKeyError(pub String);

impl fmt::Display for ContextKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyError: '{}'", self.0)
    }
}

impl std::error::Error for ContextKeyError {}

/// Opaque template reference (Python object until a Rust Template type exists).
#[derive(Clone, Debug)]
pub struct TemplateRef {
    pub name: String,
    pub obj: Py<PyAny>,
}

/// Primary rendering context: `BaseContext` plus engine fields
/// (autoescape, l10n, tz, render_context, template binding). Mirrors
/// `django.template.context.Context`.
#[derive(Clone, Debug)]
pub struct Context {
    pub base: BaseContext,
    pub autoescape: bool,
    pub use_l10n: Option<bool>,
    pub use_tz: Option<bool>,
    pub template_name: Option<String>,
    pub render_context: RenderContext,
    pub template: Option<TemplateRef>,
    /// Output when a variable lookup fails (Django's `string_if_invalid`).
    pub string_if_invalid: String,
    /// Set by `ForNode` per-iteration when its body has a `ForBatchPlan`.
    /// Variable resolution checks this before the dynamic path, making
    /// `{{ app.candidate.name }}` an O(1) probe + tuple read instead of
    /// two getattr calls.
    pub loop_batch_cache: Option<LoopBatchCache>,
    /// `{% extends %}` inheritance state. Set by `ExtendsNode`, read by
    /// `BlockNode`. Living on Context (not a thread-local) means nested
    /// `Template::render` calls (e.g. custom tags rendering another
    /// template) naturally get a fresh slot via PyTemplate.render
    /// building a new Context.
    pub block_context: Option<crate::tags::loader_tags::BlockContext>,
    /// The Django `Engine` instance that owns this rendering session.
    /// Used by `{% extends %}` and `{% include %}` to load templates
    /// through the Engine's loader chain (locmem, filesystem, cached,
    /// etc.) instead of the global `django.template.loader`.
    pub engine: Option<Py<PyAny>>,
    /// Engine debug flag. When true, `VariableDoesNotExist` is raised
    /// instead of silently substituting `string_if_invalid`.
    pub debug: bool,
}

/// Per-iteration cache populated by ForNode when its body has a
/// parse-time `ForBatchPlan`. Variable resolution uses `path_to_slot`
/// to map dotted paths to tuple slots; the tuple is the output of
/// `operator.attrgetter(*paths)(item)` for the current iteration.
#[derive(Clone, Debug)]
pub struct LoopBatchCache {
    pub loopvar: String,
    pub path_to_slot: std::sync::Arc<std::collections::HashMap<String, u16>>,
    pub current_tuple: Py<pyo3::PyAny>,
}

impl Context {
    pub fn new(values: Option<ContextDict>) -> Self {
        Self {
            base: match values {
                Some(mut v) => {
                    v.remove("True");
                    v.remove("False");
                    v.remove("None");
                    BaseContext::with_values(v)
                }
                None => BaseContext::new(),
            },
            autoescape: true,
            use_l10n: None,
            use_tz: None,
            template_name: None,
            render_context: RenderContext::new(),
            template: None,
            string_if_invalid: String::new(),
            loop_batch_cache: None,
            block_context: None,
            engine: None,
            debug: false,
        }
    }

    pub fn push(&mut self) {
        self.base.push();
    }

    pub fn push_with(&mut self, values: ContextDict) {
        self.base.push_with(values);
    }

    pub fn pop(&mut self) -> ContextDict {
        self.base.pop()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.base.get(key)
    }

    pub fn get_or_default<'a>(&'a self, key: &str, default: &'a Value) -> &'a Value {
        self.base.get_or_default(key, default)
    }

    pub fn get_or_err(&self, key: &str) -> Result<&Value, ContextKeyError> {
        self.base.get_or_err(key)
    }

    pub fn set(&mut self, key: impl Into<String>, value: Value) {
        self.base.set(key, value);
    }

    pub fn get_in_topmost_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.base.get_in_topmost_mut(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.base.contains(key)
    }

    pub fn set_upward(&mut self, key: &str, value: Value) {
        self.base.set_upward(key, value);
    }

    pub fn setdefault(&mut self, key: &str, default: Value) -> &Value {
        self.base.setdefault(key, default)
    }

    pub fn flatten(&self) -> HashMap<String, Value> {
        self.base.flatten()
    }

    pub fn scope<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.base.push();
        let result = f(self);
        self.base.pop();
        result
    }

    pub fn scope_with<F, R>(&mut self, values: ContextDict, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.base.push_with(values);
        let result = f(self);
        self.base.pop();
        result
    }

    /// Fresh dict stack, inheriting settings. Matches `Context.new`.
    pub fn new_child(&self, values: Option<ContextDict>) -> Self {
        Self {
            base: self.base.new_child(values),
            autoescape: self.autoescape,
            use_l10n: self.use_l10n,
            use_tz: self.use_tz,
            template_name: None,
            render_context: RenderContext::new(),
            template: None,
            string_if_invalid: self.string_if_invalid.clone(),
            loop_batch_cache: None,
            block_context: None,
            engine: self.engine.clone(),
            debug: self.debug,
        }
    }

    /// Panics if a template is already bound (matches Django's assertion).
    pub fn bind_template<F, R>(&mut self, template: TemplateRef, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        assert!(
            self.template.is_none(),
            "Context is already bound to a template"
        );
        self.template = Some(template);
        let result = f(self);
        self.template = None;
        result
    }

    /// Matches `Context.update`.
    pub fn update<F, R>(&mut self, other: ContextDict, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.scope_with(other, f)
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new(None)
    }
}

impl PartialEq for Context {
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base
    }
}

/// `RenderContext` key. Mirrors Django's plain-dict semantics: both
/// string variable names and arbitrary Python objects (used by
/// `{% include %}`, InclusionNode, django-cotton). PyObject hash is
/// cached at construction; equality tries identity before Python `__eq__`.
#[derive(Clone, Debug)]
pub enum RenderKey {
    Str(String),
    PyObject { hash: isize, obj: Py<PyAny> },
}

impl RenderKey {
    /// String-like keys become `Str`; others become `PyObject` with
    /// cached `__hash__`.
    pub fn from_py(key: &Bound<'_, PyAny>) -> PyResult<Self> {
        if let Ok(s) = key.cast::<PyString>() {
            return Ok(RenderKey::Str(s.to_str()?.to_owned()));
        }
        let hash = key.hash()?;
        Ok(RenderKey::PyObject {
            hash,
            obj: key.clone().unbind(),
        })
    }
}

impl std::hash::Hash for RenderKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            RenderKey::Str(s) => {
                0u8.hash(state);
                s.hash(state);
            }
            RenderKey::PyObject { hash, .. } => {
                1u8.hash(state);
                hash.hash(state);
            }
        }
    }
}

impl PartialEq for RenderKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (RenderKey::Str(a), RenderKey::Str(b)) => a == b,
            (
                RenderKey::PyObject { obj: a, hash: ha },
                RenderKey::PyObject { obj: b, hash: hb },
            ) => {
                if ha != hb {
                    return false;
                }
                // Identity is cheap and covers Node instances (the
                // common case). Fall back to Python __eq__ on hash hit.
                if a.is(b) {
                    return true;
                }
                Python::attach(|py| a.bind(py).eq(b.bind(py)).unwrap_or(false))
            }
            _ => false,
        }
    }
}

impl Eq for RenderKey {}

impl From<String> for RenderKey {
    fn from(s: String) -> Self {
        RenderKey::Str(s)
    }
}

impl From<&str> for RenderKey {
    fn from(s: &str) -> Self {
        RenderKey::Str(s.to_owned())
    }
}

type RenderDict = FastMap<RenderKey, Value>;

/// Top-only-lookup context stack for per-render tag state (`{% cycle %}`,
/// `{% include %}` caching). Mirrors Django's `RenderContext`.
#[derive(Clone, Debug)]
pub struct RenderContext {
    pub dicts: Vec<RenderDict>,
    pub template: Option<TemplateRef>,
}

impl RenderContext {
    pub fn new() -> Self {
        Self {
            dicts: vec![RenderDict::default()],
            template: None,
        }
    }

    pub fn with_values(values: ContextDict) -> Self {
        let mut layer = RenderDict::default();
        for (k, v) in values {
            layer.insert(RenderKey::Str(k), v);
        }
        Self {
            dicts: vec![layer],
            template: None,
        }
    }

    #[inline]
    pub fn push(&mut self) {
        self.dicts.push(RenderDict::default());
    }

    #[inline]
    pub fn push_with(&mut self, values: ContextDict) {
        let mut layer = RenderDict::default();
        for (k, v) in values {
            layer.insert(RenderKey::Str(k), v);
        }
        self.dicts.push(layer);
    }

    /// Returns string-keyed entries; object-keyed are dropped. Panics
    /// if the stack would be empty.
    pub fn pop(&mut self) -> ContextDict {
        if self.dicts.len() <= 1 {
            panic!("pop() called on RenderContext with only one layer remaining");
        }
        let layer = self.dicts.pop().expect("dicts is non-empty");
        let mut out = ContextDict::with_capacity(layer.len());
        for (k, v) in layer {
            if let RenderKey::Str(s) = k {
                out.insert(s, v);
            }
        }
        out
    }

    /// Top-only lookup. Short string keys keep the allocation cheap.
    pub fn get(&self, key: &str) -> Option<&Value> {
        let probe = RenderKey::Str(key.to_owned());
        self.dicts.last().and_then(|d| d.get(&probe))
    }

    pub fn get_or_err(&self, key: &str) -> Result<&Value, ContextKeyError> {
        self.get(key).ok_or_else(|| ContextKeyError(key.to_owned()))
    }

    pub fn get_key(&self, key: &RenderKey) -> Option<&Value> {
        self.dicts.last().and_then(|d| d.get(key))
    }

    pub fn set(&mut self, key: impl Into<String>, value: Value) {
        self.dicts
            .last_mut()
            .expect("dicts always has at least one layer")
            .insert(RenderKey::Str(key.into()), value);
    }

    pub fn set_key(&mut self, key: RenderKey, value: Value) {
        self.dicts
            .last_mut()
            .expect("dicts always has at least one layer")
            .insert(key, value);
    }

    pub fn contains(&self, key: &str) -> bool {
        let probe = RenderKey::Str(key.to_owned());
        self.dicts.last().is_some_and(|d| d.contains_key(&probe))
    }

    /// Returns `true` if `key` is present in the **topmost** dict only.
    pub fn contains_key(&self, key: &RenderKey) -> bool {
        self.dicts.last().is_some_and(|d| d.contains_key(key))
    }

    /// Returns all string-typed keys in the topmost dict. Object keys
    /// are skipped because the existing callers (`PyRenderContext.__repr__`)
    /// only need a human-readable view.
    pub fn keys(&self) -> Vec<&str> {
        self.dicts
            .last()
            .map(|d| {
                d.keys()
                    .filter_map(|k| match k {
                        RenderKey::Str(s) => Some(s.as_str()),
                        RenderKey::PyObject { .. } => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Save template, optionally push a scope, run `f`, restore.
    /// Matches `RenderContext.push_state`.
    pub fn push_state<F, R>(
        &mut self,
        template: Option<TemplateRef>,
        isolated_context: bool,
        f: F,
    ) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        let old_template = self.template.take();

        if let Some(t) = template {
            self.template = Some(t);
        }

        if isolated_context {
            self.push();
        }

        let result = f(self);

        if isolated_context {
            self.pop();
        }

        self.template = old_template;
        result
    }
}

impl Default for RenderContext {
    fn default() -> Self {
        Self::new()
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn val_int(n: i64) -> Value {
        Value::Int(n)
    }

    fn val_str(s: &str) -> Value {
        Value::String(s.to_owned())
    }

    fn dict_from(pairs: &[(&str, Value)]) -> ContextDict {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn test_basic_push_pop_get() {
        let mut ctx = BaseContext::with_values(dict_from(&[("a", val_int(1))]));

        assert_eq!(ctx.get("a"), Some(&val_int(1)));

        // push, shadow, pop
        ctx.push();
        ctx.set("a", val_int(2));
        assert_eq!(ctx.get("a"), Some(&val_int(2)));

        let popped = ctx.pop();
        assert_eq!(popped.get("a"), Some(&val_int(2)));
        assert_eq!(ctx.get("a"), Some(&val_int(1)));
    }

    #[test]
    fn test_push_pop_with_scope() {
        let mut ctx = BaseContext::with_values(dict_from(&[("a", val_int(1))]));

        ctx.scope(|ctx| {
            ctx.set("a", val_int(2));
            assert_eq!(ctx.get("a"), Some(&val_int(2)));
        });
        // scope ended -> pop
        assert_eq!(ctx.get("a"), Some(&val_int(1)));
    }

    #[test]
    fn test_push_with_values() {
        let mut ctx = BaseContext::with_values(dict_from(&[("a", val_int(1))]));

        ctx.scope_with(dict_from(&[("a", val_int(3))]), |ctx| {
            assert_eq!(ctx.get("a"), Some(&val_int(3)));
        });
        assert_eq!(ctx.get("a"), Some(&val_int(1)));
    }

    #[test]
    #[should_panic(expected = "pop() called on BaseContext")]
    fn test_pop_builtins_panics() {
        let mut ctx = BaseContext::new();
        ctx.pop(); // only builtins remain -> panic
    }

    #[test]
    fn test_get_missing_returns_none() {
        let ctx = BaseContext::new();
        assert_eq!(ctx.get("nonexistent"), None);
    }

    #[test]
    fn test_get_or_default() {
        let ctx = BaseContext::new();
        let default = val_int(42);
        assert_eq!(ctx.get_or_default("foo", &default), &val_int(42));
    }

    #[test]
    fn test_get_or_err() {
        let ctx = BaseContext::new();
        assert!(ctx.get_or_err("missing").is_err());

        let ctx2 = BaseContext::with_values(dict_from(&[("x", val_int(5))]));
        assert_eq!(ctx2.get_or_err("x").unwrap(), &val_int(5));
    }

    #[test]
    fn test_variable_shadowing() {
        let mut ctx = BaseContext::with_values(dict_from(&[("x", val_str("outer"))]));
        ctx.push_with(dict_from(&[("x", val_str("inner"))]));

        // Top-of-stack value wins.
        assert_eq!(ctx.get("x"), Some(&val_str("inner")));

        ctx.pop();
        assert_eq!(ctx.get("x"), Some(&val_str("outer")));
    }

    #[test]
    fn test_multiple_scope_shadowing() {
        let mut ctx = BaseContext::with_values(dict_from(&[("a", val_int(1))]));
        ctx.push_with(dict_from(&[("a", val_int(2))]));
        ctx.push_with(dict_from(&[("a", val_int(3))]));

        assert_eq!(ctx.get("a"), Some(&val_int(3)));
        ctx.pop();
        assert_eq!(ctx.get("a"), Some(&val_int(2)));
        ctx.pop();
        assert_eq!(ctx.get("a"), Some(&val_int(1)));
    }

    #[test]
    fn test_render_context_top_only_get() {
        let mut rc = RenderContext::with_values(dict_from(&[("fruit", val_str("papaya"))]));

        // push hides "fruit"
        rc.push();
        rc.set("vegetable", val_str("artichoke"));

        assert_eq!(rc.get("vegetable"), Some(&val_str("artichoke")));
        assert_eq!(rc.get("fruit"), None); // NOT visible!
        assert!(!rc.contains("fruit"));
        assert!(rc.contains("vegetable"));
    }

    #[test]
    fn test_render_context_keys_top_only() {
        let mut rc = RenderContext::with_values(dict_from(&[("fruit", val_str("papaya"))]));
        rc.push();
        rc.set("vegetable", val_str("artichoke"));

        let keys = rc.keys();
        assert_eq!(keys, vec!["vegetable"]);
    }

    #[test]
    fn test_render_context_pop_restores() {
        let mut rc = RenderContext::with_values(dict_from(&[("fruit", val_str("papaya"))]));
        rc.push();
        rc.set("vegetable", val_str("artichoke"));

        rc.pop();
        assert_eq!(rc.get("fruit"), Some(&val_str("papaya")));
        assert_eq!(rc.get("vegetable"), None);
    }

    #[test]
    #[should_panic(expected = "pop() called on RenderContext")]
    fn test_render_context_pop_empty_panics() {
        let mut rc = RenderContext::new();
        rc.pop();
    }

    #[test]
    fn test_builtins_available() {
        let ctx = BaseContext::new();

        assert_eq!(ctx.get("True"), Some(&Value::Bool(true)));
        assert_eq!(ctx.get("False"), Some(&Value::Bool(false)));
        assert_eq!(ctx.get("None"), Some(&Value::None));
    }

    #[test]
    fn test_builtins_in_context() {
        let ctx = Context::new(None);

        assert_eq!(ctx.get("True"), Some(&Value::Bool(true)));
        assert_eq!(ctx.get("False"), Some(&Value::Bool(false)));
        assert_eq!(ctx.get("None"), Some(&Value::None));
    }

    #[test]
    fn test_builtins_in_flatten() {
        let ctx = BaseContext::new();
        let flat = ctx.flatten();
        assert_eq!(flat.get("True"), Some(&Value::Bool(true)));
        assert_eq!(flat.get("False"), Some(&Value::Bool(false)));
        assert_eq!(flat.get("None"), Some(&Value::None));
    }

    #[test]
    fn test_set_upward_existing_key() {
        let mut ctx = BaseContext::with_values(dict_from(&[("a", val_int(1))]));
        ctx.set_upward("a", val_int(2));
        assert_eq!(ctx.get("a"), Some(&val_int(2)));
    }

    #[test]
    fn test_set_upward_sets_in_highest_scope() {
        // Django test: `test_set_upward_with_push`
        let mut ctx = BaseContext::with_values(dict_from(&[("a", val_int(1))]));
        ctx.push_with(dict_from(&[("a", val_int(2))]));

        ctx.set_upward("a", val_int(3));

        // The topmost scope that has "a" (dicts[2]) gets the update.
        assert_eq!(ctx.get("a"), Some(&val_int(3)));

        // After popping, the lower scope is untouched.
        ctx.pop();
        assert_eq!(ctx.get("a"), Some(&val_int(1)));
    }

    #[test]
    fn test_set_upward_missing_key_goes_to_top() {
        // Django test: `test_set_upward_with_push_no_match`
        let mut ctx = BaseContext::with_values(dict_from(&[("b", val_int(1))]));
        ctx.push_with(dict_from(&[("b", val_int(2))]));

        ctx.set_upward("a", val_int(2));

        assert_eq!(ctx.dicts.len(), 3); // builtins + two pushed
        assert_eq!(ctx.dicts.last().unwrap().get("a"), Some(&val_int(2)));
    }

    #[test]
    fn test_set_upward_empty_context() {
        // Django test: `test_set_upward_empty_context`
        let mut ctx = BaseContext::new();
        ctx.set_upward("a", val_int(1));
        assert_eq!(ctx.get("a"), Some(&val_int(1)));
    }

    #[test]
    fn test_flatten_merges_all_dicts() {
        let mut ctx = BaseContext::new();
        ctx.push_with(dict_from(&[("a", val_int(2))]));
        ctx.push_with(dict_from(&[("b", val_int(4))]));
        ctx.push_with(dict_from(&[("c", val_int(8))]));

        let flat = ctx.flatten();
        assert_eq!(flat.get("a"), Some(&val_int(2)));
        assert_eq!(flat.get("b"), Some(&val_int(4)));
        assert_eq!(flat.get("c"), Some(&val_int(8)));
        // builtins still present
        assert_eq!(flat.get("True"), Some(&Value::Bool(true)));
    }

    #[test]
    fn test_flatten_later_overrides_earlier() {
        let mut ctx = BaseContext::with_values(dict_from(&[("a", val_int(1))]));
        ctx.push_with(dict_from(&[("a", val_int(99))]));

        let flat = ctx.flatten();
        assert_eq!(flat.get("a"), Some(&val_int(99)));
    }

    #[test]
    fn test_context_push_pop() {
        let mut c = Context::new(Some(dict_from(&[
            ("a", val_int(1)),
            ("b", val_str("xyzzy")),
        ])));

        assert_eq!(c.get("a"), Some(&val_int(1)));

        c.scope(|c| {
            c.set("a", val_int(2));
            assert_eq!(c.get("a"), Some(&val_int(2)));
        });
        assert_eq!(c.get("a"), Some(&val_int(1)));
    }

    #[test]
    fn test_context_update() {
        let mut c = Context::new(Some(dict_from(&[("a", val_int(1))])));

        c.update(dict_from(&[("a", val_int(3))]), |c| {
            assert_eq!(c.get("a"), Some(&val_int(3)));
        });
        assert_eq!(c.get("a"), Some(&val_int(1)));
    }

    #[test]
    fn test_context_new_child() {
        let parent = Context::new(Some(dict_from(&[("a", val_int(2))])));
        let child = parent.new_child(Some(dict_from(&[("b", val_int(4))])));

        // Child has fresh dicts; parent's "a" is not visible.
        assert_eq!(child.get("b"), Some(&val_int(4)));
        assert_eq!(child.get("a"), None);
        // Builtins still present.
        assert_eq!(child.get("True"), Some(&Value::Bool(true)));
        // Settings inherited.
        assert!(child.autoescape);
    }

    #[test]
    fn test_context_autoescape_default() {
        let c = Context::new(None);
        assert!(c.autoescape);
    }

    #[test]
    fn test_context_contains() {
        let ctx = Context::new(Some(dict_from(&[("a", val_int(1))])));
        assert!(ctx.contains("a"));
        assert!(ctx.contains("True")); // builtin
        assert!(!ctx.contains("zzz"));
    }

    #[test]
    fn test_context_equality() {
        let a = Context::new(Some(dict_from(&[("x", val_str("y"))])));
        let b = Context::new(Some(dict_from(&[("x", val_str("y"))])));
        assert_eq!(a, b);
    }

    #[test]
    fn test_context_inequality() {
        let mut a = Context::new(None);
        let b = Context::new(None);

        a.base.push_with(dict_from(&[("a", val_int(1))]));
        assert_ne!(a, b);
    }

    #[test]
    fn test_setdefault() {
        let mut ctx = BaseContext::new();

        let v = ctx.setdefault("x", val_int(42));
        assert_eq!(v, &val_int(42));
        assert_eq!(ctx.get("x"), Some(&val_int(42)));

        // Calling again with a different default returns the existing value.
        let v = ctx.setdefault("x", val_int(100));
        assert_eq!(v, &val_int(42));
        assert_eq!(ctx.get("x"), Some(&val_int(42)));
    }

    #[test]
    fn test_render_context_push_state_isolated() {
        let mut rc = RenderContext::with_values(dict_from(&[("a", val_int(1))]));

        rc.push_state(None, true, |inner| {
            // "a" is not visible in the isolated scope.
            assert_eq!(inner.get("a"), None);
            inner.set("b", val_int(2));
            assert_eq!(inner.get("b"), Some(&val_int(2)));
        });

        // After push_state, the isolated scope is gone.
        assert_eq!(rc.get("a"), Some(&val_int(1)));
        assert_eq!(rc.get("b"), None);
    }

    #[test]
    fn test_render_context_push_state_non_isolated() {
        let mut rc = RenderContext::with_values(dict_from(&[("a", val_int(1))]));

        rc.push_state(None, false, |inner| {
            // Non-isolated: top dict is still accessible.
            assert_eq!(inner.get("a"), Some(&val_int(1)));
        });
    }

    #[test]
    fn test_value_from_primitives() {
        assert_eq!(Value::from(true), Value::Bool(true));
        assert_eq!(Value::from(42i64), Value::Int(42));
        assert_eq!(Value::from(3.14f64), Value::Float(3.14));
        assert_eq!(Value::from("hello"), Value::String("hello".into()));
    }

    #[test]
    fn test_value_display() {
        assert_eq!(format!("{}", Value::None), "None");
        assert_eq!(format!("{}", Value::Bool(true)), "True");
        assert_eq!(format!("{}", Value::Bool(false)), "False");
        assert_eq!(format!("{}", Value::Int(42)), "42");
        assert_eq!(format!("{}", Value::String("hi".into())), "hi");
    }

    #[test]
    fn test_value_equality() {
        assert_eq!(Value::None, Value::None);
        assert_eq!(Value::Bool(true), Value::Bool(true));
        assert_ne!(Value::Bool(true), Value::Bool(false));
        assert_ne!(Value::Int(1), Value::String("1".into()));
    }

    #[test]
    fn test_base_context_len() {
        let ctx = BaseContext::new();
        assert_eq!(ctx.len(), 1); // builtins only

        let ctx2 = BaseContext::with_values(dict_from(&[("a", val_int(1))]));
        assert_eq!(ctx2.len(), 2);
    }

    #[test]
    fn test_base_context_is_empty() {
        let ctx = BaseContext::new();
        assert!(ctx.is_empty());

        let ctx2 = BaseContext::with_values(dict_from(&[("a", val_int(1))]));
        assert!(!ctx2.is_empty());
    }

    #[test]
    fn test_reset_dicts() {
        let mut ctx = BaseContext::with_values(dict_from(&[("a", val_int(1))]));
        ctx.push_with(dict_from(&[("b", val_int(2))]));

        ctx.reset_dicts(Some(dict_from(&[("c", val_int(3))])));

        assert_eq!(ctx.dicts.len(), 2);
        assert_eq!(ctx.get("c"), Some(&val_int(3)));
        assert_eq!(ctx.get("a"), None);
        assert_eq!(ctx.get("True"), Some(&Value::Bool(true)));
    }

    #[test]
    fn test_reset_dicts_none() {
        let mut ctx = BaseContext::with_values(dict_from(&[("a", val_int(1))]));
        ctx.reset_dicts(None);

        assert_eq!(ctx.dicts.len(), 1);
        assert_eq!(ctx.get("a"), None);
        assert_eq!(ctx.get("True"), Some(&Value::Bool(true)));
    }

    #[test]
    fn test_value_from_python_none() {
        Python::attach(|py| {
            let binding = pyo3::types::PyNone::get(py);
            let none = binding.as_any();
            let v = Value::from(none);
            assert_eq!(v, Value::None);
        });
    }

    #[test]
    fn test_value_from_python_bool() {
        Python::attach(|py| {
            let t = true.into_pyobject(py).unwrap();
            let v = Value::from(t.as_any());
            assert_eq!(v, Value::Bool(true));
        });
    }

    #[test]
    fn test_value_from_python_int() {
        Python::attach(|py| {
            let n = 42i64.into_pyobject(py).unwrap();
            let v = Value::from(n.as_any());
            assert_eq!(v, Value::Int(42));
        });
    }

    #[test]
    fn test_value_from_python_float() {
        Python::attach(|py| {
            let f = 3.14f64.into_pyobject(py).unwrap();
            let v = Value::from(f.as_any());
            assert_eq!(v, Value::Float(3.14));
        });
    }

    #[test]
    fn test_value_from_python_string() {
        Python::attach(|py| {
            let s = "hello".into_pyobject(py).unwrap();
            let v = Value::from(s.as_any());
            assert_eq!(v, Value::String("hello".into()));
        });
    }

    #[test]
    fn test_value_from_python_list() {
        // Lists are kept as PyObject (lazy access) for performance.
        Python::attach(|py| {
            let list = PyList::new(py, &[1i64, 2, 3]).unwrap();
            let v = Value::from(list.as_any());
            assert!(matches!(v, Value::PyObject(_)));
        });
    }

    #[test]
    fn test_value_from_python_dict() {
        // Dicts are kept as PyObject (lazy access) for performance.
        Python::attach(|py| {
            let dict = PyDict::new(py);
            dict.set_item("a", 1i64).unwrap();
            dict.set_item("b", "two").unwrap();
            let v = Value::from(dict.as_any());
            assert!(matches!(v, Value::PyObject(_)));
        });
    }

    #[test]
    fn test_value_from_python_arbitrary_object() {
        Python::attach(|py| {
            // A Python `object()` instance has no special mapping.
            let obj = py.eval(c"object()", None, None).unwrap();
            let v = Value::from(&obj);
            assert!(matches!(v, Value::PyObject(_)));
        });
    }

    #[test]
    fn test_value_roundtrip_to_pyobject() {
        Python::attach(|py| {
            let original = Value::Int(42);
            let py_obj = original.to_pyobject(py);
            let back = Value::from(py_obj.bind(py));
            assert_eq!(back, original);
        });
    }
}
