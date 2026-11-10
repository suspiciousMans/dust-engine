//! Loading compiled `.ox` cdylibs, dispatching hooks, and hot reload.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use libloading::Library;

use super::api::{BehaviorCtx, EntityState, OxApi, OxServices};
use super::build::compile::{build_script, BuildOptions, Toolchain};
use super::build::diag::ScriptDiagnostics;
use super::build::spec;
use crate::behavior::{Behavior, ScriptApi};

/// ABI value; layout must match `spec::SHIM_PRELUDE`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OxVal {
    pub tag: u32,
    pub i: i64,
    pub f: f64,
    pub s: *const u8,
    pub n: usize,
}

impl OxVal {
    const NIL: OxVal = OxVal { tag: 0, i: 0, f: 0.0, s: std::ptr::null(), n: 0 };
    fn int(i: i64) -> Self {
        Self { tag: 1, i, ..Self::NIL }
    }
    fn float(f: f64) -> Self {
        Self { tag: 2, f, ..Self::NIL }
    }
    fn boolean(b: bool) -> Self {
        Self { tag: 3, i: b as i64, ..Self::NIL }
    }
}

type HostFn = extern "C" fn(u32, *const OxVal, usize, *mut OxVal);
type Hook0 = unsafe extern "C" fn() -> i32;
type HookF = unsafe extern "C" fn(f64) -> i32;
type HookS = unsafe extern "C" fn(*const u8, usize) -> i32;

// ---------------------------------------------------------------- dispatch

thread_local! {
    static CTX: Cell<Option<NonNull<dyn OxApi>>> = const { Cell::new(None) };
}

struct CtxGuard(Option<NonNull<dyn OxApi>>);
impl CtxGuard {
    fn enter(api: &mut dyn OxApi) -> Self {
        // SAFETY: lifetime erased; the guard clears the pointer before `api`'s borrow ends.
        let p: NonNull<dyn OxApi> = unsafe { std::mem::transmute::<&mut dyn OxApi, NonNull<dyn OxApi>>(api) };
        CtxGuard(CTX.with(|c| c.replace(Some(p))))
    }
}
impl Drop for CtxGuard {
    fn drop(&mut self) {
        CTX.with(|c| c.set(self.0));
    }
}

fn host_ops() -> &'static Vec<String> {
    static OPS: OnceLock<Vec<String>> = OnceLock::new();
    OPS.get_or_init(|| spec::natives().into_iter().filter(|n| n.inline_body.is_none()).map(|n| n.name).collect())
}

extern "C" fn host_trampoline(op: u32, args: *const OxVal, nargs: usize, ret: *mut OxVal) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        *ret = OxVal::NIL;
        let args = std::slice::from_raw_parts(args, nargs);
        let Some(mut p) = CTX.with(|c| c.get()) else { return };
        *ret = dispatch(p.as_mut(), host_ops().get(op as usize).map(String::as_str).unwrap_or(""), args);
    }));
}

fn dispatch(api: &mut dyn OxApi, name: &str, a: &[OxVal]) -> OxVal {
    let f = |i: usize| -> f64 {
        a.get(i).map_or(0.0, |v| if v.tag == 1 { v.i as f64 } else { v.f })
    };
    let s = |i: usize| -> &str {
        a.get(i).filter(|v| v.tag == 4 && !v.s.is_null()).map_or("", |v| {
            std::str::from_utf8(unsafe { std::slice::from_raw_parts(v.s, v.n) }).unwrap_or("")
        })
    };
    let v3 = |i: usize| [f(i) as f32, f(i + 1) as f32, f(i + 2) as f32];
    let axis = |v: [f32; 3]| v.get(f(0) as usize).copied().unwrap_or(0.0) as f64;
    match name {
        "log" => api.log(s(0)),
        "time" => return OxVal::float(api.time()),
        "pos" => return OxVal::float(axis(api.position())),
        "set_pos" => api.set_position(v3(0)),
        "move_by" => {
            let (p, d) = (api.position(), v3(0));
            api.set_position([p[0] + d[0], p[1] + d[1], p[2] + d[2]]);
        }
        "rot" => return OxVal::float(axis(api.rotation())),
        "set_rot" => api.set_rotation(v3(0)),
        "scale" => return OxVal::float(axis(api.scale())),
        "set_scale" => api.set_scale(v3(0)),
        "key_down" => return OxVal::int(api.key_down(s(0)) as i64),
        "key_pressed" => return OxVal::int(api.key_pressed(s(0)) as i64),
        "axis" => return OxVal::float(api.axis(s(0)) as f64),
        "spawn" => return OxVal::int(api.spawn(s(0), v3(1))),
        "despawn" => api.despawn(f(0) as i64),
        "play_sound" => api.play_sound(s(0)),
        "play_tone" => api.play_tone(f(0) as f32, f(1) as f32),
        "tunable" => return OxVal::float(api.tunable(s(0), f(1))),
        "get_var" => return OxVal::float(api.var(s(0), f(1))),
        "set_var" => api.set_var(s(0), f(1)),
        "hud_bar" => api.hud_bar(s(0), f(1) as f32),
        "toast" => api.toast(s(0), f(1) as f32),
        "camera_shake" => api.camera_shake(f(0) as f32, f(1) as f32),
        other => log::error!("script called unknown host op '{other}'"),
    }
    OxVal::NIL
}

