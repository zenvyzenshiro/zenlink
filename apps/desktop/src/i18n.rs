//! UI languages as gettext `.po` files. English texts are the keys (`msgid`); a language file
//! gives their translations (`msgstr`). Built in: English and `id.po` (Bahasa Indonesia, from
//! `ZenshiroDev/resources/lang`). More languages, or fixes to a built-in one, are `<code>.po`
//! files in a `lang` folder next to ZenLink.exe or in `%LOCALAPPDATA%\ZenLink-Next\lang`;
//! ZenLink lists them in Settings without a rebuild. `ZenshiroDev/resources/lang/README.md`
//! explains how to translate.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, RwLock};

const BUILT_IN: &[(&str, &str)] = &[(
    "id",
    include_str!("../../../ZenshiroDev/resources/lang/id.po"),
    include_str!("../../../ZenshiroDev/resources/lang/zh_CN.po"),
)];

/// The chosen language's texts (empty for English).
static CURRENT: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

/// `text` in the chosen language; `text` itself when it has no translation.
pub fn tr(text: &str) -> String {
    CURRENT
        .read()
        .ok()
        .and_then(|m| m.as_ref()?.get(text).cloned())
        .unwrap_or_else(|| text.to_string())
}

/// `tr` with `{name}` placeholders filled in: `trf!("Opening {app}…", app = name)`.
#[macro_export]
macro_rules! trf {
    ($text:expr $(, $k:ident = $v:expr)* $(,)?) => {{
        #[allow(unused_mut)]
        let mut s = $crate::i18n::tr($text);
        $( s = s.replace(concat!("{", stringify!($k), "}"), &($v).to_string()); )*
        s
    }};
}

/// The code `CURRENT` was loaded for.
static CODE: Mutex<String> = Mutex::new(String::new());

/// Switches the language ("en" or a code with a `.po` file). Files are read
/// once per switch, not on every call with the same code.
pub fn set(code: &str) {
    let Ok(mut current) = CODE.lock() else { return };
    if *current == code {
        return;
    }
    *current = code.to_string();
    let map = (code != "en").then(|| load(code)).flatten();
    if let Ok(mut c) = CURRENT.write() {
        *c = map;
    }
}

/// Languages to pick from: code and the language's own name, English first.
/// Found once per run (a new file shows after a restart).
pub fn languages() -> Vec<(String, String)> {
    static FOUND: OnceLock<Vec<(String, String)>> = OnceLock::new();
    FOUND.get_or_init(find).clone()
}

fn find() -> Vec<(String, String)> {
    let mut out = vec![("en".to_string(), "English".to_string())];
    let mut add = |code: &str, text: &str| {
        if code != "en" && !out.iter().any(|(c, _)| c == code) {
            let name = parse(text).1.unwrap_or_else(|| code.to_string());
            out.push((code.to_string(), name));
        }
    };
    for (code, text) in BUILT_IN {
        add(code, text);
    }
    for dir in dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        files.sort();
        for p in files {
            if p.extension().and_then(|e| e.to_str()) != Some("po") {
                continue;
            }
            let (Some(code), Ok(text)) = (
                p.file_stem().and_then(|s| s.to_str()),
                std::fs::read_to_string(&p),
            ) else {
                continue;
            };
            add(&code.to_lowercase(), &text);
        }
    }
    // Indonesia first, then English, then the rest by name.
    let rank = |c: &str| match c {
        "id" => 0,
        "en" => 1,
        _ => 2,
    };
    out.sort_by(|a, b| (rank(&a.0), &a.1).cmp(&(rank(&b.0), &b.1)));
    out
}

/// Built-in texts, then each folder's file on top (a fix in a file wins).
fn load(code: &str) -> Option<HashMap<String, String>> {
    let mut map = HashMap::new();
    let mut found = false;
    if let Some((_, text)) = BUILT_IN.iter().find(|(c, _)| *c == code) {
        map.extend(parse(text).0);
        found = true;
    }
    for dir in dirs() {
        if let Ok(text) = std::fs::read_to_string(dir.join(format!("{code}.po"))) {
            map.extend(parse(&text).0);
            found = true;
        }
    }
    found.then_some(map)
}

