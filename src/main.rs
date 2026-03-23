//! LED controller daemon for REG-Linux handheld devices.
//!
//! Supports three LED backends:
//! - RGB: multicolor:chassis sysfs (Ayn Odin, Loki, etc.)
//! - PWM: htr3212-pwm PWM chips
//! - Individual: per-LED sysfs entries (Retroid Pocket 5)
//!
//! Features:
//! - Battery-level color indication via /userdata/system/configs/leds.conf
//! - Per-LED individual control via leds-individual.conf or colorsave.json
//! - Rainbow and pulse effects
//! - CLI for manual LED control

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ═══════════════════════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════════════════════

const CHECK_INTERVAL_SECS: u64 = 3;
const BLOCK_DURATION_SECS: f64 = 120.0;
const EFFECT_STEPS: u32 = 60;
const EFFECT_DURATION_MS: u64 = 2000;
const PULSE_DURATION_MS: u64 = 1000;

const CONFIG_FILE: &str = "/userdata/system/configs/leds.conf";
const INDIVIDUAL_CONFIG: &str = "/userdata/system/configs/leds-individual.conf";
const JSON_CONFIG: &str = "/userdata/system/configs/colorsave.json";
const BLOCK_FILE: &str = "/var/run/led-handheld-block";
const SYSTEM_CONF: &str = "/userdata/system/system.conf";

const DEFAULT_ES_COLOR: (u8, u8, u8) = (255, 0, 165);

// ═══════════════════════════════════════════════════════════════════════════════
// LED Backend Types
// ═══════════════════════════════════════════════════════════════════════════════

struct LedChannels {
    red: PathBuf,
    green: PathBuf,
    blue: PathBuf,
}

struct RgbBackend {
    base: PathBuf,
}

struct PwmBackend {
    chips: Vec<PathBuf>,
    period: u32,
}

struct IndividualBackend {
    leds: Vec<LedChannels>,
}

enum Backend {
    Rgb(RgbBackend),
    Pwm(PwmBackend),
    Individual(IndividualBackend),
}

// ═══════════════════════════════════════════════════════════════════════════════
// Backend Detection
// ═══════════════════════════════════════════════════════════════════════════════

fn detect_backend() -> Option<Backend> {
    // 1. Try multicolor:chassis (Ayn devices)
    let rgb_path = Path::new("/sys/class/leds/multicolor:chassis/multi_intensity");
    if rgb_path.exists() {
        return Some(Backend::Rgb(RgbBackend {
            base: PathBuf::from("/sys/class/leds/multicolor:chassis"),
        }));
    }

    // 2. Try individual per-LED sysfs (Retroid Pocket 5)
    if let Some(ind) = detect_individual() {
        return Some(Backend::Individual(ind));
    }

    // 3. Try PWM (htr3212-pwm)
    if let Some(pwm) = detect_pwm() {
        return Some(Backend::Pwm(pwm));
    }

    None
}

fn detect_individual() -> Option<IndividualBackend> {
    if !Path::new("/sys/class/leds/l:r1/brightness").exists() {
        return None;
    }

    let mut leds = Vec::with_capacity(8);
    for &(side, num) in &[
        ('l', 1), ('l', 2), ('l', 3), ('l', 4),
        ('r', 1), ('r', 2), ('r', 3), ('r', 4),
    ] {
        let r = PathBuf::from(format!("/sys/class/leds/{}:r{}/brightness", side, num));
        let g = PathBuf::from(format!("/sys/class/leds/{}:g{}/brightness", side, num));
        let b = PathBuf::from(format!("/sys/class/leds/{}:b{}/brightness", side, num));
        if !r.exists() || !g.exists() || !b.exists() {
            return None;
        }
        leds.push(LedChannels { red: r, green: g, blue: b });
    }

    Some(IndividualBackend { leds })
}

