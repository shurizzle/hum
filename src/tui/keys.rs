//! Configurable key bindings: vim-style key notation, multi-key sequences, actions modelled on the CLI.

use std::{collections::HashMap, time::Duration};

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

use crate::{
    cli::{Cycle, Level, OnOff, RepeatArg},
    config,
};

/// `keymap` in `config.ron`.
/// Missing fields take the values in `config.default.ron`, `keys` none (the defaults merge in later).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    #[serde(default = "default_mode")]
    pub mode: Mode,
    /// Milliseconds an ambiguous sequence waits for the next key; `None` waits forever.
    #[serde(default = "default_timeout")]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub keys: HashMap<String, Binding>,
}

impl Default for Keys {
    fn default() -> Self {
        Keys { mode: default_mode(), timeout: default_timeout(), keys: HashMap::new() }
    }
}

fn default_mode() -> Mode {
    config::defaults().keymap.mode
}

fn default_timeout() -> Option<u64> {
    config::defaults().keymap.timeout
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
pub enum Mode {
    /// Over the defaults.
    Merge,
    /// Instead of the defaults.
    Replace,
}

/// One action or a list run in order; `Nop` or `[]` unbinds.
#[derive(Deserialize)]
#[serde(untagged, expecting = "an action (`Next`, `Set(Volume(Relative(5)))`, ...) or a list of actions")]
pub enum Binding {
    One(Action),
    Many(Vec<Action>),
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub enum Action {
    Nop,
    Quit,
    /// Quit and stop the daemon, like `hum kill`.
    Kill,
    /// Cycle focus library → main → queue.
    Focus(Step),
    Search,
    Home,
    Help,
    Devices,
    /// Move the cursor in the focused panel; lyrics scroll.
    Select(Dir),
    /// Close lyrics, cancel loading, go back.
    Back,
    /// Open the selected playlist/album/artist, play a track.
    Open,
    /// Queue focused: remove / move the selected track.
    Unqueue,
    Move(Vertical),
    // Like the CLI subcommands.
    Play(PlayTarget),
    Queue(QueueTarget),
    Next,
    Prev,
    Toggle(Toggle),
    Cycle(Cycle),
    Set(Set),
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum Toggle {
    Play,
    Shuffle,
    /// Off, or on (context) when off.
    Repeat,
    Like(LikeTarget),
    Mute,
    Lyrics,
    /// The now-playing panel.
    Queue,
    Library,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Next,
    Prev,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
    /// First / last item of the panel (vim `gg` / `G`).
    First,
    Last,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum Vertical {
    Up,
    Down,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub enum PlayTarget {
    /// A Spotify URI or open.spotify.com link: a track alone, anything else as a context.
    Uri(String),
    /// Without opening it.
    Selected,
    /// The open playlist/album/artist from the start.
    Page,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub enum QueueTarget {
    /// A track/episode URI or open.spotify.com link.
    Uri(String),
    Selected,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum LikeTarget {
    Selected,
    Playing,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub enum Set {
    Play(OnOff),
    Shuffle(OnOff),
    Repeat(RepeatArg),
    Like(LikeTarget, OnOff),
    Mute(OnOff),
    /// Percent.
    Volume(Level),
    /// Seconds.
    Position(Level),
}

/// A key press, normalized so that config keys and terminal events compare equal.
pub type Key = (KeyCode, KeyModifiers);

pub fn key(e: KeyEvent) -> Key {
    let mut mods = e.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER);
    let code = match e.code {
        // The case already says shift; terminals send Ctrl+letter the same either way.
        KeyCode::Char(c) => {
            mods.remove(KeyModifiers::SHIFT);
            KeyCode::Char(if mods.contains(KeyModifiers::CONTROL) { c.to_ascii_lowercase() } else { c })
        }
        KeyCode::BackTab => {
            mods.remove(KeyModifiers::SHIFT);
            KeyCode::BackTab
        }
        c => c,
    };
    (code, mods)
}

/// Vim notation: `gg`, `<C-c>`, `<S-Right>`, `<Space>`, `<lt>`.
pub fn parse(s: &str) -> Result<Vec<Key>> {
    let mut out = vec![];
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && let Some(end) = rest.find('>')
            && end > 1
        {
            out.push(special(&rest[1..end]).ok_or_else(|| anyhow::anyhow!("unknown key <{}> in {s:?}", &rest[1..end]))?);
            rest = &rest[end + 1..];
        } else {
            out.push(key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
            rest = &rest[c.len_utf8()..];
        }
    }
    if out.is_empty() {
        bail!("empty key");
    }
    Ok(out)
}

fn special(s: &str) -> Option<Key> {
    let mut mods = KeyModifiers::NONE;
    let mut name = s;
    while name.len() > 2 && name.as_bytes()[1] == b'-' {
        mods |= match name.as_bytes()[0].to_ascii_uppercase() {
            b'C' => KeyModifiers::CONTROL,
            b'A' | b'M' => KeyModifiers::ALT,
            b'S' => KeyModifiers::SHIFT,
            b'D' => KeyModifiers::SUPER,
            _ => return None,
        };
        name = &name[2..];
    }
    let mut chars = name.chars();
    let code = match (chars.next(), chars.next()) {
        (Some(c), None) if mods.contains(KeyModifiers::SHIFT) => KeyCode::Char(c.to_ascii_uppercase()),
        (Some(c), None) => KeyCode::Char(c),
        _ => match name.to_ascii_lowercase().as_str() {
            "space" => KeyCode::Char(' '),
            "lt" => KeyCode::Char('<'),
            "gt" => KeyCode::Char('>'),
            "bar" => KeyCode::Char('|'),
            "bslash" => KeyCode::Char('\\'),
            "tab" if mods.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
            "tab" => KeyCode::Tab,
            "cr" | "enter" | "return" => KeyCode::Enter,
            "esc" => KeyCode::Esc,
            "bs" | "backspace" => KeyCode::Backspace,
            "del" | "delete" => KeyCode::Delete,
            "insert" => KeyCode::Insert,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            f => KeyCode::F(f.strip_prefix('f')?.parse().ok().filter(|n| (1..=24).contains(n))?),
        },
    };
    Some(key(KeyEvent::new(code, mods)))
}

/// Sequences to actions, with the notation they were written in for the help.
pub struct Keymap {
    map: HashMap<Vec<Key>, (String, Vec<Action>)>,
    pub timeout: Option<Duration>,
}

pub enum Lookup<'a> {
    /// A longer sequence starts with these keys; `.0` is what they are bound to themselves.
    Prefix(Option<&'a [Action]>),
    Exact(&'a [Action]),
    None,
}

impl Keymap {
    pub fn new(user: &Keys) -> Result<Keymap> {
        let mut map = HashMap::new();
        let defaults = &config::defaults().keymap;
        let layers = if user.mode == Mode::Merge { vec![defaults, user] } else { vec![user] };
        for (notation, binding) in layers.into_iter().flat_map(|k| &k.keys) {
            let seq = parse(notation)?;
            let actions = match binding {
                Binding::One(a) => vec![a.clone()],
                Binding::Many(a) => a.clone(),
            };
            let actions: Vec<_> = actions.into_iter().filter(|a| *a != Action::Nop).collect();
            if actions.is_empty() {
                map.remove(&seq);
            } else {
                map.insert(seq, (notation.clone(), actions));
            }
        }
        Ok(Keymap { map, timeout: user.timeout.map(Duration::from_millis) })
    }

    pub fn lookup(&self, seq: &[Key]) -> Lookup<'_> {
        let exact = self.map.get(seq).map(|(_, a)| a.as_slice());
        if self.map.keys().any(|k| k.len() > seq.len() && k.starts_with(seq)) {
            Lookup::Prefix(exact)
        } else {
            exact.map_or(Lookup::None, Lookup::Exact)
        }
    }

    /// `(keys, action)` rows for the help overlay, one per distinct binding.
    pub fn help(&self) -> Vec<(String, String)> {
        let mut by_action: HashMap<String, Vec<&str>> = HashMap::new();
        for (notation, actions) in self.map.values() {
            let shown = if let [a] = &actions[..] { format!("{a:?}") } else { format!("{actions:?}") };
            by_action.entry(shown).or_default().push(notation);
        }
        let mut rows: Vec<_> = by_action
            .into_iter()
            .map(|(action, mut keys)| {
                keys.sort_by_key(|k| (k.len(), *k));
                (keys.join(" "), action)
            })
            .collect();
        rows.sort_by(|a, b| a.1.cmp(&b.1));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, mods: KeyModifiers) -> Key {
        (code, mods)
    }

    #[test]
    fn notation() {
        let none = KeyModifiers::NONE;
        assert_eq!(parse("gg").unwrap(), vec![k(KeyCode::Char('g'), none); 2]);
        assert_eq!(parse("<C-c>").unwrap(), vec![k(KeyCode::Char('c'), KeyModifiers::CONTROL)]);
        assert_eq!(parse("<S-a>").unwrap(), parse("A").unwrap());
        assert_eq!(parse("<S-Tab>").unwrap(), vec![k(KeyCode::BackTab, none)]);
        assert_eq!(parse("<S-Right>").unwrap(), vec![k(KeyCode::Right, KeyModifiers::SHIFT)]);
        assert_eq!(parse("<Space>x").unwrap(), vec![k(KeyCode::Char(' '), none), k(KeyCode::Char('x'), none)]);
        assert_eq!(parse("<").unwrap(), parse("<lt>").unwrap());
        assert_eq!(parse("<F12>").unwrap(), vec![k(KeyCode::F(12), none)]);
        assert!(parse("<Nope>").is_err());
        assert!(parse("").is_err());
        // What terminals send for Shift+L and Shift+Tab.
        assert_eq!(key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT)), parse("L").unwrap()[0]);
        assert_eq!(key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)), parse("<S-Tab>").unwrap()[0]);
    }

    #[test]
    fn merge_and_replace() {
        let user = config::parse(r#"(keymap: (keys: {"n": [Next, Set(Play(On))], "q": Nop, "z": Home, "zx": Quit}))"#).unwrap().keymap;
        let map = Keymap::new(&user).unwrap();
        assert!(matches!(map.lookup(&parse("n").unwrap()), Lookup::Exact([Action::Next, Action::Set(Set::Play(OnOff::On))])));
        assert!(matches!(map.lookup(&parse("q").unwrap()), Lookup::None));
        assert!(matches!(map.lookup(&parse("z").unwrap()), Lookup::Prefix(Some([Action::Home]))));
        assert!(matches!(map.lookup(&parse("j").unwrap()), Lookup::Exact([Action::Select(Dir::Down)])));

        // Unset fields come from config.default.ron.
        assert_eq!(Keymap::new(&config::Config::default().keymap).unwrap().timeout, config::defaults().keymap.timeout.map(Duration::from_millis));
        assert!(config::defaults().keymap.timeout.is_some());
        let user = config::parse("(keymap: (keys: {}))").unwrap().keymap;
        assert_eq!(user.timeout, config::defaults().keymap.timeout);

        let user = config::parse(r#"(keymap: (mode: Replace, timeout: None, keys: {"x": Quit}))"#).unwrap().keymap;
        let map = Keymap::new(&user).unwrap();
        assert!(matches!(map.lookup(&parse("j").unwrap()), Lookup::None));
        assert_eq!(map.timeout, None);

        assert!(config::parse(r#"(keymap: (keys: {"x": Set(Volume(Set(-5)))}))"#).is_err());
        assert!(config::parse(r#"(keymap: (keys: {"x": Nexxt}))"#).is_err());
    }
}