fn dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("lang")))
    {
        v.push(d);
    }
    if let Some(d) = zenlink_core::local_data_dir() {
        v.push(d.join("lang"));
    }
    v
}

/// msgid -> msgstr (untranslated and fuzzy entries left out) and the header's
/// `Language-Name`.
fn parse(text: &str) -> (HashMap<String, String>, Option<String>) {
    let mut map = HashMap::new();
    let (mut id, mut s, mut fuzzy) = (String::new(), String::new(), false);
    let mut field: Option<bool> = None; // Some(true): msgid, Some(false): msgstr
    let mut name = None;
    let mut flush = |id: &mut String, s: &mut String, fuzzy: &mut bool| {
        if id.is_empty() {
            name = s
                .lines()
                .find_map(|l| l.strip_prefix("Language-Name:"))
                .map(|n| n.trim().to_string())
                .or(name.take());
        } else if !s.is_empty() && !*fuzzy {
            map.insert(std::mem::take(id), std::mem::take(s));
        }
        id.clear();
        s.clear();
        *fuzzy = false;
    };
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') {
            // Comments belong to the next entry: finish the one before first.
            if field.take().is_some() {
                flush(&mut id, &mut s, &mut fuzzy);
            }
            fuzzy |= line.starts_with("#,") && line.contains("fuzzy");
        } else if let Some(rest) = line.strip_prefix("msgid ") {
            if field.is_some() {
                flush(&mut id, &mut s, &mut fuzzy);
            }
            id = unquote(rest);
            field = Some(true);
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            s = unquote(rest);
            field = Some(false);
        } else if line.starts_with('"') {
            match field {
                Some(true) => id.push_str(&unquote(line)),
                Some(false) => s.push_str(&unquote(line)),
                None => {}
            }
        }
    }
    if field.is_some() {
        flush(&mut id, &mut s, &mut fuzzy);
    }
    (map, name)
}

fn unquote(q: &str) -> String {
    let q = q.trim();
    let inner = q
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(q);
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn po_entries_and_name() {
        let po = "msgid \"\"\nmsgstr \"\"\n\"Language-Name: Bahasa Indonesia\\n\"\n\n\
                  msgid \"Start\"\nmsgstr \"Mulai\"\n\n\
                  #, fuzzy\nmsgid \"Stop\"\nmsgstr \"Henti\"\n\n\
                  msgid \"Two \"\n\"lines\"\nmsgstr \"Dua \"\n\"baris \\\"ok\\\"\"\n\n\
                  msgid \"Empty\"\nmsgstr \"\"\n";
        let (m, name) = parse(po);
        assert_eq!(name.as_deref(), Some("Bahasa Indonesia"));
        assert_eq!(m.get("Start").map(String::as_str), Some("Mulai"));
        assert_eq!(
            m.get("Two lines").map(String::as_str),
            Some("Dua baris \"ok\"")
        );
        assert!(!m.contains_key("Stop") && !m.contains_key("Empty"));
    }

    #[test]
    fn built_in_indonesian_loads() {
        let (m, name) = parse(BUILT_IN[0].1);
        assert_eq!(name.as_deref(), Some("Indonesia"));
        assert!(m.len() > 400);
    }

    #[test]
    fn translations_keep_placeholders() {
        let names = |t: &str| {
            let mut v: Vec<String> = t
                .split('{')
                .skip(1)
                .filter_map(|p| p.split_once('}').map(|(n, _)| n.to_string()))
                .collect();
            v.sort();
            v
        };
        for (en, id) in parse(BUILT_IN[0].1).0 {
            assert_eq!(names(&en), names(&id), "{en:?}");
        }
    }

    #[test]
    fn placeholders_fill_in() {
        set("en");
        assert_eq!(trf!("Opening {app}…", app = "Notes"), "Opening Notes…");
    }
}