// ----------------------------------------------------------------- loading

/// One compiled-and-loaded script generation.
pub struct LoadedScript {
    pub name: String,
    pub generation: u64,
    pub hooks: Vec<String>,
    on_ready: Option<Hook0>,
    on_start: Option<Hook0>,
    on_update: Option<HookF>,
    on_interact: Option<Hook0>,
    on_trigger_enter: Option<HookS>,
    on_trigger_exit: Option<HookS>,
    last_error: unsafe extern "C" fn(*mut u8, usize) -> usize,
    // Never dlclose: a Rust cdylib's thread-locals/atexit handlers make unloading unsafe.
    _lib: std::mem::ManuallyDrop<Library>,
}

impl LoadedScript {
    pub fn load(path: &Path, name: &str, generation: u64) -> Result<Self, String> {
        // SAFETY: loading code we just compiled from the user's own script.
        unsafe {
            let lib = Library::new(path).map_err(|e| format!("cannot load {}: {e}", path.display()))?;
            let hash: libloading::Symbol<unsafe extern "C" fn() -> u64> =
                lib.get(b"ox_api_hash\0").map_err(|e| format!("not an oxidized script library: {e}"))?;
            if hash() != spec::api_hash() {
                return Err("script was built against a different ScriptApi version; rebuild it".into());
            }
            let init: libloading::Symbol<unsafe extern "C" fn(HostFn)> =
                lib.get(b"ox_init\0").map_err(|e| e.to_string())?;
            init(host_trampoline);
            macro_rules! hook {
                ($sym:literal) => {
                    lib.get(concat!($sym, "\0").as_bytes()).ok().map(|s: libloading::Symbol<_>| *s)
                };
            }
            let last_error = *lib
                .get::<unsafe extern "C" fn(*mut u8, usize) -> usize>(b"ox_last_error\0")
                .map_err(|e| e.to_string())?;
            let mut s = LoadedScript {
                name: name.to_string(),
                generation,
                hooks: Vec::new(),
                on_ready: hook!("ox_on_ready"),
                on_start: hook!("ox_on_start"),
                on_update: hook!("ox_on_update"),
                on_interact: hook!("ox_on_interact"),
                on_trigger_enter: hook!("ox_on_trigger_enter"),
                on_trigger_exit: hook!("ox_on_trigger_exit"),
                last_error,
                _lib: std::mem::ManuallyDrop::new(lib),
            };
            for (n, present) in [
                ("on_ready", s.on_ready.is_some()),
                ("on_start", s.on_start.is_some()),
                ("on_update", s.on_update.is_some()),
                ("on_interact", s.on_interact.is_some()),
                ("on_trigger_enter", s.on_trigger_enter.is_some()),
                ("on_trigger_exit", s.on_trigger_exit.is_some()),
            ] {
                if present {
                    s.hooks.push(n.to_string());
                }
            }
            Ok(s)
        }
    }

    fn check(&self, hook: &str, code: i32) -> Result<(), String> {
        if code == 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; 512];
        let n = unsafe { (self.last_error)(buf.as_mut_ptr(), buf.len()) };
        Err(format!("{}::{hook}: {}", self.name, String::from_utf8_lossy(&buf[..n])))
    }
}

/// A hot-swappable reference to "the current build of script X". Every
/// `OxBehavior` of that script shares one slot, so a reload retargets them all.
#[derive(Default)]
pub struct ScriptSlot {
    current: RwLock<Option<Arc<LoadedScript>>>,
}

impl ScriptSlot {
    pub fn get(&self) -> Option<Arc<LoadedScript>> {
        self.current.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set(&self, s: Arc<LoadedScript>) {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Some(s);
    }
}

// --------------------------------------------------------------- behavior

/// A `Behavior` implemented by an `.ox` script.
pub struct OxBehavior {
    slot: Arc<ScriptSlot>,
    services: Arc<OxServices>,
    pub state: EntityState,
    pub entity: u64,
    ready: bool,
    errors: Vec<String>,
}

impl OxBehavior {
    pub fn new(slot: Arc<ScriptSlot>, services: Arc<OxServices>) -> Self {
        Self { slot, services, state: EntityState::default(), entity: 0, ready: false, errors: Vec::new() }
    }

