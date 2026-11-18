# Changelog

## v0.1.0-dust
- Renamed Jame Engine → Dust Engine. Crate `engine` is now `dust_engine` (`use dust_engine::...`; Cargo: `dust_engine = { path = ".../engine" }`).
- Env var renamed: `JAME_TUNABLES` → `DUST_TUNABLES`.
- Editor config lives in `<assets>/.editor` and never contained "jame": no config migration needed.
- Scripting is Oxidized-only (`.pss` retired).
