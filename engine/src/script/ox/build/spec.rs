//! The ScriptApi surface — single source of truth.
//!
//! Everything a `.ox` script can call is one line of [`SPEC`]. From it we
//! generate (1) `engine_api.ox`, the `fn native` declarations scripts import,
//! (2) the `__oxidized_natives` module of each script's cdylib shim, and
//! (3) the op table the host dispatches on. `host` natives cross the C ABI via
//! one function pointer; `inline` natives are plain Rust in the shim (math —
//! oxidized has no `sin`/`cos`/`clamp`/`PI` of its own).
//!
//! Adding an API: append a line, add a match arm in `runtime::dispatch`, bump
//! nothing else — the API hash (checked at load) changes automatically, so a
//! stale cached `.so` is rejected instead of calling the wrong op.

// NOTE: natives must not return `Bool`: oxidized emits `.is_truthy()` on the result, which
// `bool` lacks, so `if key_down(..)` fails in rustc. Return `Int` (1/0) and compare `== 1`.
pub const SPEC: &str = r#"
host log(msg: String)
host time() -> Float
host pos(axis: Int) -> Float
host set_pos(x: Float, y: Float, z: Float)
host move_by(dx: Float, dy: Float, dz: Float)
host rot(axis: Int) -> Float
host set_rot(x: Float, y: Float, z: Float)
host scale(axis: Int) -> Float
host set_scale(x: Float, y: Float, z: Float)
host key_down(key: String) -> Int
host key_pressed(key: String) -> Int
host axis(name: String) -> Float
host spawn(name: String, x: Float, y: Float, z: Float) -> Int
host despawn(id: Int)
host play_sound(path: String)
host play_tone(freq: Float, secs: Float)
host tunable(name: String, default: Float) -> Float
host get_var(name: String, default: Float) -> Float
host set_var(name: String, value: Float)
host hud_bar(name: String, fraction: Float)
host toast(msg: String, secs: Float)
host camera_shake(intensity: Float, secs: Float)
inline sin(x: Float) -> Float = x.sin()
inline cos(x: Float) -> Float = x.cos()
inline tan(x: Float) -> Float = x.tan()
inline atan2(y: Float, x: Float) -> Float = y.atan2(x)
inline clamp(x: Float, lo: Float, hi: Float) -> Float = x.max(lo).min(hi)
inline lerp(a: Float, b: Float, t: Float) -> Float = a + (b - a) * t
inline pi() -> Float = std::f64::consts::PI
inline tau() -> Float = std::f64::consts::TAU
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
}

