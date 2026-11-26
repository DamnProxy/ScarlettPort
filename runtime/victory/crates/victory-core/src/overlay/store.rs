//! What the overlay keeps: the settings it edits and how they are saved.
//!
//! Keys, mouse and controller settings go to `[controls]` of the config
//! file (`victory.toml`), the window and the graph to `[display]` and the
//! pacing switches to `[pace]`. The file is edited line by line, so its
//! comments, its order and every other setting stay as the user left them;
//! bindings go to the key map file (`VICTORY_KEYMAP`, else `keymap.txt`
//! beside the config file) as the keys that differ from the defaults.
//!
//! Environment variables still win over the file at the next start.

use std::path::{Path, PathBuf};

use crate::kbm::Settings;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowMode {
    #[default]
    Windowed,
    /// A window without a frame the size of its monitor.
    Borderless,
}

impl WindowMode {
    pub fn name(self) -> &'static str {
        match self {
            WindowMode::Windowed => "windowed",
            WindowMode::Borderless => "borderless",
        }
    }

    pub fn parse(text: &str) -> Option<WindowMode> {
        match text.trim().to_ascii_lowercase().as_str() {
            "windowed" | "window" => Some(WindowMode::Windowed),
            "borderless" | "fullscreen" => Some(WindowMode::Borderless),
            _ => None,
        }
    }
}

/// `[display]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    pub window_mode: WindowMode,
    /// The frame-time graph on screen while the menu is closed.
    pub graph: bool,
    /// Pixels to a font pixel (0: by the window's height).
    pub ui_scale: u8,
}

impl Default for Display {
    fn default() -> Display {
        Display {
            window_mode: WindowMode::Windowed,
            graph: false,
            ui_scale: 0,
        }
    }
}

impl Display {
    /// `[display]` of the config file; what cannot be read is named in the
    /// second result and left at its default.
    pub fn parse(table: &toml::Table) -> (Display, Vec<String>) {
        let mut d = Display::default();
        let mut problems = Vec::new();
        for (key, value) in table {
            match (key.as_str(), value) {
                ("window_mode", toml::Value::String(s)) => match WindowMode::parse(s) {
                    Some(m) => d.window_mode = m,
                    None => problems.push(format!(
                        "[display] window_mode = \"{s}\": expected windowed or borderless"
                    )),
                },
                ("fps_graph", toml::Value::Boolean(b)) => d.graph = *b,
                ("ui_scale", toml::Value::Integer(n)) if (0..=6).contains(n) => {
                    d.ui_scale = *n as u8
                }
                (k, _) => problems.push(format!("[display] {k}: not understood")),
            }
        }
        (d, problems)
    }
}

/// The frame caps the overlay offers: `None` is the title's own interval.
pub const FRAME_CAPS: [Option<u32>; 8] = [
    None,
    Some(30),
    Some(45),
    Some(60),
    Some(90),
    Some(120),
    Some(144),
    Some(240),
];

/// `p` with the frame cap `cap` chosen. A cap is held by the pacer, so
/// choosing one turns pacing on; "Title" leaves pacing as it was.
pub fn with_frame_cap(mut p: PaceModel, cap: Option<u32>) -> PaceModel {
    p.fps = cap;
    if cap.is_some() {
        p.pace = true;
    }
    p
}

/// The pacing switches as the overlay edits them (`victory_core::pace`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaceModel {
    pub pace: bool,
    pub fps: Option<u32>,
    pub input: bool,
    pub latency: u32,
    pub tearing: bool,
    pub feedback: bool,
}

impl PaceModel {
    /// The switches in force: the start-up ones with the live changes.
    pub fn current() -> PaceModel {
        let s = crate::pace::switches();
        PaceModel {
            pace: crate::pace::live_pace(),
            fps: crate::pace::live_fps(),
            input: crate::pace::live_input(),
            latency: s.latency,
            tearing: s.tearing,
            feedback: s.feedback,
        }
    }

    /// What the settings say at start, before any live change.
    pub fn at_start() -> PaceModel {
        let s = crate::pace::switches();
        PaceModel {
            pace: s.pace,
            fps: s.fps,
            input: s.input,
            latency: s.latency,
            tearing: s.tearing,
            feedback: s.feedback,
        }
    }

