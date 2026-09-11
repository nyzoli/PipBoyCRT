//! A `config.toml` beolvasása: a héj a `theme`-et kapja, a modulok a nyers táblát.
//!
//! A tipizált szekció-structok (`WeatherCfg`, `RadioCfg`, …) a saját modulfájljukban
//! élnek; itt csak a fájl beolvasása és a felső szintű `theme` marad.
use crate::module::ModuleConfig;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeKind {
    #[default]
    Color,
    Mono,
}

#[derive(Debug, Clone, Default)]
pub struct RawConfig {
    pub theme: ThemeKind,
    pub modules: ModuleConfig,
}

fn first_line(e: impl ToString) -> String {
    e.to_string().lines().next().unwrap_or("").to_string()
}

impl RawConfig {
    /// Config + egysoros üzenet a láblécbe, ha alapértékekkel indulunk.
    pub fn load(path: &Path) -> (RawConfig, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return (RawConfig::default(), Some(format!("{} not found, defaults", path.display()))),
        };
        let table = match text.parse::<toml::Table>() {
            Ok(t) => t,
            Err(e) => return (RawConfig::default(), Some(format!("config error: {}", first_line(e)))),
        };
        let (theme, notice) = match table.get("theme") {
            None => (ThemeKind::default(), None),
            Some(v) => match v.clone().try_into::<ThemeKind>() {
                Ok(t) => (t, None),
                Err(e) => (ThemeKind::default(), Some(format!("config theme: {}", first_line(e)))),
            },
        };
        (RawConfig { theme, modules: ModuleConfig(table) }, notice)
    }

    /// A `[shell] disabled = [...]` sor átírása: a fájl minden más bájtja marad.
    ///
    /// Szándékosan szöveges szerkesztés (nincs `toml_edit`): a felhasználó
    /// kommentjei, sorrendje és formázása így sértetlen marad.
    ///
    /// Hiányzó fájlnál üres szövegről indulunk; minden más olvasási hibát
    /// (pl. nem UTF-8 tartalom egy ANSI/UTF-16-ban mentett configból)
    /// továbbadunk, hogy sose írjuk felül egy üres alapértékkel. Az írás
    /// atomikus: `<path>.tmp`-be írunk, majd átnevezzük az eredetire.
    pub fn save_disabled(path: &Path, ids: &[&str]) -> std::io::Result<()> {
        let list = ids.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let out = splice_disabled(&text, &format!("disabled = [{list}]"))?;
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, path)
    }

    /// `config.toml` az exe mellett; ha az exe útja nem kérdezhető le, a munkakönyvtárban.
    pub fn path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("config.toml")))
            .unwrap_or_else(|| PathBuf::from("config.toml"))
    }
}

/// Ha `l` egy tábla-fejléc sor (a sor végi `# ...` komment leszámítva), a
/// zárójelek közötti — belső szóközöktől megfosztott — nevet adja vissza,
/// `true`-val jelezve, ha tömb-tábla (`[[name]]`). Minden más sorra `None`.
fn header_name(l: &str) -> Option<(&str, bool)> {
    let l = l.trim().split('#').next().unwrap_or("").trim();
    if let Some(inner) = l.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
        return Some((inner.trim(), true));
    }
    l.strip_prefix('[').and_then(|s| s.strip_suffix(']')).map(|inner| (inner.trim(), false))
}