impl Ty {
    pub fn ox(self) -> &'static str {
        match self {
            Ty::Int => "Int",
            Ty::Float => "Float",
            Ty::Bool => "Bool",
            Ty::Str => "String",
        }
    }
    pub fn rust(self) -> &'static str {
        match self {
            Ty::Int => "i64",
            Ty::Float => "f64",
            Ty::Bool => "bool",
            Ty::Str => "String",
        }
    }
    fn parse(s: &str) -> Ty {
        match s.trim() {
            "Int" => Ty::Int,
            "Float" => Ty::Float,
            "Bool" => Ty::Bool,
            "String" => Ty::Str,
            other => panic!("bad type in SPEC: {other}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Native {
    pub name: String,
    pub params: Vec<(String, Ty)>,
    pub ret: Option<Ty>,
    /// `Some(rust expr)` for inline natives; `None` for host calls.
    pub inline_body: Option<String>,
    /// Op index for host calls (position among `host` lines).
    pub op: u32,
}

pub fn natives() -> Vec<Native> {
    let mut out = Vec::new();
    let mut op = 0u32;
    for line in SPEC.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (kind, rest) = line.split_once(' ').expect("SPEC line");
        let (sig, body) = match rest.split_once(" = ") {
            Some((s, b)) => (s, Some(b.trim().to_string())),
            None => (rest, None),
        };
        let (head, ret) = match sig.split_once(") -> ") {
            Some((h, r)) => (format!("{h})"), Some(Ty::parse(r))),
            None => (sig.to_string(), None),
        };
        let open = head.find('(').expect("SPEC paren");
        let name = head[..open].trim().to_string();
        let inner = head[open + 1..head.len() - 1].trim();
        let params = if inner.is_empty() {
            Vec::new()
        } else {
            inner
                .split(',')
                .map(|p| {
                    let (n, t) = p.split_once(':').expect("SPEC param");
                    (n.trim().to_string(), Ty::parse(t))
                })
                .collect()
        };
        let is_host = kind == "host";
        out.push(Native {
            name,
            params,
            ret,
            inline_body: if is_host { None } else { body },
            op: if is_host { op } else { u32::MAX },
        });
        if is_host {
            op += 1;
        }
    }
    out
}

/// FNV-1a of the spec text; baked into every shim and checked at load.
pub fn api_hash() -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in SPEC.bytes().chain(SHIM_PRELUDE.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// `engine_api.ox`: the `fn native` declarations every script imports.
pub fn api_ox() -> String {
    let mut s = String::from("# GENERATED from engine/src/script/ox/build/spec.rs — do not edit.\n");
    for n in natives() {
        let params: Vec<String> = n.params.iter().map(|(p, t)| format!("{p}: {}", t.ox())).collect();
        let ret = n.ret.map(|t| format!(" -> {}", t.ox())).unwrap_or_default();
        s.push_str(&format!("fn native {}({}){};\n", n.name, params.join(", "), ret));
    }
    s
}

/// ABI value passed across the C boundary (host side mirror: `runtime::OxVal`).
pub const SHIM_PRELUDE: &str = r#"
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OxVal { pub tag: u32, pub i: i64, pub f: f64, pub s: *const u8, pub n: usize }
impl OxVal {
    const NIL: OxVal = OxVal { tag: 0, i: 0, f: 0.0, s: std::ptr::null(), n: 0 };
    fn int(i: i64) -> OxVal { OxVal { tag: 1, i, ..OxVal::NIL } }
    fn float(f: f64) -> OxVal { OxVal { tag: 2, f, ..OxVal::NIL } }
    fn boolean(b: bool) -> OxVal { OxVal { tag: 3, i: b as i64, ..OxVal::NIL } }
    fn string(s: &str) -> OxVal { OxVal { tag: 4, s: s.as_ptr(), n: s.len(), ..OxVal::NIL } }
}
pub type HostFn = extern "C" fn(op: u32, args: *const OxVal, nargs: usize, ret: *mut OxVal);
static HOST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static LAST_ERROR: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
fn host_call(op: u32, args: &[OxVal]) -> OxVal {
    let p = HOST.load(std::sync::atomic::Ordering::Relaxed);
    if p == 0 { panic!("script called a host function before ox_init"); }
    let f: HostFn = unsafe { std::mem::transmute::<usize, HostFn>(p) };
    let mut ret = OxVal::NIL;
    f(op, args.as_ptr(), args.len(), &mut ret);
    ret
}
fn guarded(f: impl FnOnce() + std::panic::UnwindSafe) -> i32 {
    match std::panic::catch_unwind(f) {
        Ok(()) => 0,
        Err(e) => {
            let msg = if let Some(s) = e.downcast_ref::<&str>() { s.to_string() }
                else if let Some(s) = e.downcast_ref::<String>() { s.clone() }
                else { "script panicked".to_string() };
            *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) = msg;
            1
        }
    }
}
#[no_mangle] pub extern "C" fn ox_init(f: HostFn) { HOST.store(f as usize, std::sync::atomic::Ordering::Relaxed); }
#[no_mangle] pub extern "C" fn ox_api_hash() -> u64 { OX_API_HASH }
#[no_mangle] pub extern "C" fn ox_last_error(out: *mut u8, cap: usize) -> usize {
    let g = LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner());
    let n = g.len().min(cap);
    unsafe { std::ptr::copy_nonoverlapping(g.as_ptr(), out, n); }
    n
}
"#;

/// Generates the `__oxidized_natives` module text for a shim.
pub fn shim_natives_rs() -> String {
    let mut s = String::from("pub mod __oxidized_natives {\n    use super::*;\n");
    for n in natives() {
        let params: Vec<String> = n.params.iter().map(|(p, t)| format!("{p}: {}", t.rust())).collect();
        let ret = n.ret.map(|t| format!(" -> {}", t.rust())).unwrap_or_default();
        if let Some(body) = &n.inline_body {
            s.push_str(&format!("    pub fn {}({}){} {{ {} }}\n", n.name, params.join(", "), ret, body));
            continue;
        }
        let args: Vec<String> = n
            .params
            .iter()
            .map(|(p, t)| match t {
                Ty::Int => format!("OxVal::int({p})"),
                Ty::Float => format!("OxVal::float({p})"),
                Ty::Bool => format!("OxVal::boolean({p})"),
                Ty::Str => format!("OxVal::string(&{p})"),
            })
            .collect();
        let call = format!("host_call({}, &[{}])", n.op, args.join(", "));
        let body = match n.ret {
            None => format!("{call};"),
            Some(Ty::Int) => format!("{call}.i"),
            Some(Ty::Float) => format!("{call}.f"),
            Some(Ty::Bool) => format!("{call}.i != 0"),
            Some(Ty::Str) => panic!("string returns unsupported"),
        };
        s.push_str(&format!("    pub fn {}({}){} {{ {} }}\n", n.name, params.join(", "), ret, body));
    }
    s.push_str("}\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parses() {
        let n = natives();
        assert!(n.iter().any(|x| x.name == "log" && x.op == 0));
        assert!(n.iter().any(|x| x.name == "sin" && x.inline_body.is_some()));
        let host_ops = n.iter().filter(|x| x.inline_body.is_none()).count();
        assert_eq!(n.iter().filter(|x| x.op != u32::MAX).count(), host_ops);
    }

    #[test]
    fn api_ox_declares_everything() {
        let ox = api_ox();
        assert!(ox.contains("fn native spawn(name: String, x: Float, y: Float, z: Float) -> Int;"));
        assert!(ox.contains("fn native tau() -> Float;"));
    }
}