    /// Names of the settings that differ from `start` and need a restart.
    pub fn restart_needed(&self, start: &PaceModel) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.latency != start.latency {
            v.push("frame latency");
        }
        if self.tearing != start.tearing {
            v.push("tearing");
        }
        if self.feedback != start.feedback {
            v.push("display feedback");
        }
        v
    }
}

/// The upscaler and frame generation as the overlay edits them
/// (`victory_core::upscale`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UpscaleModel {
    pub upscaler: crate::upscale::Upscaler,
    pub quality: crate::upscale::Quality,
    pub sharpness: f32,
    pub framegen: bool,
}

impl Default for UpscaleModel {
    fn default() -> UpscaleModel {
        let s = crate::upscale::Switches::default();
        UpscaleModel {
            upscaler: s.upscaler,
            quality: s.quality,
            sharpness: s.sharpness,
            framegen: s.framegen,
        }
    }
}

impl UpscaleModel {
    /// The switches in force: the start-up ones with the live changes.
    pub fn current() -> UpscaleModel {
        UpscaleModel {
            upscaler: crate::upscale::live_upscaler(),
            framegen: crate::upscale::live_framegen(),
            sharpness: crate::upscale::live_sharpness(),
            ..UpscaleModel::at_start()
        }
    }

    pub fn at_start() -> UpscaleModel {
        let s = crate::upscale::switches();
        UpscaleModel {
            upscaler: s.upscaler,
            quality: s.quality,
            sharpness: s.sharpness,
            framegen: s.framegen,
        }
    }

    /// Names of the settings that differ from `start` and need a restart:
    /// the quality (it sets the render size at load) and a move to or from
    /// the title's own upscaler (a patch made at load).
    pub fn restart_needed(&self, start: &UpscaleModel) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.quality != start.quality {
            v.push("upscale quality");
        }
        if self.upscaler.wants_own_upscaler() != start.upscaler.wants_own_upscaler() {
            v.push("title upscaler");
        }
        v
    }
}

/// Everything the menu edits.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub controls: Settings,
    pub display: Display,
    pub pace: PaceModel,
    pub upscale: UpscaleModel,
}

/// One setting to write: `section`'s `key` is `value`, or removed (`None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub section: &'static str,
    pub key: &'static str,
    pub value: Option<String>,
}

fn number(v: f32) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0');
    if s.ends_with('.') {
        format!("{s}0")
    } else {
        s.to_string()
    }
}

fn edit(section: &'static str, key: &'static str, value: impl Into<Option<String>>) -> Edit {
    Edit {
        section,
        key,
        value: value.into(),
    }
}

/// The lines the config file should have for `m`.
pub fn edits(m: &Model) -> Vec<Edit> {
    let c = &m.controls;
    let b = |v: bool| Some(v.to_string());
    vec![
        edit("controls", "mouse_sensitivity", Some(number(c.sensitivity))),
        edit("controls", "mouse_y_scale", Some(number(c.y_scale))),
        edit("controls", "mouse_invert_y", b(c.invert_y)),
        edit("controls", "walk_speed", Some(number(c.walk_speed))),
        edit("controls", "pad_deadzone", Some(number(c.pad_deadzone))),
        edit(
            "display",
            "window_mode",
            Some(format!("\"{}\"", m.display.window_mode.name())),
        ),
        edit("display", "fps_graph", b(m.display.graph)),
        edit("display", "ui_scale", Some(m.display.ui_scale.to_string())),
        edit("pace", "pace", b(m.pace.pace)),
        edit("pace", "fps", m.pace.fps.map(|f| f.to_string())),
        edit("pace", "input", b(m.pace.input)),
        edit("pace", "latency", Some(m.pace.latency.to_string())),
        edit("pace", "tearing", b(m.pace.tearing)),
        edit("pace", "feedback", b(m.pace.feedback)),
        edit("upscale", "upscaler", Some(format!("\"{}\"", m.upscale.upscaler.name()))),
        edit("upscale", "quality", Some(format!("\"{}\"", m.upscale.quality.name()))),
        edit("upscale", "sharpness", Some(number(m.upscale.sharpness))),
        edit("upscale", "framegen", b(m.upscale.framegen)),
    ]
}

