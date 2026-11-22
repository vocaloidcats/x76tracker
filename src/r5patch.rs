//! The optional R5Reloaded patch behind opponent tracking.
//!
//! R5Reloaded mods can't replace game scripts, so x76tracker adds a few lines to the game's own
//! `platform/scripts/vscripts/client/cl_obituary.gnut`: one call at the top of `Obituary()` and a
//! helper that prints a `[x76tracker]` line to `script.log` for each kill or death of the local
//! player (who, by whom, weapon). Nothing else in the game changes. Before patching, the file is
//! copied to `cl_obituary.gnut.bak-x76tracker`; switching it off takes exactly those lines out.

use std::path::{Path, PathBuf};

const SCRIPT: [&str; 5] = ["platform", "scripts", "vscripts", "client", "cl_obituary.gnut"];
const BACKUP_SUFFIX: &str = ".bak-x76tracker";
const HELPER_START: &str = "// ---- x76tracker (unofficial) ---";

/// The patch as it is on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The game's script is patched.
    On,
    /// The script is the game's own.
    Off,
    /// The game folder has no cl_obituary.gnut (a very different game version).
    NoScript,
}

pub fn script_path(game_dir: &Path) -> PathBuf {
    SCRIPT.iter().fold(game_dir.to_path_buf(), |p, part| p.join(part))
}

fn backup_path(script: &Path) -> PathBuf {
    let mut name = script.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(BACKUP_SUFFIX);
    script.with_file_name(name)
}

pub fn is_on(text: &str) -> bool {
    text.contains("void function x76tracker_LogObituary(") && text.contains("\tx76tracker_LogObituary( attacker")
}

// ------------------------------------------------------------------ the three insertions
fn init_line(eol: &str) -> String {
    format!("\tprintt( \"[x76tracker]\\tloaded\" ) // x76tracker{eol}")
}

fn call_block(eol: &str) -> String {
    format!(
        "\t// x76tracker: note fights you were in, for the app to read from script.log{eol}\
         \tx76tracker_LogObituary( attacker, victim, damageSourceId, obitFlags ){eol}{eol}"
    )
}

fn helper(eol: &str) -> String {
    [
        "// ---- x76tracker (unofficial) -----------------------------------------------------------",
        "// Writes one line to script.log for each kill or death of the local player. Changes nothing else.",
        "// Original file: cl_obituary.gnut.bak-x76tracker",
        "void function x76tracker_LogObituary( entity attacker, entity victim, int damageSourceId, int obitFlags )",
        "{",
        "\tentity me = GetLocalClientPlayer()",
        "\tif ( !IsValid( me ) || !IsValid( victim ) || !victim.IsPlayer() )",
        "\t\treturn",
        "",
        "\tbool iDied = victim == me",
        "\tbool iKilled = IsValid( attacker ) && attacker == me",
        "\tif ( !iDied && !iKilled )",
        "\t\treturn",
        "",
        "\tstring attackerName = ( IsValid( attacker ) && attacker.IsPlayer() ) ? attacker.GetPlayerName() : \"\"",
        "\tstring kind = iDied ? \"death\" : \"kill\"",
        "\tstring state = ( obitFlags & OBIT_FLAG_DOWNED ) > 0 ? \"downed\" : \"dead\"",
        "\tprintt( \"[x76tracker]\\t\" + kind + \"\\t\" + attackerName + \"\\t\" + victim.GetPlayerName() + \"\\t\" + damageSourceId + \"\\t\" + state )",
        "}",
    ]
    .iter()
    .map(|l| format!("{l}{eol}"))
    .collect()
}

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// The patched script. Fails if the game's script doesn't look like the one the patch was made for.
pub fn apply(text: &str) -> Result<String, String> {
    if is_on(text) {
        return Ok(text.to_string());
    }
    let changed = "This R5Reloaded version's cl_obituary.gnut looks different, so x76tracker can't patch it safely.";
    let eol = eol_of(text);
    let init_anchor = format!("void function ClObituary_Init(){eol}{{{eol}");
    let at_init = text.find(&init_anchor).ok_or(changed)? + init_anchor.len();
    let sig = text.find("void function Obituary( entity attacker,").ok_or(changed)?;
    let brace = format!("{eol}{{{eol}");
    let at_call = text[sig..].find(&brace).filter(|&i| i < 400).ok_or(changed)? + sig + brace.len();
    if at_call <= at_init {
        return Err(changed.into());
    }
    let mut out = String::with_capacity(text.len() + 1500);
    out += &text[..at_init];
    out += &init_line(eol);
    out += &text[at_init..at_call];
    out += &call_block(eol);
    out += text[at_call..].trim_end_matches(['\r', '\n']);
    out += eol;
    out += eol;
    out += &helper(eol);
    Ok(out)
}

