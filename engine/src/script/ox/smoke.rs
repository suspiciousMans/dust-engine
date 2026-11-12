//! The Oxidized smoke-test driver: an `.ox` script exposing
//! `fn tick(frame: Int, t: Float)` drives a live game one frame at a time.
//!
//! The script is compiled with the `Smoke` profile (see `build::spec::SMOKE_SPEC`)
//! into a cdylib and loaded through the same loader as behavior scripts.
//! Its natives are `press(key)` / `release(key)` (queue synthetic SDL key
//! events), `assert_true(cond, msg)`, `assert_eq(a, b, msg)`,
//! `assert_eq_str(a, b, msg)`, `log(msg)`, `finish(ok)`, `time()` and
//! `get_var/set_var` (script functions are stateless, so phase counters live
//! host-side). `App::run_scripted` feeds the queued events through
//! `ctx.input.handle_event` + `game.handle_event` — the same path a real
//! keypress takes — and exits 0/1 from [`SmokeDriver::result`].

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use libloading::Library;
use sdl2::event::Event;
use sdl2::keyboard::{Keycode, Mod};

use super::build::compile::{build_script, BuildOptions, Toolchain};
use super::build::spec::{self, Profile};
use super::runtime::{HostFn, OxVal};

/// Everything the smoke natives read and write. Lives in a `Box` so its
/// address is stable while a `tick` is on the stack.
#[derive(Default)]
pub struct SmokeState {
    elapsed: f64,
    result: Option<bool>,
    events: Vec<Event>,
    vars: HashMap<String, f64>,
    /// Assertion failures recorded so far (also logged as they happen).
    pub failures: Vec<String>,
    /// Every `log(...)` line, so tests can assert on what the script said.
    pub log_lines: Vec<String>,
}

fn key_event(down: bool, keycode: Keycode) -> Event {
    if down {
        Event::KeyDown { timestamp: 0, window_id: 0, keycode: Some(keycode), scancode: None, keymod: Mod::empty(), repeat: false }
    } else {
        Event::KeyUp { timestamp: 0, window_id: 0, keycode: Some(keycode), scancode: None, keymod: Mod::empty(), repeat: false }
    }
}

impl SmokeState {
    fn fail(&mut self, what: &str, message: &str) {
        let line = format!("{what}: {message}");
        log::error!("[smoke test] {line}");
        self.failures.push(line);
        self.result = Some(false);
    }

    fn key(&mut self, op: &str, name: &str) {
        match Keycode::from_name(name) {
            Some(code) => self.events.push(key_event(op == "press", code)),
            None => self.fail(op, &format!("unknown key '{name}'")),
        }
    }

    fn call(&mut self, op: &str, a: &[OxVal]) -> OxVal {
        // SAFETY: string args point into the shim's frame for the duration of this call.
        let s = |i: usize| -> &str { a.get(i).map_or("", |v| unsafe { v.str() }) };
        let n = |i: usize| -> f64 { a.get(i).map_or(0.0, OxVal::num) };
        match op {
            "log" => {
                log::info!("[smoke test] {}", s(0));
                self.log_lines.push(s(0).to_string());
            }
            "time" => return OxVal::float(self.elapsed),
            "press" | "release" => {
                let name = s(0).to_string();
                self.key(op, &name);
            }
            "check_fail" => self.fail("assert_true failed", s(0)),
            "assert_eq" => {
                if n(0) != n(1) {
                    self.fail(&format!("assert_eq failed ({} != {})", n(0), n(1)), s(2));
                }
            }
            "assert_eq_str" => {
                if s(0) != s(1) {
                    self.fail(&format!("assert_eq failed ({:?} != {:?})", s(0), s(1)), s(2));
                }
            }
            // An assertion failure earlier this run always wins, even if the
            // script goes on to call `finish(true)` regardless.
            "finish_with" => {
                if self.result != Some(false) {
                    self.result = Some(n(0) != 0.0);
                }
            }
            "get_var" => return OxVal::float(self.vars.get(s(0)).copied().unwrap_or(n(1))),
            "set_var" => {
                let name = s(0).to_string();
                self.vars.insert(name, n(1));
            }
            other => log::error!("smoke script called unknown host op '{other}'"),
        }
        OxVal::NIL
    }
}

thread_local! {
    static STATE: Cell<*mut SmokeState> = const { Cell::new(std::ptr::null_mut()) };
}