/// The name in a `[section]` or `[[section]]` line.
fn header(line: &str) -> Option<&str> {
    let t = line.trim();
    let t = t.split('#').next().unwrap_or("").trim();
    let inner = t.strip_prefix('[')?.strip_suffix(']')?;
    Some(inner.trim_start_matches('[').trim_end_matches(']').trim())
}

/// The key a `key = value` line sets.
fn key_of(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    let key = t.split_once('=')?.0.trim();
    Some(key.trim_matches('"'))
}

fn apply(lines: &mut Vec<String>, e: &Edit) {
    let Some(start) = lines.iter().position(|l| header(l) == Some(e.section)) else {
        if let Some(v) = &e.value {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{}]", e.section));
            lines.push(format!("{} = {v}", e.key));
        }
        return;
    };
    let end = (start + 1..lines.len())
        .find(|&i| header(&lines[i]).is_some())
        .unwrap_or(lines.len());
    let at = (start + 1..end).find(|&i| key_of(&lines[i]) == Some(e.key));
    match (at, &e.value) {
        (Some(i), Some(v)) => {
            let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
            let after = lines[i].split_once('=').map_or("", |(_, rest)| rest);
            let comment = after
                .find('#')
                .map(|h| format!(" {}", &after[h..]))
                .unwrap_or_default();
            lines[i] = format!("{indent}{} = {v}{comment}", e.key);
        }
        (Some(i), None) => {
            lines.remove(i);
        }
        (None, Some(v)) => {
            let last = (start + 1..end)
                .rev()
                .find(|&i| !lines[i].trim().is_empty())
                .unwrap_or(start);
            lines.insert(last + 1, format!("{} = {v}", e.key));
        }
        (None, None) => {}
    }
}

/// `text` with `edits` made, every other line as it was. Refuses (the
/// message says why) when the result is not a valid TOML file, so a file
/// that is not what we expect is never made worse.
pub fn edit_toml(text: &str, edits: &[Edit]) -> Result<String, String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    for e in edits {
        apply(&mut lines, e);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    toml::from_str::<toml::Table>(&out)
        .map_err(|e| format!("the settings file would no longer parse: {e}"))?;
    Ok(out)
}

/// The first line of a key map file the overlay wrote.
pub const KEYMAP_HEADER: &str = "# Key bindings changed from the defaults";

/// What one save writes.
#[derive(Clone, Debug)]
pub struct SaveJob {
    pub settings_path: PathBuf,
    pub edits: Vec<Edit>,
    pub keymap_path: Option<PathBuf>,
    pub keymap_text: String,
}

