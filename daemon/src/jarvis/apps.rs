//! Desktop applications for "open ...": the XDG `.desktop` entries the user
//! sees in their launcher, matched by name, generic name, keywords or id.

use super::intent::{name_score, squash};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    /// Desktop file id (`org.gnome.Nautilus.desktop`; subdirectories joined by `-`).
    pub id: String,
    pub path: PathBuf,
    pub name: String,
    pub generic: Option<String>,
    pub keywords: Vec<String>,
}

/// `$XDG_DATA_HOME/applications`, then each of `$XDG_DATA_DIRS`.
pub fn dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let data = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(
            data.split(':')
                .map(PathBuf::from)
                .filter(|p| p.is_absolute()),
        )
        .map(|d| d.join("applications"))
        .collect()
}

/// Every launchable entry in `dirs` (an id found earlier hides later ones,
/// so user entries override system ones).
pub fn scan(dirs: &[PathBuf]) -> Vec<App> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in dirs {
        let mut files = Vec::new();
        walk(dir, dir, 0, &mut files);
        files.sort();
        for (id, path) in files {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(app) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| parse(&id, &path, &text))
            {
                apps.push(app);
            }
        }
    }
    apps
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_dir() && depth < 3 {
            walk(root, &path, depth + 1, out);
        } else if path.extension().is_some_and(|x| x == "desktop") {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push((rel.to_string_lossy().replace('/', "-"), path));
            }
        }
    }
}

/// The `[Desktop Entry]` of an application that is shown in launchers.
fn parse(id: &str, path: &Path, text: &str) -> Option<App> {
    let mut in_entry = false;
    let (mut name, mut generic, mut keywords) = (None, None, Vec::new());
    let (mut app, mut exec) = (false, false);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Type" => app = value == "Application",
            "Name" => name = Some(value.to_string()),
            "GenericName" => generic = Some(value.to_string()),
            "Keywords" => {
                keywords = value
                    .split(';')
                    .map(str::trim)
                    .filter(|k| !k.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            "Exec" => exec = !value.is_empty(),
            "NoDisplay" | "Hidden" if value == "true" => return None,
            _ => {}
        }
    }
    (app && exec).then_some(App {
        id: id.to_string(),
        path: path.to_path_buf(),
        name: name?,
        generic,
        keywords,
    })
}

/// The app `heard` names best: its name first, then its id, generic name
/// and keywords.
pub fn pick<'a>(apps: &'a [App], heard: &str) -> Option<&'a App> {
    let h = squash(heard);
    if h.is_empty() {
        return None;
    }
    let score = |a: &App| -> u32 {
        let name = name_score(heard, &a.name);
        if name > 0 {
            return 10 + name;
        }
        let n = squash(&a.name);
        // "code" for "Visual Studio Code", "studio code".
        if n.ends_with(&h) || (h.len() >= 4 && n.starts_with(&h)) {
            return 9;
        }
        let stem = a.id.trim_end_matches(".desktop");
        let last = stem.rsplit(['.', '-']).next().unwrap_or(stem);
        if name_score(heard, last) > 0 || name_score(heard, stem) > 0 {
            return 8;
        }
        if a.generic
            .as_deref()
            .is_some_and(|g| name_score(heard, g) > 0)
        {
            return 6;
        }
        if a.keywords.iter().any(|k| name_score(heard, k) > 0) {
            return 4;
        }
        0
    };
    let mut best: Option<(u32, &App)> = None;
    for a in apps {
        let s = score(a);
        if s > 0 && best.is_none_or(|(b, _)| s > b) {
            best = Some((s, a));
        }
    }
    best.map(|(_, a)| a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hako-apps-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(dir: &Path, file: &str, body: &str) {
        let p = dir.join(file);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            p,
            format!("[Desktop Entry]\nType=Application\nExec=x\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn scan_skips_hidden_entries_and_user_entries_override() {
        let user = dir("user");
        let system = dir("system");
        entry(&user, "firefox.desktop", "Name=Firefox Nightly\n");
        entry(&system, "firefox.desktop", "Name=Firefox\n");
        entry(
            &system,
            "org.gnome.Nautilus.desktop",
            "Name=Files\nGenericName=File Manager\nKeywords=folder;explorer;\n",
        );
        entry(&system, "kde/dolphin.desktop", "Name=Dolphin\n");
        entry(&system, "hidden.desktop", "Name=Hidden\nNoDisplay=true\n");
        entry(&system, "gone.desktop", "Name=Gone\nHidden=true\n");
        std::fs::write(
            system.join("link.desktop"),
            "[Desktop Entry]\nType=Link\nName=Link\nURL=x\n",
        )
        .unwrap();
        std::fs::write(
            system.join("other.desktop"),
            "[Desktop Action new]\nName=Ghost\nExec=y\n[Desktop Entry]\nType=Application\nExec=z\nName=Other\n",
        )
        .unwrap();
        let apps = scan(&[user.clone(), system.clone()]);
        let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Firefox Nightly", "Dolphin", "Files", "Other"]);
        assert_eq!(apps[1].id, "kde-dolphin.desktop");
        assert_eq!(apps[2].keywords, ["folder", "explorer"]);

        assert_eq!(pick(&apps, "firefox").unwrap().id, "firefox.desktop");
        assert_eq!(pick(&apps, "files").unwrap().name, "Files");
        assert_eq!(pick(&apps, "nautilus").unwrap().name, "Files");
        assert_eq!(pick(&apps, "file manager").unwrap().name, "Files");
        assert_eq!(pick(&apps, "explorer").unwrap().name, "Files");
        assert_eq!(pick(&apps, "dolphn").unwrap().name, "Dolphin");
        assert!(pick(&apps, "steam").is_none());
        let _ = std::fs::remove_dir_all(user);
        let _ = std::fs::remove_dir_all(system);
    }

    #[test]
    fn the_name_beats_another_apps_keyword() {
        let app = |name: &str, kw: &[&str]| App {
            id: format!("{}.desktop", name.to_lowercase()),
            path: PathBuf::new(),
            name: name.into(),
            generic: None,
            keywords: kw.iter().map(|k| k.to_string()).collect(),
        };
        let apps = [
            app("Lutris", &["steam"]),
            app("Steam", &[]),
            app("Visual Studio Code", &[]),
        ];
        assert_eq!(pick(&apps, "steam").unwrap().name, "Steam");
        assert_eq!(pick(&apps, "code").unwrap().name, "Visual Studio Code");
    }
}