fn host_ops() -> &'static Vec<String> {
    static OPS: OnceLock<Vec<String>> = OnceLock::new();
    OPS.get_or_init(|| spec::natives_for(Profile::Smoke).into_iter().filter(|n| n.inline_body.is_none()).map(|n| n.name).collect())
}

extern "C" fn smoke_trampoline(op: u32, args: *const OxVal, nargs: usize, ret: *mut OxVal) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        *ret = OxVal::NIL;
        let p = STATE.with(|c| c.get());
        if p.is_null() {
            return;
        }
        let args = std::slice::from_raw_parts(args, nargs);
        *ret = (*p).call(host_ops().get(op as usize).map(String::as_str).unwrap_or(""), args);
    }));
}

type TickFn = unsafe extern "C" fn(i64, f64) -> i32;

/// A loaded smoke script plus the state its natives act on.
pub struct SmokeDriver {
    pub name: String,
    tick: TickFn,
    last_error: unsafe extern "C" fn(*mut u8, usize) -> usize,
    // Never dlclose (see `LoadedScript`).
    _lib: std::mem::ManuallyDrop<Library>,
    state: Box<SmokeState>,
    frame: i64,
    /// How long building the script took (`Duration::ZERO` on a cache hit) and whether it was a hit.
    pub build_time: Duration,
    pub cached: bool,
}

impl SmokeDriver {
    /// Builds (or reuses the cached build of) `script` and loads it. The
    /// build directory is `<script dir>/.ox-build`.
    pub fn load(script: &Path) -> Result<Self, String> {
        let out_dir = script.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")).join(".ox-build");
        Self::load_with(&Toolchain::discover()?, script, &out_dir)
    }

    pub fn load_with(tc: &Toolchain, script: &Path, out_dir: &Path) -> Result<Self, String> {
        let started = std::time::Instant::now();
        let built = build_script(tc, script, out_dir, &BuildOptions { profile: Profile::Smoke, ..Default::default() })
            .map_err(|d| format!("smoke script failed to build:\n{d}"))?;
        // SAFETY: loading the library we just compiled from the user's script.
        unsafe {
            let lib = Library::new(&built.lib_path).map_err(|e| format!("cannot load {}: {e}", built.lib_path.display()))?;
            let hash: libloading::Symbol<unsafe extern "C" fn() -> u64> =
                lib.get(b"ox_api_hash\0").map_err(|e| format!("not an oxidized script library: {e}"))?;
            if hash() != spec::api_hash_for(Profile::Smoke) {
                return Err("smoke script was built against a different API version; delete .ox-build and retry".into());
            }
            let init: libloading::Symbol<unsafe extern "C" fn(HostFn)> = lib.get(b"ox_init\0").map_err(|e| e.to_string())?;
            init(smoke_trampoline);
            let tick = *lib.get::<TickFn>(b"ox_tick\0").map_err(|e| format!("script has no `tick`: {e}"))?;
            let last_error = *lib.get::<unsafe extern "C" fn(*mut u8, usize) -> usize>(b"ox_last_error\0").map_err(|e| e.to_string())?;
            Ok(Self {
                name: built.name,
                tick,
                last_error,
                _lib: std::mem::ManuallyDrop::new(lib),
                state: Box::default(),
                frame: 0,
                build_time: started.elapsed(),
                cached: built.cached,
            })
        }
    }

    /// Calls the script's `tick(frame, t)` once; `elapsed` is the game clock
    /// in seconds. A script panic comes back as `Err` (the run should fail).
    pub fn tick(&mut self, elapsed: f32) -> Result<(), String> {
        self.state.elapsed = elapsed as f64;
        let prev = STATE.with(|c| c.replace(&mut *self.state as *mut SmokeState));
        let code = unsafe { (self.tick)(self.frame, elapsed as f64) };
        STATE.with(|c| c.set(prev));
        self.frame += 1;
        if code == 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; 512];
        let n = unsafe { (self.last_error)(buf.as_mut_ptr(), buf.len()) };
        Err(format!("{}::tick: {}", self.name, String::from_utf8_lossy(&buf[..n])))
    }

    /// `Some(passed)` once the script called `finish(...)` or an assertion failed.
    pub fn result(&self) -> Option<bool> {
        self.state.result
    }

    /// Drains the synthetic key events `press`/`release` queued during `tick`.
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.state.events)
    }

    pub fn state(&self) -> &SmokeState {
        &self.state
    }

    /// Number of `tick` calls so far.
    pub fn frames(&self) -> i64 {
        self.frame
    }
}