/// A `[shell]` szekció `disabled` sorának cseréje/beszúrása, a többi szekció érintetlenül.
///
/// A fejléc-egyeztetés a `#` komment és a zárójel-belüli szóközök felett néz
/// (`[shell]  # ...`, `[ shell ]`); `[shell.sub]` nem `[shell]`, és `[[shell]]`
/// tömb-táblát hibával jelezzük, mert szöveges cserével nem kezelhető.
fn splice_disabled(text: &str, line: &str) -> std::io::Result<String> {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<&str> = text.lines().collect();
    let mut head = None;
    for (i, l) in lines.iter().enumerate() {
        match header_name(l) {
            Some(("shell", true)) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "config: [[shell]] array table not supported",
                ))
            }
            Some(("shell", false)) => {
                head = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(head) = head else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(nl);
        }
        out.push_str(&format!("{nl}[shell]{nl}{line}{nl}"));
        return Ok(out);
    };
    // A szekció teste a következő tábla-fejlécig tart.
    let end = lines[head + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |i| head + 1 + i);
    let key = |l: &str| l.trim_start().strip_prefix("disabled").is_some_and(|r| r.trim_start().starts_with('='));
    match lines[head + 1..end].iter().position(|l| key(l)) {
        Some(i) => {
            // Többsoros tömb esetén a záró `]`-ig tart a régi érték.
            let from = head + 1 + i;
            let mut to = from;
            while to + 1 < end && !lines[to].contains(']') {
                to += 1;
            }
            lines.splice(from..=to, [line]);
        }
        None => lines.insert(head + 1, line),
    }
    let mut out = lines.join(nl);
    out.push_str(nl);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults_and_notice() {
        let (cfg, notice) = RawConfig::load(Path::new("nincs-ilyen/config.toml"));
        assert_eq!(cfg.theme, ThemeKind::Color);
        assert!(cfg.modules.0.is_empty());
        assert!(notice.unwrap().contains("not found"));
    }

    #[test]
    fn parses_theme_and_keeps_sections_for_modules() {
        let dir = std::env::temp_dir().join("pipboy-test-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "theme = \"mono\"\n[weather]\nname = \"X\"\n").unwrap();
        let (cfg, notice) = RawConfig::load(&p);
        assert_eq!(cfg.theme, ThemeKind::Mono);
        assert!(notice.is_none());
        #[derive(Deserialize, Default, PartialEq, Debug)]
        #[serde(default)]
        struct W {
            name: String,
        }
        let (w, n) = cfg.modules.section::<W>("weather");
        assert_eq!(w.name, "X");
        assert!(n.is_none());
    }

    #[test]
    fn bad_theme_value_gives_notice_but_starts() {
        let dir = std::env::temp_dir().join("pipboy-test-theme");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "theme = \"amber\"\n").unwrap();
        let (cfg, notice) = RawConfig::load(&p);
        assert_eq!(cfg.theme, ThemeKind::Color);
        assert!(notice.unwrap().starts_with("config theme"));
    }

    #[test]
    fn bad_toml_gives_defaults_and_error_notice() {
        let dir = std::env::temp_dir().join("pipboy-test-bad");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "theme = [").unwrap();
        let (cfg, notice) = RawConfig::load(&p);
        assert_eq!(cfg.theme, ThemeKind::Color);
        assert!(notice.unwrap().starts_with("config error"));
    }
    #[test]
    fn save_disabled_appends_shell_section_when_missing() {
        let text = "theme = \"mono\"\n[weather]\nname = \"X\"\n";
        let out = splice_disabled(text, "disabled = [\"mail\"]").unwrap();
        assert!(out.starts_with(text), "a meglévő tartalom bájtra ugyanaz: {out:?}");
        assert!(out.ends_with("\n[shell]\ndisabled = [\"mail\"]\n"), "{out:?}");
    }

    #[test]
    fn save_disabled_replaces_the_existing_line_and_keeps_other_sections() {
        let text = "# fejléc\ntheme = \"mono\"\n\n[shell]\n# melyik modul alszik\ndisabled = [\"mail\", \"term\"]\n\n[weather]\nname = \"X\"  # komment\n";
        let out = splice_disabled(text, "disabled = []").unwrap();
        assert!(out.contains("# melyik modul alszik\ndisabled = []\n"), "{out:?}");
        assert!(!out.contains("\"mail\""), "a régi érték eltűnt: {out:?}");
        assert!(out.contains("[weather]\nname = \"X\"  # komment\n"), "a többi szekció érintetlen: {out:?}");
        assert!(out.starts_with("# fejléc\ntheme = \"mono\"\n\n[shell]\n"), "{out:?}");
    }

    #[test]
    fn save_disabled_handles_multiline_arrays_and_crlf() {
        let text = "[shell]\r\ndisabled = [\r\n  \"mail\",\r\n]\r\n[weather]\r\nname = \"X\"\r\n";
        let out = splice_disabled(text, "disabled = [\"term\"]").unwrap();
        let want = "[shell]\r\ndisabled = [\"term\"]\r\n[weather]\r\nname = \"X\"\r\n";
        assert_eq!(out, want, "a CRLF és a többi szekció marad");
    }

    #[test]
    fn save_disabled_matches_commented_and_spaced_header_and_replaces_it() {
        let text = "theme = \"mono\"\n\n[shell]  # kedvenc szekció\ndisabled = [\"mail\"]\n\n[weather]\nname = \"X\"\n";
        let out = splice_disabled(text, "disabled = []").unwrap();
        assert!(out.contains("[shell]  # kedvenc szekció\ndisabled = []\n"), "{out:?}");
        assert!(!out.contains("\"mail\""), "{out:?}");

        let text2 = "[ shell ]\ndisabled = [\"mail\"]\n[weather]\nname = \"X\"\n";
        let out2 = splice_disabled(text2, "disabled = []").unwrap();
        assert!(out2.contains("[ shell ]\ndisabled = []\n"), "{out2:?}");
        assert!(!out2.contains("\"mail\""), "beszúrás helyett csere történt: {out2:?}");
    }

    #[test]
    fn save_disabled_ignores_shell_sub_table_and_rejects_array_table() {
        let text = "[shell.sub]\nx = 1\n";
        let out = splice_disabled(text, "disabled = [\"mail\"]").unwrap();
        assert!(out.contains("[shell.sub]\nx = 1\n"), "a [shell.sub] érintetlen: {out:?}");
        assert!(out.ends_with("[shell]\ndisabled = [\"mail\"]\n"), "új [shell] hozzáadva: {out:?}");

        let err = splice_disabled("[[shell]]\ndisabled = [\"mail\"]\n", "disabled = []").unwrap_err();
        assert!(err.to_string().contains("[[shell]] array table not supported"), "{err}");
    }

    #[test]
    fn save_disabled_roundtrips_through_the_file() {
        let dir = std::env::temp_dir().join("pipboy-test-save");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "theme = \"mono\"\n").unwrap();
        RawConfig::save_disabled(&p, &["mail", "term"]).unwrap();
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct S {
            disabled: Vec<String>,
        }
        let (cfg, notice) = RawConfig::load(&p);
        assert!(notice.is_none());
        assert_eq!(cfg.theme, ThemeKind::Mono, "a többi beállítás túléli");
        let (s, _) = cfg.modules.section::<S>("shell");
        assert_eq!(s.disabled, vec!["mail".to_string(), "term".to_string()]);
    }

    #[test]
    fn save_disabled_rejects_invalid_utf8_without_touching_the_file() {
        let dir = std::env::temp_dir().join("pipboy-test-badutf8");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        // ANSI/UTF-16-szerű, nem UTF-8 bájtsorozat: read_to_string InvalidData-t ad.
        let original: &[u8] = &[0x74, 0x68, 0x65, 0x6d, 0x65, 0xff, 0xfe, 0x00];
        std::fs::write(&p, original).unwrap();
        let err = RawConfig::save_disabled(&p, &["mail"]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&p).unwrap(), original, "a fájl bájtra ugyanaz marad");
        assert!(!p.with_extension("toml.tmp").exists());
    }
}