    /// Runtime errors (script panics) since the last call, for the console.
    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    /// Calls `f` against the current build with the full `OxApi`.
    pub fn with_api(&mut self, api: &mut dyn ScriptApi, f: impl FnOnce(&LoadedScript)) {
        let Some(script) = self.slot.get() else { return };
        let input = self.services.input();
        let mut ctx = BehaviorCtx { api, state: &mut self.state, services: &self.services, input: &input, entity: self.entity };
        let _g = CtxGuard::enter(&mut ctx);
        f(&script);
    }

    fn run(&mut self, api: &mut dyn ScriptApi, hook: &'static str, call: impl FnOnce(&LoadedScript) -> Option<i32>) {
        let mut result = None;
        self.with_api(api, |s| {
            if let Some(code) = call(s) {
                result = Some(s.check(hook, code));
            }
        });
        if let Some(Err(e)) = result {
            log::error!("script error: {e}");
            api.log(&format!("[script error] {e}"));
            self.errors.push(e);
        }
    }
}

impl Behavior for OxBehavior {
    fn on_ready(&mut self, api: &mut dyn ScriptApi) {
        self.ready = true;
        self.run(api, "on_ready", |s| {
            let a = s.on_ready.map(|f| unsafe { f() });
            let b = s.on_start.map(|f| unsafe { f() });
            // report the first failure, if any
            match (a, b) {
                (Some(x), _) if x != 0 => Some(x),
                (_, Some(y)) => Some(y),
                (x, None) => x,
            }
        });
    }
    fn on_update(&mut self, api: &mut dyn ScriptApi, dt: f32) {
        self.run(api, "on_update", |s| s.on_update.map(|f| unsafe { f(dt as f64) }));
    }
    fn on_interact(&mut self, api: &mut dyn ScriptApi) {
        self.run(api, "on_interact", |s| s.on_interact.map(|f| unsafe { f() }));
    }
    fn on_trigger_enter(&mut self, api: &mut dyn ScriptApi, other: &str) {
        self.run(api, "on_trigger_enter", |s| s.on_trigger_enter.map(|f| unsafe { f(other.as_ptr(), other.len()) }));
    }
    fn on_trigger_exit(&mut self, api: &mut dyn ScriptApi, other: &str) {
        self.run(api, "on_trigger_exit", |s| s.on_trigger_exit.map(|f| unsafe { f(other.as_ptr(), other.len()) }));
    }
}

// ---------------------------------------------------------------- runtime

/// Result of one (re)build, as surfaced to the editor console.
#[derive(Clone, Debug)]
pub struct ReloadReport {
    pub script: String,
    pub result: Result<(), ScriptDiagnostics>,
}

struct Watch {
    _watcher: notify::RecommendedWatcher,
    events: Receiver<PathBuf>,
    pending: Mutex<(HashSet<PathBuf>, Option<Instant>)>,
    results_tx: Sender<ReloadReport>,
    results_rx: Mutex<Receiver<ReloadReport>>,
    building: Arc<AtomicBool>,
}

pub struct OxRuntime {
    toolchain: Toolchain,
    scripts_dir: PathBuf,
    out_dir: PathBuf,
    services: Arc<OxServices>,
    shared: Arc<Shared>,
    watch: Option<Watch>,
}

#[derive(Default)]
struct Shared {
    slots: Mutex<HashMap<String, Arc<ScriptSlot>>>,
    diagnostics: Mutex<HashMap<String, ScriptDiagnostics>>,
    generation: AtomicU64,
}

impl Shared {
    fn slot(&self, name: &str) -> Arc<ScriptSlot> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).entry(name.to_string()).or_default().clone()
    }
}

fn rebuild(tc: &Toolchain, shared: &Shared, src: &Path, out_dir: &Path) -> ReloadReport {
    let name = src.file_stem().and_then(|s| s.to_str()).unwrap_or("script").to_string();
    let generation = shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
    let result = build_script(tc, src, out_dir, &BuildOptions { generation, ..Default::default() }).and_then(|built| {
        LoadedScript::load(&built.lib_path, &built.name, generation)
            .map(|l| shared.slot(&built.name).set(Arc::new(l)))
            .map_err(|m| ScriptDiagnostics::single(&src.display().to_string(), 0, 0, m))
    });
    let mut diags = shared.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
    match &result {
        Ok(()) => {
            diags.remove(&name);
        }
        Err(d) => {
            diags.insert(name.clone(), d.clone());
        }
    }
    ReloadReport { script: name, result }
}