fn detect_pwm() -> Option<PwmBackend> {
    let pwm_dir = Path::new("/sys/class/pwm");
    let entries = fs::read_dir(pwm_dir).ok()?;
    let mut chips = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let name = fs::read_to_string(path.join("device/name")).unwrap_or_default();
        if name.trim() != "htr3212-pwm" {
            continue;
        }
        let npwm: u32 = fs::read_to_string(path.join("npwm"))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        if npwm == 0 || npwm % 3 != 0 {
            continue;
        }
        let period: u32 = 100;
        for i in 0..npwm {
            let pwm_dir = path.join(format!("pwm{}", i));
            if !pwm_dir.is_dir() {
                let _ = fs::write(path.join("export"), i.to_string());
            }
            let _ = fs::write(pwm_dir.join("enable"), "1");
            let _ = fs::write(pwm_dir.join("period"), period.to_string());
        }
        chips.push(path);
    }

    if chips.is_empty() {
        None
    } else {
        Some(PwmBackend { chips, period: 100 })
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Backend Implementation
// ═══════════════════════════════════════════════════════════════════════════════

impl RgbBackend {
    fn set_color(&self, r: u8, g: u8, b: u8) -> io::Result<()> {
        fs::write(self.base.join("multi_intensity"), format!("{} {} {}", r, g, b))
    }

    fn get_color(&self) -> io::Result<(u8, u8, u8)> {
        let s = fs::read_to_string(self.base.join("multi_intensity"))?;
        parse_space_rgb(s.trim())
    }

    fn set_brightness(&self, b: u8) -> io::Result<()> {
        fs::write(self.base.join("brightness"), b.to_string())
    }

    fn get_brightness(&self) -> io::Result<(u8, u8)> {
        let cur: u8 = fs::read_to_string(self.base.join("brightness"))?
            .trim()
            .parse()
            .unwrap_or(0);
        let max: u8 = fs::read_to_string(self.base.join("max_brightness"))?
            .trim()
            .parse()
            .unwrap_or(255);
        Ok((cur, max))
    }
}

impl PwmBackend {
    fn set_color(&self, r: u8, g: u8, b: u8) -> io::Result<()> {
        let rp = (r as u32 * self.period / 255).to_string();
        let gp = (g as u32 * self.period / 255).to_string();
        let bp = (b as u32 * self.period / 255).to_string();

        for chip in &self.chips {
            for i in (0..12).step_by(3) {
                fs::write(chip.join(format!("pwm{}/duty_cycle", i)), &rp)?;
            }
            for i in (1..12).step_by(3) {
                fs::write(chip.join(format!("pwm{}/duty_cycle", i)), &gp)?;
            }
            for i in (2..12).step_by(3) {
                fs::write(chip.join(format!("pwm{}/duty_cycle", i)), &bp)?;
            }
        }
        Ok(())
    }

    fn get_color(&self) -> io::Result<(u8, u8, u8)> {
        let chip = self.chips.first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no PWM chip")
        })?;
        let r = read_u32(&chip.join("pwm0/duty_cycle"))?;
        let g = read_u32(&chip.join("pwm1/duty_cycle"))?;
        let b = read_u32(&chip.join("pwm2/duty_cycle"))?;
        Ok((
            (r * 255 / self.period) as u8,
            (g * 255 / self.period) as u8,
            (b * 255 / self.period) as u8,
        ))
    }
}

impl IndividualBackend {
    fn set_led(&self, index: usize, r: u8, g: u8, b: u8) -> io::Result<()> {
        let led = self.leds.get(index).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "LED index out of range")
        })?;
        fs::write(&led.red, r.to_string())?;
        fs::write(&led.green, g.to_string())?;
        fs::write(&led.blue, b.to_string())?;
        Ok(())
    }

    fn set_all(&self, r: u8, g: u8, b: u8) -> io::Result<()> {
        for i in 0..self.leds.len() {
            self.set_led(i, r, g, b)?;
        }
        Ok(())
    }

    fn get_led(&self, index: usize) -> io::Result<(u8, u8, u8)> {
        let led = self.leds.get(index).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "LED index out of range")
        })?;
        let r = read_u8(&led.red)?;
        let g = read_u8(&led.green)?;
        let b = read_u8(&led.blue)?;
        Ok((r, g, b))
    }
}

impl Backend {
    fn set_all_color(&self, r: u8, g: u8, b: u8) -> io::Result<()> {
        match self {
            Backend::Rgb(rgb) => rgb.set_color(r, g, b),
            Backend::Pwm(pwm) => pwm.set_color(r, g, b),
            Backend::Individual(ind) => ind.set_all(r, g, b),
        }
    }

    fn get_color(&self) -> io::Result<(u8, u8, u8)> {
        match self {
            Backend::Rgb(rgb) => rgb.get_color(),
            Backend::Pwm(pwm) => pwm.get_color(),
            Backend::Individual(ind) => ind.get_led(0),
        }
    }

    fn turn_off(&self) -> io::Result<()> {
        self.set_all_color(0, 0, 0)
    }

