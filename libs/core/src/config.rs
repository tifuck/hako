//! XDG config / profiles / device TOML.

use crate::catalog::{normalize_key, SettingKey, SettingValue, SettingsMap};
use crate::ipc::AuxEffectChain;
use crate::remap::{RemapBinding, RemapFile};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Serialises every read-modify-write of `config.toml` in this process.
static APP_CONFIG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub version: u32,
    #[serde(default)]
    pub last_profile: Option<String>,
    #[serde(default)]
    pub log_level: String,
    #[serde(default = "default_true")]
    pub chatmix_set_default_sink: bool,
    #[serde(default)]
    pub aux: AuxConfig,
    /// Devices (`vid_pid` config stems) whose owner allowed settings that are
    /// saved to the device's onboard memory. Empty = none.
    #[serde(default)]
    pub onboard_writes: Vec<String>,
    /// UI color theme id; empty = the UI's default.
    #[serde(default)]
    pub theme: String,
    /// The UI looks for new GitHub releases.
    #[serde(default = "default_true")]
    pub check_updates: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            last_profile: None,
            log_level: "info".into(),
            chatmix_set_default_sink: true,
            aux: AuxConfig::default(),
            onboard_writes: Vec::new(),
            theme: String::new(),
            check_updates: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuxConfig {
    #[serde(default)]
    pub auto_enable: bool,
    #[serde(default = "default_capture_regex")]
    pub capture_name_regex: String,
    #[serde(default = "default_sink_regex")]
    pub sink_name_regex: String,
    #[serde(default = "default_capture_percent")]
    pub capture_percent: u8,
    #[serde(default)]
    pub set_default_sink: bool,
    #[serde(default)]
    pub effects: AuxEffectChain,
}

fn default_true() -> bool {
    true
}

fn default_capture_regex() -> String {
    r"alsa_input\.pci-.*\.analog-stereo".into()
}
fn default_sink_regex() -> String {
    r"alsa_output\.usb-SteelSeries_Arctis_Nova_Pro_Wireless.*analog-stereo".into()
}
fn default_capture_percent() -> u8 {
    66
}