impl OxRuntime {
    pub fn new(scripts_dir: impl Into<PathBuf>, out_dir: impl Into<PathBuf>) -> Result<Self, String> {
        Ok(Self::with_toolchain(Toolchain::discover()?, scripts_dir, out_dir))
    }

    pub fn with_toolchain(toolchain: Toolchain, scripts_dir: impl Into<PathBuf>, out_dir: impl Into<PathBuf>) -> Self {
        Self {
            toolchain,
            scripts_dir: scripts_dir.into(),
            out_dir: out_dir.into(),
            services: Arc::new(OxServices::default()),
            shared: Arc::new(Shared::default()),
            watch: None,
        }
    }

    pub fn services(&self) -> Arc<OxServices> {
        self.services.clone()
    }

    pub fn script_files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(&self.scripts_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |e| e == "ox"))
            .collect();
        v.sort();
        v
    }

    /// Builds + loads one script now (blocking, ~3 s). On failure the previous
    /// good build (if any) stays active and the diagnostics are returned.
    pub fn reload(&self, script_file: &Path) -> Result<(), ScriptDiagnostics> {
        rebuild(&self.toolchain, &self.shared, script_file, &self.out_dir).result
    }

    /// Builds every `*.ox` in the scripts dir; one report each.
    pub fn build_all(&self) -> Vec<ReloadReport> {
        self.script_files().iter().map(|f| rebuild(&self.toolchain, &self.shared, f, &self.out_dir)).collect()
    }

    /// A new behavior instance for `script` (file stem). Valid even before the
    /// first successful build: it no-ops until a build lands.
    pub fn behavior(&self, script: &str) -> OxBehavior {
        OxBehavior::new(self.shared.slot(script), self.services.clone())
    }

    /// Current build errors, all scripts, for the editor console.
    pub fn diagnostics(&self) -> ScriptDiagnostics {
        let d = self.shared.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<_> = d.keys().cloned().collect();
        names.sort();
        ScriptDiagnostics { items: names.iter().flat_map(|n| d[n].items.clone()).collect() }
    }

    /// Starts watching the scripts dir (notify). Call `poll()` each frame.
    pub fn start_watching(&mut self) -> Result<(), String> {
        use notify::{RecursiveMode, Watcher};
        let (tx, rx) = channel::<PathBuf>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if ev.kind.is_modify() || ev.kind.is_create() || ev.kind.is_remove() {
                    for p in ev.paths {
                        if p.extension().map_or(false, |e| e == "ox") {
                            let _ = tx.send(p);
                        }
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
        watcher.watch(&self.scripts_dir, RecursiveMode::Recursive).map_err(|e| e.to_string())?;
        let (results_tx, results_rx) = channel();
        self.watch = Some(Watch {
            _watcher: watcher,
            events: rx,
            pending: Mutex::new((HashSet::new(), None)),
            results_tx,
            results_rx: Mutex::new(results_rx),
            building: Arc::new(AtomicBool::new(false)),
        });
        Ok(())
    }

    /// Non-blocking. Collects file events, (debounced 150 ms) starts a
    /// background rebuild, and returns reports of finished rebuilds. New builds
    /// swap into live behaviors automatically.
    pub fn poll(&self) -> Vec<ReloadReport> {
        let Some(w) = &self.watch else { return Vec::new() };
        let mut pending = w.pending.lock().unwrap_or_else(|e| e.into_inner());
        while let Ok(p) = w.events.try_recv() {
            pending.0.insert(p);
            pending.1 = Some(Instant::now());
        }
        let quiet = pending.1.map_or(false, |t| t.elapsed() >= Duration::from_millis(150));
        if quiet && !pending.0.is_empty() && !w.building.swap(true, Ordering::AcqRel) {
            let changed: Vec<PathBuf> = pending.0.drain().collect();
            pending.1 = None;
            // engine_api.ox or a shared import changed → rebuild everything; else just those files.
            let all = self.script_files();
            let targets: Vec<PathBuf> = if changed.iter().all(|c| all.iter().any(|a| a.file_name() == c.file_name())) {
                changed.into_iter().filter(|c| c.exists()).collect()
            } else {
                all
            };
            let (tc, shared, out, tx, flag) =
                (self.toolchain.clone(), self.shared.clone(), self.out_dir.clone(), w.results_tx.clone(), w.building.clone());
            std::thread::spawn(move || {
                for t in targets {
                    let _ = tx.send(rebuild(&tc, &shared, &t, &out));
                }
                flag.store(false, Ordering::Release);
            });
        }
        let rx = w.results_rx.lock().unwrap_or_else(|e| e.into_inner());
        rx.try_iter().collect()
    }
}