    fn set_brightness(&self, b: u8) -> io::Result<()> {
        match self {
            Backend::Rgb(rgb) => rgb.set_brightness(b),
            _ => Ok(()),
        }
    }

    fn get_brightness(&self) -> io::Result<(u8, u8)> {
        match self {
            Backend::Rgb(rgb) => rgb.get_brightness(),
            _ => Ok((255, 255)),
        }
    }

    fn set_led_color(&self, index: usize, r: u8, g: u8, b: u8) -> io::Result<()> {
        match self {
            Backend::Individual(ind) => ind.set_led(index, r, g, b),
            _ => self.set_all_color(r, g, b),
        }
    }

    fn supports_individual(&self) -> bool {
        matches!(self, Backend::Individual(_))
    }

    fn num_leds(&self) -> usize {
        match self {
            Backend::Individual(ind) => ind.leds.len(),
            _ => 1,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Backend::Rgb(_) => "rgb",
            Backend::Pwm(_) => "pwm",
            Backend::Individual(_) => "individual",
        }
    }

    fn apply_brightness_conf(&self) {
        let brightness = read_conf_key("led.brightness")
            .and_then(|v| v.parse::<u8>().ok())
            .unwrap_or(128);
        let _ = self.set_brightness(brightness);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Battery Config (leds.conf)
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
enum LedColor {
    Hex(u8, u8, u8),
    Pulse,
    Rainbow,
    Off,
    EsColor,
}

struct BatteryThreshold {
    percent: u8,
    color: LedColor,
}

fn default_battery_config() -> Vec<BatteryThreshold> {
    vec![
        BatteryThreshold { percent: 100, color: LedColor::Hex(0x00, 0x99, 0x00) },
        BatteryThreshold { percent: 15,  color: LedColor::EsColor },
        BatteryThreshold { percent: 10,  color: LedColor::Hex(0xCC, 0x33, 0x33) },
        BatteryThreshold { percent: 5,   color: LedColor::Hex(0xFF, 0x00, 0x00) },
        BatteryThreshold { percent: 3,   color: LedColor::Pulse },
    ]
}

fn load_battery_config(path: &str) -> Vec<BatteryThreshold> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return default_battery_config(),
    };

    let mut thresholds = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let pct_str = match parts.next() {
            Some(s) => s.trim(),
            None => continue,
        };
        let color_str = match parts.next() {
            Some(s) => s.trim(),
            None => continue,
        };
        let percent: u8 = match pct_str.parse() {
            Ok(p) if p <= 100 => p,
            _ => continue,
        };
        let color = match color_str {
            "PULSE" => LedColor::Pulse,
            "RAINBOW" => LedColor::Rainbow,
            "OFF" => LedColor::Off,
            "ESCOLOR" => LedColor::EsColor,
            hex if hex.len() == 6 => {
                match parse_hex_color(hex) {
                    Some((r, g, b)) => LedColor::Hex(r, g, b),
                    None => continue,
                }
            }
            _ => continue,
        };
        thresholds.push(BatteryThreshold { percent, color });
    }

    if thresholds.is_empty() {
        return default_battery_config();
    }
    thresholds.sort_by(|a, b| b.percent.cmp(&a.percent));
    thresholds
}

fn color_for_battery(percent: u8, thresholds: &[BatteryThreshold]) -> &LedColor {
    for t in thresholds {
        if percent >= t.percent {
            return &t.color;
        }
    }
    &thresholds.last().unwrap().color
}

// ═══════════════════════════════════════════════════════════════════════════════
// Individual LED Config
// ═══════════════════════════════════════════════════════════════════════════════

struct IndividualLedEntry {
    index: usize,
    r: u8,
    g: u8,
    b: u8,
    brightness: u8,
}

struct IndividualConfig {
    leds: Vec<IndividualLedEntry>,
    group_left: Option<(u8, u8, u8, u8)>,   // r, g, b, brightness
    group_right: Option<(u8, u8, u8, u8)>,
    group_all: Option<(u8, u8, u8, u8)>,
}

fn led_name_to_index(name: &str) -> Option<usize> {
    match name {
        "l1" | "l1_right" => Some(0),
        "l2" | "l2_up"    => Some(1),
        "l3" | "l3_left"  => Some(2),
        "l4" | "l4_down"  => Some(3),
        "r1" | "r1_right" => Some(4),
        "r2" | "r2_up"    => Some(5),
        "r3" | "r3_left"  => Some(6),
        "r4" | "r4_down"  => Some(7),
        _ => None,
    }
}