/// Writes `text` to `path` through a temporary file beside it, so a crash
/// never leaves half a file.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Makes the files say what `job` says. The message of the first failure.
pub fn save(job: &SaveJob) -> Result<(), String> {
    let mut failure = None;
    let old = match std::fs::read_to_string(&job.settings_path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(e) => {
            failure = Some(format!("{}: {e}", job.settings_path.display()));
            None
        }
    };
    if let Some(old) = old {
        match edit_toml(&old, &job.edits) {
            Ok(new) => {
                if new != old
                    && let Err(e) = write_atomic(&job.settings_path, &new)
                {
                    failure = Some(format!("{}: {e}", job.settings_path.display()));
                }
            }
            Err(e) => failure = Some(format!("{}: {e}", job.settings_path.display())),
        }
    }
    if let Some(path) = &job.keymap_path {
        // A key map file somebody wrote by hand is kept beside the new one.
        if let Ok(old) = std::fs::read_to_string(path)
            && !old.starts_with(KEYMAP_HEADER)
        {
            let mut bak = path.as_os_str().to_owned();
            bak.push(".bak");
            if !Path::new(&bak).exists() {
                let _ = std::fs::write(&bak, old);
            }
        }
        if let Err(e) = write_atomic(path, &job.keymap_text) {
            failure.get_or_insert(format!("{}: {e}", path.display()));
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kbm::{Output, key_code};

    fn e(section: &'static str, key: &'static str, value: Option<&str>) -> Edit {
        Edit {
            section,
            key,
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn a_value_is_replaced_in_place_and_nothing_else_moves() {
        let text = "# my config\n[paths]\ngame_dir = 'D:\\g'\n\n[controls]\n# mouse_sensitivity = 1.0\nmouse_sensitivity = 2.5 # mine\nwalk_speed = 0.5\n\n[controls.on_foot]\nSpace = \"X\"\n";
        let out = edit_toml(text, &[e("controls", "mouse_sensitivity", Some("1.25"))]).unwrap();
        assert_eq!(
            out,
            "# my config\n[paths]\ngame_dir = 'D:\\g'\n\n[controls]\n# mouse_sensitivity = 1.0\nmouse_sensitivity = 1.25 # mine\nwalk_speed = 0.5\n\n[controls.on_foot]\nSpace = \"X\"\n"
        );
    }

    #[test]
    fn a_missing_key_joins_its_section_and_a_missing_section_is_added() {
        let text = "[controls]\nwalk_speed = 0.5\n\n[controls.on_foot]\nSpace = \"X\"\n";
        let out = edit_toml(
            text,
            &[
                e("controls", "pad_deadzone", Some("0.1")),
                e("display", "fps_graph", Some("true")),
                e("display", "ui_scale", Some("2")),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            "[controls]\nwalk_speed = 0.5\npad_deadzone = 0.1\n\n[controls.on_foot]\nSpace = \"X\"\n\n[display]\nfps_graph = true\nui_scale = 2\n"
        );
        // From nothing at all.
        let out = edit_toml("", &[e("pace", "pace", Some("true"))]).unwrap();
        assert_eq!(out, "[pace]\npace = true\n");
    }

    #[test]
    fn a_value_of_none_removes_the_key() {
        let text = "[pace]\npace = true\nfps = 60\n";
        let out = edit_toml(text, &[e("pace", "fps", None), e("pace", "tearing", None)]).unwrap();
        assert_eq!(out, "[pace]\npace = true\n");
    }

    #[test]
    fn a_file_the_edit_would_break_is_refused() {
        // `controls` as an inline table; a `[controls]` section after it is a duplicate.
        let text = "controls = { walk_speed = 0.5 }\n";
        assert!(edit_toml(text, &[e("controls", "pad_deadzone", Some("0.1"))]).is_err());
    }

    #[test]
    fn model_edits_round_trip_through_the_loaders() {
        let mut m = Model {
            controls: Settings::default(),
            display: Display::default(),
            pace: PaceModel::default(),
            upscale: UpscaleModel::default(),
        };
        m.controls.sensitivity = 1.75;
        m.controls.invert_y = true;
        m.controls.pad_deadzone = 0.15;
        m.display = Display {
            window_mode: WindowMode::Borderless,
            graph: true,
            ui_scale: 3,
        };
        m.pace = PaceModel {
            pace: true,
            fps: Some(60),
            input: false,
            latency: 2,
            tearing: true,
            feedback: false,
        };
        let text = edit_toml("", &edits(&m)).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        let s = Settings::parse(table["controls"].as_table().unwrap()).unwrap();
        assert_eq!(
            (s.sensitivity, s.invert_y, s.pad_deadzone),
            (1.75, true, 0.15)
        );
        let (d, problems) = Display::parse(table["display"].as_table().unwrap());
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(d, m.display);
        let s = crate::pace::Switches::from_lookup(|name| {
            let key = name
                .strip_prefix("VICTORY_PACE")
                .unwrap_or("")
                .trim_start_matches('_')
                .to_ascii_lowercase();
            let key = if key.is_empty() {
                "pace".to_string()
            } else {
                key
            };
            match table["pace"].as_table().unwrap().get(&key)? {
                toml::Value::Boolean(b) => Some(if *b { "1" } else { "0" }.to_string()),
                toml::Value::Integer(i) => Some(i.to_string()),
                _ => None,
            }
        });
        assert!(s.pace && s.tearing && !s.input && !s.feedback);
        assert_eq!((s.fps, s.latency), (Some(60), 2));
    }

    #[test]
    fn numbers_are_written_short() {
        assert_eq!(number(1.0), "1.0");
        assert_eq!(number(0.25), "0.25");
        assert_eq!(number(0.1 + 0.2), "0.3");
        assert_eq!(number(12.5), "12.5");
    }

    #[test]
    fn display_reads_its_table_and_names_what_it_cannot() {
        let t: toml::Table = toml::from_str(
            "window_mode = \"Borderless\"\nfps_graph = true\nui_scale = 9\nbogus = 1\n",
        )
        .unwrap();
        let (d, problems) = Display::parse(&t);
        assert_eq!(d.window_mode, WindowMode::Borderless);
        assert!(d.graph);
        assert_eq!(d.ui_scale, 0);
        assert_eq!(problems.len(), 2);
    }

    #[test]
    fn restart_is_asked_only_for_what_needs_it() {
        let start = PaceModel::default();
        let live = PaceModel {
            pace: true,
            fps: Some(30),
            input: true,
            ..start
        };
        assert!(live.restart_needed(&start).is_empty());
        let swap = PaceModel {
            latency: 1,
            tearing: true,
            ..live
        };
        assert_eq!(
            swap.restart_needed(&start),
            vec!["frame latency", "tearing"]
        );
    }

    #[test]
    fn saving_writes_both_files_and_keeps_the_users_lines() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("victory.toml");
        std::fs::write(&settings, "# mine\n[paths]\ngame_dir = 'x'\n").unwrap();
        let mut m = Model {
            controls: Settings::default(),
            display: Display::default(),
            pace: PaceModel::default(),
            upscale: UpscaleModel::default(),
        };
        m.controls
            .on_foot
            .rebind(Output::Walk, None, Some(key_code("X").unwrap()));
        let job = SaveJob {
            settings_path: settings.clone(),
            edits: edits(&m),
            keymap_path: Some(dir.path().join("sub").join("keymap.txt")),
            keymap_text: m.controls.keymap_text(),
        };
        save(&job).unwrap();
        let text = std::fs::read_to_string(&settings).unwrap();
        assert!(text.starts_with("# mine\n[paths]\ngame_dir = 'x'\n"));
        assert!(text.contains("[controls]\nmouse_sensitivity = 1.0\n"));
        let keymap = std::fs::read_to_string(dir.path().join("sub").join("keymap.txt")).unwrap();
        let mut again = Settings::default();
        assert!(again.apply_keymap(&keymap).is_empty());
        assert_eq!(again, m.controls);
        // Saving the same thing again changes no file.
        let before = std::fs::metadata(&settings).unwrap().modified().unwrap();
        save(&job).unwrap();
        assert_eq!(
            std::fs::metadata(&settings).unwrap().modified().unwrap(),
            before
        );
        assert!(!dir.path().join("victory.toml.tmp").exists());
    }

    #[test]
    fn a_hand_written_keymap_is_kept_as_a_backup_once() {
        let dir = tempfile::tempdir().unwrap();
        let keymap = dir.path().join("keymap.txt");
        std::fs::write(&keymap, "# mine\nSpace=X\n").unwrap();
        let job = |text: &str| SaveJob {
            settings_path: dir.path().join("victory.toml"),
            edits: Vec::new(),
            keymap_path: Some(keymap.clone()),
            keymap_text: text.to_string(),
        };
        save(&job(&format!("{KEYMAP_HEADER} (a)\n"))).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keymap.txt.bak")).unwrap(),
            "# mine\nSpace=X\n"
        );
        save(&job(&format!("{KEYMAP_HEADER} (b)\n"))).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keymap.txt.bak")).unwrap(),
            "# mine\nSpace=X\n"
        );
        assert!(std::fs::read_to_string(&keymap).unwrap().ends_with("(b)\n"));
    }

    #[test]
    fn a_settings_file_that_cannot_be_edited_still_lets_the_keymap_save() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("victory.toml");
        std::fs::write(&settings, "controls = { walk_speed = 0.5 }\n").unwrap();
        let m = Model {
            controls: Settings::default(),
            display: Display::default(),
            pace: PaceModel::default(),
            upscale: UpscaleModel::default(),
        };
        let job = SaveJob {
            settings_path: settings.clone(),
            edits: edits(&m),
            keymap_path: Some(dir.path().join("keymap.txt")),
            keymap_text: "x\n".into(),
        };
        let err = save(&job).unwrap_err();
        assert!(err.contains("victory.toml"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keymap.txt")).unwrap(),
            "x\n"
        );
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            "controls = { walk_speed = 0.5 }\n"
        );
    }
}
