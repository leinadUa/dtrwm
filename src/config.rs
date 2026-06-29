use serde::Deserialize;
use xkbcommon::xkb;

#[derive(Deserialize)]
#[serde(default)]
pub struct Config {
    pub master_ratio: f64,
    pub border_px: i32,
    pub default_layout: String,
    pub focus_follows_mouse: bool,
    pub colors: Colors,
    pub bindings: Vec<Binding>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            master_ratio: 0.55,
            border_px: 2,
            default_layout: "tile".into(),
            focus_follows_mouse: false,
            colors: Colors::default(),
            bindings: default_bindings(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
pub struct Colors {
    pub focused: String,
    pub unfocused: String,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            focused: "#88c0d0ff".into(),
            unfocused: "#3b4252ff".into(),
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct Binding {
    pub keys: String,
    pub action: String,
    #[serde(default)]
    pub arg: String,
}

fn default_bindings() -> Vec<Binding> {
    [
        ("Super+Return",   "spawn",          "kitty"),
        ("Super+q",        "close",          ""),
        ("Super+Shift+e",  "quit",           ""),
        ("Super+j",        "focus_next",     ""),
        ("Super+k",        "focus_prev",     ""),
        ("Super+t",        "set_layout",     "tile"),
        ("Super+g",        "set_layout",     "grid"),
        ("Super+m",        "set_layout",     "monocle"),
        ("Super+space",    "toggle_float",   ""),
        ("Super+r",        "reload",         ""),
    ].iter()
        .map(|(keys, action, arg)| Binding {
            keys: (*keys).into(),
            action: (*action).into(),
            arg: (*arg).into(),
        })
        .collect()
}

pub fn load() -> Config {
    let dirs = match xdg::BaseDirectories::with_prefix("dtrwm") {
        Ok(d) => d,
        Err(_) => return Config::default(),
    };
    if let Some(path) = dirs.find_config_file("config.toml") {
        if let Ok(text) = std::fs::read_to_string(path) {
            match toml::from_str::<Config>(&text) {
                Ok(cfg) => return cfg,
                Err(e) => log::warn!("config parse error: {e}"),
            }
        }
    }
    Config::default()
}

pub fn parse_color(s: &str) -> (u32, u32, u32, u32) {
    let s = s.trim_start_matches('#');
    if s.len() < 6 {
        return (0, 0, 0, 0xffffffff);
    }
    let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(0);
    let a = if s.len() >= 8 {
        u8::from_str_radix(&s[6..8], 16).unwrap_or(0xff)
    } else {
        0xff
    };
    let scale = |v: u8| (v as u32) * 0x01010101;
    (scale(r), scale(g), scale(b), scale(a))
}

pub fn parse_key(keys: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = keys.split('+').collect();
    let (key_name, mod_parts) = parts.split_last()?;
    let mut mods: u32 = 0;
    for m in mod_parts {
        mods |= match m.to_lowercase().as_str() {
            "super" | "meta" | "win" | "logo" | "mod4" => 64,
            "shift" => 1,
            "ctrl" | "control" => 4,
            "alt" | "mod1" => 8,
            _ => {
                log::warn!("unknown modifier: {m}");
                return None;
            }
        };
    }
    let sym = xkb::keysym_from_name(key_name, xkb::KEYSYM_CASE_INSENSITIVE);
    if sym.raw() == 0 {
        log::warn!("unknown key: {key_name}");
        return None;
    }
    log::debug!("parse_key: {keys} → keysym=0x{:x} mods=0x{:x}", sym.raw(), mods);
    Some((sym.raw(), mods))
}