/// Load per-LED config from INI format (leds-individual.conf).
fn load_individual_config(path: &str) -> Option<IndividualConfig> {
    let content = fs::read_to_string(path).ok()?;

    let mut config = IndividualConfig {
        leds: Vec::new(),
        group_left: None,
        group_right: None,
        group_all: None,
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let key = match parts.next() {
            Some(s) => s.trim().to_lowercase(),
            None => continue,
        };
        let value = match parts.next() {
            Some(s) => s.trim(),
            None => continue,
        };

        let mut vparts = value.splitn(2, ',');
        let hex = match vparts.next() {
            Some(s) => s.trim(),
            None => continue,
        };
        let brightness: u8 = vparts
            .next()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(255);

        let (r, g, b) = match parse_hex_color(hex) {
            Some(c) => c,
            None => continue,
        };

        match key.as_str() {
            "left" => config.group_left = Some((r, g, b, brightness)),
            "right" => config.group_right = Some((r, g, b, brightness)),
            "all" => config.group_all = Some((r, g, b, brightness)),
            name => {
                if let Some(index) = led_name_to_index(name) {
                    config.leds.push(IndividualLedEntry { index, r, g, b, brightness });
                }
            }
        }
    }

    Some(config)
}

// ═══════════════════════════════════════════════════════════════════════════════
// colorsave.json Parser (Retroid Pocket 5 / Batocera compatibility)
//
// Expected JSON structure:
// {
//   "Left Joystick": {
//     "L1_Right": {"enabled": true, "color": "#FF0000", "brightness": 50},
//     ...
//   },
//   "Right Joystick": {
//     "R1_Right": {"enabled": true, "color": "#FF0000", "brightness": 50},
//     ...
//   },
//   "Controls": {
//     "LEFT":  {"color": "#FF0000", "brightness": 50},
//     "RIGHT": {"color": "#FF0000", "brightness": 50},
//     "BOTH":  {"color": "#FF0000", "brightness": 50}
//   }
// }
// ═══════════════════════════════════════════════════════════════════════════════

/// Map JSON LED key names to hardware index.
fn json_led_name_to_index(name: &str) -> Option<usize> {
    match name {
        "L1_Right" => Some(0),
        "L2_Up"    => Some(1),
        "L3_Left"  => Some(2),
        "L4_Down"  => Some(3),
        "R1_Right" => Some(4),
        "R2_Up"    => Some(5),
        "R3_Left"  => Some(6),
        "R4_Down"  => Some(7),
        _ => None,
    }
}

/// Parse a "#RRGGBB" color string (with or without '#').
fn parse_json_color(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.trim().trim_start_matches('#');
    parse_hex_color(hex)
}

/// Minimal JSON string value extractor.
/// Finds `"key": "value"` and returns value (without quotes).
fn json_extract_string<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{}\"", key);
    let key_pos = obj.find(&pattern)?;
    let after_key = &obj[key_pos + pattern.len()..];
    // Skip whitespace and colon
    let after_colon = after_key.find(':').map(|i| &after_key[i + 1..])?;
    let trimmed = after_colon.trim_start();
    if !trimmed.starts_with('"') {
        return None;
    }
    let start = 1; // skip opening quote
    let end = trimmed[start..].find('"')?;
    Some(&trimmed[start..start + end])
}

/// Minimal JSON number extractor.
/// Finds `"key": 123` and returns the number.
fn json_extract_number(obj: &str, key: &str) -> Option<u8> {
    let pattern = format!("\"{}\"", key);
    let key_pos = obj.find(&pattern)?;
    let after_key = &obj[key_pos + pattern.len()..];
    let after_colon = after_key.find(':').map(|i| &after_key[i + 1..])?;
    let trimmed = after_colon.trim_start();
    // Parse digits
    let num_str: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    num_str.parse().ok()
}

