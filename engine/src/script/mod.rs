//! Scripting. The only script language is Oxidized (`.ox`); see [`ox`] and
//! `docs/SCRIPTING-OXIDIZED.md`. (The old interpreted `.pss` language was
//! retired — `tools/oxc_scripts convert` migrates leftover `.pss` files.)

pub mod ox;