/// The script with the patch taken out again (the game's own text, as it was).
pub fn strip(text: &str) -> String {
    let mut out = text.to_string();
    for eol in ["\r\n", "\n"] {
        out = out.replacen(&init_line(eol), "", 1).replacen(&call_block(eol), "", 1);
    }
    if let Some(i) = out.find(HELPER_START) {
        out.truncate(i);
        let trimmed = out.trim_end_matches(['\r', '\n']).len();
        out.truncate(trimmed); // the game's file ends right after its last "}"
    }
    out
}

// ------------------------------------------------------------------ on disk
pub fn state(game_dir: &Path) -> State {
    match std::fs::read_to_string(script_path(game_dir)) {
        Ok(text) if is_on(&text) => State::On,
        Ok(_) => State::Off,
        Err(_) => State::NoScript,
    }
}

/// Patches the game's script (keeping a copy of the original). Does nothing if it's already on.
pub fn install(game_dir: &Path) -> Result<(), String> {
    let path = script_path(game_dir);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    if is_on(&text) {
        return Ok(());
    }
    let patched = apply(&text)?;
    std::fs::write(backup_path(&path), &text).map_err(|e| format!("Couldn't save a backup of the game's script: {e}"))?;
    write(&path, &patched)
}

/// Takes the patch out of the game's script.
pub fn remove(game_dir: &Path) -> Result<(), String> {
    let path = script_path(game_dir);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    if !text.contains("x76tracker") {
        return Ok(());
    }
    let clean = strip(&text);
    if clean.contains("x76tracker") {
        return Err(format!(
            "Couldn't take the patch out cleanly. The original is saved as {}; copy it back over cl_obituary.gnut, or let the R5Reloaded launcher repair the game.",
            backup_path(&path).display()
        ));
    }
    write(&path, &clean)
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("gnut.x76tracker-tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("Couldn't write the game's script: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Couldn't replace the game's script: {e}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAME: &str = "global function ClObituary_Init\r\n\r\nvoid function ClObituary_Init()\r\n{\r\n}\r\n\r\n\
        void function Obituary( entity attacker, string attackerClass, entity victim, int scriptDamageType, int damageSourceId, int obitFlags, bool victimIsOwnedTitan = false, bool forceDisplay = false )\r\n{\r\n\
        \tif ( GetConVarInt( \"hud_setting_showObituary\" ) == 0 )\r\n\t\treturn\r\n}";

    #[test]
    fn apply_then_strip_gives_back_the_original() {
        let on = apply(GAME).unwrap();
        assert!(is_on(&on) && !is_on(GAME));
        assert!(on.contains("{\r\n\tprintt( \"[x76tracker]\\tloaded\" ) // x76tracker\r\n}"));
        assert!(on.contains("{\r\n\t// x76tracker: note fights"));
        assert!(!on.replace("\r\n", "").contains('\n'), "line endings stay CRLF");
        assert_eq!(apply(&on).unwrap(), on, "applying twice changes nothing");
        assert_eq!(strip(&on), GAME);
    }

    #[test]
    fn refuses_an_unknown_script() {
        assert!(apply("void function Something()\r\n{\r\n}").is_err());
    }

    /// Checks against the real game files on this PC, if R5Reloaded is installed where it is here.
    /// `cargo test --release r5patch -- --ignored`
    #[test]
    #[ignore]
    fn matches_the_patch_on_this_pc() {
        let dir = Path::new(r"L:\R5R Library\LIVE");
        let path = script_path(dir);
        let (Ok(now), Ok(original)) = (std::fs::read_to_string(&path), std::fs::read_to_string(backup_path(&path))) else {
            return;
        };
        if is_on(&now) {
            assert_eq!(apply(&original).unwrap(), now);
            assert_eq!(strip(&now), original);
        }
    }
}