/// Minimal JSON bool extractor.
/// Finds `"key": true/false` and returns the bool.
fn json_extract_bool(obj: &str, key: &str) -> Option<bool> {
    let pattern = format!("\"{}\"", key);
    let key_pos = obj.find(&pattern)?;
    let after_key = &obj[key_pos + pattern.len()..];
    let after_colon = after_key.find(':').map(|i| &after_key[i + 1..])?;
    let trimmed = after_colon.trim_start();
    if trimmed.starts_with("true") {
        Some(true)
    } else if trimmed.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Find the JSON object `"key": { ... }` and return the inner braces content.
fn json_extract_object<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{}\"", key);
    let key_pos = json.find(&pattern)?;
    let after_key = &json[key_pos + pattern.len()..];
    let brace_start = after_key.find('{')?;
    let content = &after_key[brace_start..];
    // Find matching closing brace
    let mut depth = 0;
    for (i, ch) in content.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&content[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Parse colorsave.json into an IndividualConfig.
fn load_json_config(path: &str) -> Option<IndividualConfig> {
    let json = fs::read_to_string(path).ok()?;

    let mut config = IndividualConfig {
        leds: Vec::new(),
        group_left: None,
        group_right: None,
        group_all: None,
    };

    // Parse individual LEDs from "Left Joystick" and "Right Joystick"
    for section in &["Left Joystick", "Right Joystick"] {
        if let Some(section_obj) = json_extract_object(&json, section) {
            for &led_name in &[
                "L1_Right", "L2_Up", "L3_Left", "L4_Down",
                "R1_Right", "R2_Up", "R3_Left", "R4_Down",
            ] {
                if let Some(led_obj) = json_extract_object(section_obj, led_name) {
                    let index = match json_led_name_to_index(led_name) {
                        Some(i) => i,
                        None => continue,
                    };
                    // "enabled" defaults to true; brightness=0 already means off
                    let enabled = json_extract_bool(led_obj, "enabled").unwrap_or(true);
                    let brightness = if enabled {
                        json_extract_number(led_obj, "brightness").unwrap_or(0)
                    } else {
                        0
                    };
                    let color_str = json_extract_string(led_obj, "color").unwrap_or("#000000");
                    let (r, g, b) = parse_json_color(color_str).unwrap_or((0, 0, 0));

                    config.leds.push(IndividualLedEntry { index, r, g, b, brightness });
                }
            }
        }
    }

    // Parse group controls from "Controls"
    if let Some(controls) = json_extract_object(&json, "Controls") {
        for &key in &["BOTH", "LEFT", "RIGHT"] {
            if let Some(obj) = json_extract_object(controls, key) {
                let brightness = json_extract_number(obj, "brightness").unwrap_or(0);
                let color_str = json_extract_string(obj, "color").unwrap_or("#000000");
                let (r, g, b) = parse_json_color(color_str).unwrap_or((0, 0, 0));
                // Only activate group if brightness > 0 (matches original behavior)
                if brightness > 0 {
                    let val = (r, g, b, brightness);
                    match key {
                        "BOTH"  => config.group_all = Some(val),
                        "LEFT"  => config.group_left = Some(val),
                        "RIGHT" => config.group_right = Some(val),
                        _ => {}
                    }
                }
            }
        }
    }

    Some(config)
}

/// Apply an IndividualConfig to hardware.
/// Priority: all > left/right > individual (matches original Retroid5 behavior).
fn apply_config_to_hw(backend: &Backend, config: &IndividualConfig) {
    if let Some((r, g, b, br)) = config.group_all {
        let (r, g, b) = scale_brightness(r, g, b, br);
        let _ = backend.set_all_color(r, g, b);
        return;
    }

    // Apply individual LEDs first
    for led in &config.leds {
        let (r, g, b) = scale_brightness(led.r, led.g, led.b, led.brightness);
        let _ = backend.set_led_color(led.index, r, g, b);
    }

    // Override with group settings
    if let Some((r, g, b, br)) = config.group_left {
        let (r, g, b) = scale_brightness(r, g, b, br);
        for i in 0..4 {
            let _ = backend.set_led_color(i, r, g, b);
        }
    }
    if let Some((r, g, b, br)) = config.group_right {
        let (r, g, b) = scale_brightness(r, g, b, br);
        for i in 4..8 {
            let _ = backend.set_led_color(i, r, g, b);
        }
    }
}

/// Load per-LED config from any supported format.
/// Tries: leds-individual.conf first, then colorsave.json, then a user-specified path.
fn apply_individual(backend: &Backend, path_override: Option<&str>) {
    if !backend.supports_individual() {
        eprintln!("Device does not support individual LED control");
        process::exit(1);
    }

    // If user specified a path, try it directly
    if let Some(path) = path_override {
        if path.ends_with(".json") {
            match load_json_config(path) {
                Some(config) => { apply_config_to_hw(backend, &config); return; }
                None => { eprintln!("Could not read JSON config: {}", path); process::exit(1); }
            }
        } else {
            match load_individual_config(path) {
                Some(config) => { apply_config_to_hw(backend, &config); return; }
                None => { eprintln!("Could not read config: {}", path); process::exit(1); }
            }
        }
    }

    // Auto-detect: try INI first, then JSON
    if let Some(config) = load_individual_config(INDIVIDUAL_CONFIG) {
        apply_config_to_hw(backend, &config);
        return;
    }
    if let Some(config) = load_json_config(JSON_CONFIG) {
        apply_config_to_hw(backend, &config);
        return;
    }

    eprintln!("No per-LED config found ({} or {})", INDIVIDUAL_CONFIG, JSON_CONFIG);
    process::exit(1);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Effects
// ═══════════════════════════════════════════════════════════════════════════════

fn rainbow_rgb(pos: f64) -> (u8, u8, u8) {
    let angle = pos * 360.0;
    let mut comp = [0u8; 3];

    for i in 0..3 {
        let start = ((i + 1) * 120 % 360) as f64;
        let diff = if angle < start {
            angle + 360.0 - start
        } else {
            angle - start
        };
        comp[i] = if diff < 60.0 {
            (diff / 60.0 * 255.0) as u8
        } else if diff <= 180.0 {
            255
        } else if diff < 240.0 {
            ((240.0 - diff) / 60.0 * 255.0) as u8
        } else {
            0
        };
    }

    (comp[0], comp[1], comp[2])
}

fn pulse_factor(step: u32, total_steps: u32) -> f64 {
    let half = total_steps as f64 / 2.0;
    if (step as f64) < half {
        1.0 - 2.0 * step as f64 / total_steps as f64
    } else {
        (step as f64 - half) / half
    }
}

fn do_rainbow(backend: &Backend) -> io::Result<()> {
    let prev = backend.get_color().unwrap_or((0, 0, 0));
    let step_ms = EFFECT_DURATION_MS / EFFECT_STEPS as u64;

    for i in 0..EFFECT_STEPS {
        let (r, g, b) = rainbow_rgb(i as f64 / EFFECT_STEPS as f64);
        backend.set_all_color(r, g, b)?;
        thread::sleep(Duration::from_millis(step_ms));
    }

    backend.set_all_color(prev.0, prev.1, prev.2)
}

fn do_pulse(backend: &Backend) -> io::Result<()> {
    let prev = backend.get_color().unwrap_or((0, 0, 0));
    let step_ms = PULSE_DURATION_MS / EFFECT_STEPS as u64;

    for i in 0..EFFECT_STEPS {
        let f = pulse_factor(i, EFFECT_STEPS);
        let r = (f * prev.0 as f64) as u8;
        let g = (f * prev.1 as f64) as u8;
        let b = (f * prev.2 as f64) as u8;
        backend.set_all_color(r, g, b)?;
        thread::sleep(Duration::from_millis(step_ms));
    }

    backend.set_all_color(prev.0, prev.1, prev.2)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Battery Monitoring
// ═══════════════════════════════════════════════════════════════════════════════

fn find_battery_path() -> Option<PathBuf> {
    let ps_dir = Path::new("/sys/class/power_supply");
    for entry in fs::read_dir(ps_dir).ok()?.flatten() {
        let path = entry.path();
        let ptype = fs::read_to_string(path.join("type")).unwrap_or_default();
        if ptype.trim() == "Battery" && path.join("capacity").exists() {
            return Some(path);
        }
    }
    None
}

fn read_battery(path: &Path) -> io::Result<(u8, bool)> {
    let capacity: u8 = fs::read_to_string(path.join("capacity"))?
        .trim()
        .parse()
        .unwrap_or(0);
    let status = fs::read_to_string(path.join("status"))?;
    let charging = matches!(status.trim(), "Charging" | "Full");
    Ok((capacity, charging))
}

fn resolve_es_color() -> (u8, u8, u8) {
    if let Some(val) = read_conf_key("led.colour") {
        let parts: Vec<&str> = val.split_whitespace().collect();
        if parts.len() == 3 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                parts[0].parse::<u8>(),
                parts[1].parse::<u8>(),
                parts[2].parse::<u8>(),
            ) {
                return (r, g, b);
            }
        }
    }
    DEFAULT_ES_COLOR
}

// ═══════════════════════════════════════════════════════════════════════════════
// Block / Unblock Color Changes
// ═══════════════════════════════════════════════════════════════════════════════

fn block_changes(block: bool) {
    if block {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        let _ = fs::write(BLOCK_FILE, format!("{:.0}", now));
    } else {
        let _ = fs::write(BLOCK_FILE, "0");
    }
}

fn changes_allowed() -> bool {
    let content = match fs::read_to_string(BLOCK_FILE) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let timestamp: f64 = content.trim().parse().unwrap_or(0.0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    if now - timestamp < BLOCK_DURATION_SECS {
        return false;
    }
    let _ = fs::write(BLOCK_FILE, "0");
    true
}

// ═══════════════════════════════════════════════════════════════════════════════
// Daemon
// ═══════════════════════════════════════════════════════════════════════════════

fn daemon_start(backend: &Backend) {
    let battery_path = match find_battery_path() {
        Some(p) => p,
        None => {
            eprintln!("No battery found");
            process::exit(1);
        }
    };

    backend.apply_brightness_conf();

    let thresholds = load_battery_config(CONFIG_FILE);

    loop {
        if let Ok((capacity, charging)) = read_battery(&battery_path) {
            let effective = if charging {
                100
            } else if capacity >= 100 {
                99
            } else {
                capacity
            };

            if changes_allowed() {
                let color = color_for_battery(effective, &thresholds);
                let _ = apply_color(backend, color);
            }
        }

        thread::sleep(Duration::from_secs(CHECK_INTERVAL_SECS));
    }
}

fn apply_color(backend: &Backend, color: &LedColor) -> io::Result<()> {
    match color {
        LedColor::Hex(r, g, b) => backend.set_all_color(*r, *g, *b),
        LedColor::Pulse => do_pulse(backend),
        LedColor::Rainbow => do_rainbow(backend),
        LedColor::Off => backend.turn_off(),
        LedColor::EsColor => {
            let (r, g, b) = resolve_es_color();
            backend.set_all_color(r, g, b)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Utility Functions
// ═══════════════════════════════════════════════════════════════════════════════

fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn parse_space_rgb(s: &str) -> io::Result<(u8, u8, u8)> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 3 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected 3 values"));
    }
    Ok((
        parts[0].parse().unwrap_or(0),
        parts[1].parse().unwrap_or(0),
        parts[2].parse().unwrap_or(0),
    ))
}

fn read_u8(path: &Path) -> io::Result<u8> {
    Ok(fs::read_to_string(path)?.trim().parse().unwrap_or(0))
}

fn read_u32(path: &Path) -> io::Result<u32> {
    Ok(fs::read_to_string(path)?.trim().parse().unwrap_or(0))
}

fn scale_brightness(r: u8, g: u8, b: u8, brightness: u8) -> (u8, u8, u8) {
    let s = brightness as f64 / 255.0;
    (
        (r as f64 * s) as u8,
        (g as f64 * s) as u8,
        (b as f64 * s) as u8,
    )
}

/// Read a key=value from system.conf.
fn read_conf_key(key: &str) -> Option<String> {
    let prefix = format!("{}=", key);
    if let Ok(content) = fs::read_to_string(SYSTEM_CONF) {
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with(&prefix) {
                let val = &line[prefix.len()..];
                let val = val.split('#').next().unwrap_or("").trim();
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

fn print_usage(prog: &str) {
    eprintln!("Usage: {} <command> [args...]", prog);
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  start                   Start battery monitoring daemon");
    eprintln!("  stop / off              Turn off all LEDs");
    eprintln!("  detect                  Print detected backend type");
    eprintln!("  set_color RRGGBB        Set color (hex)");
    eprintln!("  get_color               Get current color (hex)");
    eprintln!("  set_color_dec R G B     Set color (decimal 0-255)");
    eprintln!("  get_color_dec           Get current color (decimal)");
    eprintln!("  set_brightness N        Set brightness (0-255)");
    eprintln!("  get_brightness          Get brightness (current max)");
    eprintln!("  rainbow                 Play rainbow effect");
    eprintln!("  pulse                   Play pulse effect");
    eprintln!("  block_color_changes     Block daemon color changes");
    eprintln!("  unblock_color_changes   Unblock daemon color changes");
    eprintln!("  apply [path]            Apply per-LED config (.conf or .json)");
    eprintln!("  set_led IDX RRGGBB [B]  Set individual LED (index, color, brightness)");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════════════════════════

fn main() {
    let args: Vec<String> = env::args().collect();
    let prog = &args[0];

    let backend = match detect_backend() {
        Some(b) => b,
        None => {
            if args.len() > 1 && args[1] == "detect" {
                eprintln!("Unsupported");
                process::exit(1);
            }
            eprintln!("Unsupported device (no LED backend found)");
            process::exit(1);
        }
    };

    if args.len() < 2 {
        if let Some(path) = find_battery_path() {
            if let Ok((pct, charging)) = read_battery(&path) {
                let status = if charging { "Charging" } else { "Discharging" };
                println!("Battery: {}% ({})", pct, status);
            }
        } else {
            println!("No battery found");
        }
        return;
    }

    match args[1].as_str() {
        "detect" => {
            println!("{}", backend.name());
        }
        "start" => {
            daemon_start(&backend);
        }
        "stop" | "off" => {
            let _ = backend.turn_off();
        }
        "rainbow" | "retroachievement" => {
            if changes_allowed() {
                let _ = do_rainbow(&backend);
            }
        }
        "pulse" => {
            if changes_allowed() {
                let _ = do_pulse(&backend);
            }
        }
        "set_color" => {
            if args.len() < 3 {
                eprintln!("Usage: {} set_color RRGGBB", prog);
                process::exit(1);
            }
            if changes_allowed() {
                match parse_hex_color(&args[2]) {
                    Some((r, g, b)) => { let _ = backend.set_all_color(r, g, b); }
                    None => {
                        eprintln!("Invalid color: {}", args[2]);
                        process::exit(1);
                    }
                }
            }
        }
        "get_color" => {
            match backend.get_color() {
                Ok((r, g, b)) => println!("{:02X}{:02X}{:02X}", r, g, b),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            }
        }
        "set_color_dec" => {
            if args.len() < 5 {
                eprintln!("Usage: {} set_color_dec R G B", prog);
                process::exit(1);
            }
            if changes_allowed() {
                let r: u8 = args[2].parse().unwrap_or(0);
                let g: u8 = args[3].parse().unwrap_or(0);
                let b: u8 = args[4].parse().unwrap_or(0);
                let _ = backend.set_all_color(r, g, b);
            }
        }
        "set_color_force_dec" => {
            if args.len() < 5 {
                eprintln!("Usage: {} set_color_force_dec R G B", prog);
                process::exit(1);
            }
            let r: u8 = args[2].parse().unwrap_or(0);
            let g: u8 = args[3].parse().unwrap_or(0);
            let b: u8 = args[4].parse().unwrap_or(0);
            let _ = backend.set_all_color(r, g, b);
        }
        "get_color_dec" => {
            match backend.get_color() {
                Ok((r, g, b)) => println!("{} {} {}", r, g, b),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            }
        }
        "set_brightness" => {
            if args.len() < 3 {
                eprintln!("Usage: {} set_brightness N", prog);
                process::exit(1);
            }
            let b: u8 = args[2].parse().unwrap_or(128);
            let _ = backend.set_brightness(b);
        }
        "get_brightness" => {
            match backend.get_brightness() {
                Ok((b, m)) => println!("{} {}", b, m),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    process::exit(1);
                }
            }
        }
        "block_color_changes" => {
            block_changes(true);
        }
        "unblock_color_changes" => {
            block_changes(false);
        }
        "apply" => {
            let path_override = args.get(2).map(|s| s.as_str());
            apply_individual(&backend, path_override);
        }
        "set_led" => {
            if args.len() < 4 {
                eprintln!("Usage: {} set_led INDEX RRGGBB [BRIGHTNESS]", prog);
                process::exit(1);
            }
            if !backend.supports_individual() {
                eprintln!("Device does not support individual LED control");
                process::exit(1);
            }
            let index: usize = args[2].parse().unwrap_or(0);
            if index >= backend.num_leds() {
                eprintln!("LED index {} out of range (0-{})", index, backend.num_leds() - 1);
                process::exit(1);
            }
            match parse_hex_color(&args[3]) {
                Some((r, g, b)) => {
                    let brightness: u8 = args.get(4)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(255);
                    let (r, g, b) = scale_brightness(r, g, b, brightness);
                    let _ = backend.set_led_color(index, r, g, b);
                }
                None => {
                    eprintln!("Invalid color: {}", args[3]);
                    process::exit(1);
                }
            }
        }
        "--help" | "-h" | "help" => {
            print_usage(prog);
        }
        cmd => {
            eprintln!("Unknown command: {}", cmd);
            print_usage(prog);
            process::exit(1);
        }
    }
}