impl Default for AuxConfig {
    fn default() -> Self {
        Self {
            auto_enable: false,
            capture_name_regex: default_capture_regex(),
            sink_name_regex: default_sink_regex(),
            capture_percent: default_capture_percent(),
            set_default_sink: false,
            effects: AuxEffectChain::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceConfigFile {
    pub version: u32,
    #[serde(default)]
    pub settings: BTreeMap<String, TomlSetting>,
}

/// TOML-friendly setting value (untagged).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TomlSetting {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl From<TomlSetting> for SettingValue {
    fn from(v: TomlSetting) -> Self {
        match v {
            TomlSetting::Bool(b) => SettingValue::Bool(b),
            TomlSetting::Int(i) => SettingValue::Int(i),
            TomlSetting::Float(f) => SettingValue::Float(f),
            TomlSetting::Text(s) => SettingValue::Text(s),
        }
    }
}

impl From<&SettingValue> for TomlSetting {
    fn from(v: &SettingValue) -> Self {
        match v {
            SettingValue::Bool(b) => TomlSetting::Bool(*b),
            SettingValue::Int(i) => TomlSetting::Int(*i),
            SettingValue::Float(f) => TomlSetting::Float(*f),
            SettingValue::Text(s) => TomlSetting::Text(s.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileFile {
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub devices: BTreeMap<String, BTreeMap<String, TomlSetting>>,
}

#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub root: PathBuf,
    pub config_toml: PathBuf,
    pub devices_dir: PathBuf,
    pub profiles_dir: PathBuf,
    pub remaps_dir: PathBuf,
}

impl ConfigPaths {
    pub fn xdg() -> Result<Self> {
        let root = dirs::config_dir().context("no config dir")?.join("hako");
        Ok(Self {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.devices_dir)?;
        fs::create_dir_all(&self.profiles_dir)?;
        fs::create_dir_all(&self.remaps_dir)?;
        Ok(())
    }
}

/// Defaults when the file does not exist yet (nothing is written).
pub fn load_app_config(paths: &ConfigPaths) -> Result<AppConfig> {
    if !paths.config_toml.exists() {
        return Ok(AppConfig::default());
    }
    let text = fs::read_to_string(&paths.config_toml)?;
    toml::from_str(&text).with_context(|| format!("parse {}", paths.config_toml.display()))
}

/// Load, edit and save `config.toml` as one step, so concurrent callers never
/// lose each other's changes. A parse error aborts without writing.
pub fn update_app_config<T>(
    paths: &ConfigPaths,
    edit: impl FnOnce(&mut AppConfig) -> Result<T>,
) -> Result<(AppConfig, T)> {
    let _guard = APP_CONFIG_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut cfg = load_app_config(paths)?;
    let out = edit(&mut cfg)?;
    paths.ensure()?;
    write_atomic(&paths.config_toml, &toml::to_string_pretty(&cfg)?)?;
    Ok((cfg, out))
}

/// Owner-only temp file, fsynced, then renamed over `path`: a crash never
/// leaves a truncated file and concurrent writers never share a temp file.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "toml.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let written = f
        .write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written?;
    // The rename itself is only durable once the directory is synced.
    if let Some(dir) = path.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

pub fn device_config_path(paths: &ConfigPaths, vid: u16, pid: u16) -> PathBuf {
    paths.devices_dir.join(format!("{vid:04x}_{pid:04x}.toml"))
}

pub fn load_device_settings(paths: &ConfigPaths, vid: u16, pid: u16) -> Result<SettingsMap> {
    let p = device_config_path(paths, vid, pid);
    if !p.exists() {
        return Ok(SettingsMap::new());
    }
    let text = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let file: DeviceConfigFile =
        toml::from_str(&text).with_context(|| format!("parse {}", p.display()))?;
    Ok(file
        .settings
        .into_iter()
        .map(|(k, v)| (normalize_key(k), v.into()))
        .collect())
}

pub fn save_device_settings(
    paths: &ConfigPaths,
    vid: u16,
    pid: u16,
    settings: &SettingsMap,
) -> Result<()> {
    paths.ensure()?;
    let mut file = DeviceConfigFile {
        version: 1,
        settings: BTreeMap::new(),
    };
    for (k, v) in settings {
        file.settings.insert(k.clone(), v.into());
    }
    let text = toml::to_string_pretty(&file)?;
    let p = device_config_path(paths, vid, pid);
    write_atomic(&p, &text).with_context(|| format!("save {}", p.display()))
}

pub fn list_profiles(paths: &ConfigPaths) -> Result<Vec<String>> {
    paths.ensure()?;
    let mut names = Vec::new();
    for ent in fs::read_dir(&paths.profiles_dir)? {
        let ent = ent?;
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) == Some("toml") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                // Only names ApplyProfile accepts.
                if validate_profile_name(stem).is_ok() {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Profile names become file names: no separators, dots, control chars, or
/// spaces at either end (the UI trims names; `" a"` and `"a"` must not differ).
pub fn validate_profile_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.trim() == name
        && name.len() <= 48
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'));
    anyhow::ensure!(ok, "invalid profile name {name:?}");
    Ok(())
}

pub fn load_profile(paths: &ConfigPaths, name: &str) -> Result<ProfileFile> {
    validate_profile_name(name)?;
    let p = paths.profiles_dir.join(format!("{name}.toml"));
    let text = fs::read_to_string(&p).with_context(|| format!("profile {name}"))?;
    toml::from_str(&text).with_context(|| format!("parse {}", p.display()))
}

pub fn save_profile(paths: &ConfigPaths, profile: &ProfileFile) -> Result<()> {
    validate_profile_name(&profile.name)?;
    paths.ensure()?;
    let p = paths.profiles_dir.join(format!("{}.toml", profile.name));
    let text = toml::to_string_pretty(profile)?;
    write_atomic(&p, &text)?;
    Ok(())
}

pub fn settings_from_profile_device(map: &BTreeMap<String, TomlSetting>) -> SettingsMap {
    map.iter()
        .map(|(k, v)| (normalize_key(k.clone()), SettingValue::from(v.clone())))
        .collect()
}

pub fn remap_config_path(paths: &ConfigPaths, vid: u16, pid: u16) -> PathBuf {
    paths.remaps_dir.join(format!("{vid:04x}_{pid:04x}.toml"))
}

pub fn load_remap_bindings(paths: &ConfigPaths, vid: u16, pid: u16) -> Result<Vec<RemapBinding>> {
    let p = remap_config_path(paths, vid, pid);
    if !p.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(p)?;
    let file: RemapFile = toml::from_str(&text)?;
    Ok(file.bindings)
}

pub fn save_remap_bindings(
    paths: &ConfigPaths,
    vid: u16,
    pid: u16,
    bindings: &[RemapBinding],
) -> Result<()> {
    paths.ensure()?;
    let file = RemapFile {
        version: 1,
        bindings: bindings.to_vec(),
    };
    let text = toml::to_string_pretty(&file)?;
    write_atomic(&remap_config_path(paths, vid, pid), &text)?;
    Ok(())
}

pub fn merge_setting(map: &mut SettingsMap, key: SettingKey, value: SettingValue) {
    map.insert(key.as_str().to_string(), value);
}

pub fn get_setting(map: &SettingsMap, key: SettingKey) -> Option<&SettingValue> {
    map.get(key.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_device_keys_load_under_new_names_and_save_atomically() {
        let root = std::env::temp_dir().join(format!("hako-cfg-{}", std::process::id()));
        let paths = ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root: root.clone(),
        };
        paths.ensure().unwrap();
        let file = device_config_path(&paths, 0x1532, 0x026b);
        fs::write(
            &file,
            "version = 1\n[settings]\nkb_brightness = 40\nkb_effect = \"wave\"\nodd = true\n",
        )
        .unwrap();

        let s = load_device_settings(&paths, 0x1532, 0x026b).unwrap();
        // A corrupt file names itself.
        let other = device_config_path(&paths, 0x1532, 0x0001);
        fs::write(&other, "version = [").unwrap();
        let err = load_device_settings(&paths, 0x1532, 0x0001).unwrap_err();
        assert!(format!("{err:#}").contains("1532_0001.toml"), "{err:#}");
        fs::remove_file(&other).unwrap();
        assert_eq!(
            get_setting(&s, SettingKey::Brightness),
            Some(&SettingValue::Int(40))
        );
        assert_eq!(
            get_setting(&s, SettingKey::Effect),
            Some(&SettingValue::Text("wave".into()))
        );
        assert!(s.contains_key("odd"), "unknown keys survive");

        save_device_settings(&paths, 0x1532, 0x026b, &s).unwrap();
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("brightness = 40") && !text.contains("kb_"));
        assert_eq!(
            fs::read_dir(&paths.devices_dir).unwrap().count(),
            1,
            "temp file left"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // A missing config is defaults, not a write.
        assert!(load_app_config(&paths).is_ok());
        assert!(!paths.config_toml.exists());
        let (cfg, ()) = update_app_config(&paths, |c| {
            c.last_profile = Some("a".into());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            load_app_config(&paths).unwrap().last_profile,
            cfg.last_profile
        );

        // An unparsable config is an error, never silently replaced by defaults.
        fs::write(&paths.config_toml, "version = [").unwrap();
        assert!(load_app_config(&paths).is_err());
        assert!(update_app_config(&paths, |_| Ok(())).is_err());
        assert_eq!(
            fs::read_to_string(&paths.config_toml).unwrap(),
            "version = ["
        );
        let _ = fs::remove_dir_all(root);
    }
}
