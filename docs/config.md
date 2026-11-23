# Configuration

Code: `src/config.rs` (file, parsing), `src/tui/keys.rs` (key notation, actions, keymap),
`src/config.default.ron` (defaults, compiled in).

`config.ron` in the config dir (`~/Library/Application Support/hum/`, Linux `$XDG_CONFIG_HOME/hum/`),
in [RON](https://github.com/ron-rs/ron). Optional, as is every field; unknown fields are errors.
Only the TUI reads it, at startup (before login): a bad file fails `hum` with its line and column.
`Option` fields take the bare value (`timeout: 500`), `None` or `Some(500)`.

```ron
(
    keymap: (
        mode: Merge,     // or Replace
        timeout: 1000,   // ms; None waits forever
        keys: {
            "<Space>": Toggle(Play),
            "L": Set(Position(Relative(10))),
            "+": Set(Volume(Relative(5))),
            "gp": Play(Uri("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M")),
            "<C-n>": [Next, Set(Play(On))],
            "x": Nop,
        },
    ),
)
```

## Keymap

- `mode`: `Merge` (default) binds `keys` over the defaults in `src/config.default.ron`; `Replace`
  binds only `keys`. A key bound to `Nop` or `[]` is unbound.
- `keys`: key sequence → an action or a list of actions run in order.
- `timeout`: like vim's `timeoutlen`. While the keys typed so far are the start of a longer
  sequence, hum waits for the next key; after `timeout` ms (never with `None`) it runs what those
  keys are bound to on their own, if anything. A key that continues no sequence runs the keys before
  it on their own (if bound), then is looked up alone. `Esc` cancels a pending sequence.
- One global map: actions depend on the focused panel (`Down` moves in the focused panel, scrolls
  lyrics; `Unqueue` only acts with the queue focused). The search box, Connect picker and help
  overlay keep their own fixed keys.
- The help overlay (`?`) lists the bindings in effect, grouped by action.

### Key notation

Vim's: plain characters stand for themselves (`gg`, `L`, `+`); special keys and modifiers in angle
brackets: `<C-x>` Ctrl, `<A-x>`/`<M-x>` Alt, `<S-x>` Shift, `<D-x>` Super, combined like `<C-A-x>`.
Names (case-insensitive): `Space`, `Tab` (`<S-Tab>` back-tab), `CR`/`Enter`/`Return`, `Esc`,
`BS`/`Backspace`, `Del`/`Delete`, `Insert`, `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `PageUp`,
`PageDown`, `F1`–`F24`, `lt` (`<`), `gt` (`>`), `Bar` (`|`), `Bslash` (`\`).
`<S-a>` is `A`. `<C-S-d>` (same as `<C-D>`) is Ctrl+Shift+d: hum enables the kitty keyboard
protocol, and only terminals that support it (kitty, foot, WezTerm, Ghostty, ...) send it apart
from `<C-d>`; elsewhere Ctrl+Shift+letter arrives as Ctrl+letter.

### Actions

Like the CLI (`hum <subcommand>`):

| Action | |
|---|---|
| `Play(Uri("uri"))` | Play a URI or open.spotify.com link: a track alone, anything else as a context |
| `Play(Selected)`, `Play(Page)` | Play the selected item without opening it; play the open page from the start |
| `Queue(Uri("uri") \| Selected)` | Add a track/episode to the queue |
| `Next`, `Prev` | |
| `Toggle(Play \| Shuffle \| Repeat \| Mute)` | As `hum toggle`; `Repeat` is off ↔ on (context) |
| `Toggle(Like(Playing \| Selected))` | Like/unlike the playing track (`hum toggle like`) or the selected item |
| `Cycle(Repeat)` | As `hum cycle repeat`: off → on (context) → track |
| `Set(Play(On \| Off))`, `Set(Shuffle(..))`, `Set(Mute(..))` | As `hum set` |
| `Set(Like(Playing \| Selected, On \| Off))` | Like/unlike the playing track (`hum set like`) or the selected item |
| `Set(Repeat(Off \| On \| Track))` | |
| `Set(Volume(Set(N) \| Relative(±N)))` | Percent, absolute or relative |
| `Set(Position(Set(N) \| Relative(±N)))` | Seconds, absolute or relative |

TUI only:

| Action | |
|---|---|
| `Quit`, `Kill` | Quit; quit and stop the daemon (`hum kill`) |
| `Select(Up \| Down \| Left \| Right \| First \| Last)` | Move the cursor in the focused panel; `First`/`Last` go to its first/last item (only what is loaded); lyrics scroll |
| `Focus(Next \| Prev)` | Cycle focus library → main → queue |
| `Back` | Close lyrics, cancel loading, go back |
| `Open` | Open the selected playlist/album/artist, play a track |
| `Unqueue`, `Move(Up \| Down)` | Queue focused: remove / move the selected track |
| `Toggle(Lyrics \| Queue \| Library)` | Lyrics view; now-playing panel; library panel |
| `Share(Selected \| Playing \| Page)` | Copy the open.spotify.com link of the selected item, playing track or open page to the clipboard |
| `Exec(Selected \| Playing \| Page, [args])` | Run a command on the same targets as `Share`; see below |
| `Search`, `Home`, `Help`, `Devices` | |
| `Nop` | Nothing (unbinds under `Merge`) |

### Exec

`Exec(target, args)` runs `args` as an argv (no shell). The long form
`Exec(target, (args: [...], cwd: "...", env: {"NAME": value}))` also sets the working directory
(default: hum's own; a leading `~` is the home directory) and adds variables to the inherited environment; unknown fields are an error.
Each argv element and env value is a string or one of the target's values:

| Value | |
|---|---|
| `Uri` | `spotify:<kind>:<id>` |
| `Link` | `https://open.spotify.com/<kind>/<id>` |
| `Kind` | `<kind>` (`track`, `album`, ...) |
| `Id` | `<id>` |

`Link`, `Kind` and `Id` need a `spotify:<kind>:<id>` uri: Liked Songs, `spotify:list:…` pages and
local files have none and the action only reports it. The TUI is suspended while the command runs
and owns the terminal; on a non-zero exit or a spawn error it waits for Enter, then the error is
shown in the status line. A literal string equal to `"Uri"`, `"Link"`, `"Kind"` or `"Id"` is taken
as that value. Nothing is bound by default.

```ron
"<C-o>": Exec(Selected, ["mpv", Link]),
"<C-y>": Exec(Playing, ["sh", "-c", "echo \"$0\" | wl-copy", Uri]),
"<C-d>": Exec(Selected, (args: ["./fetch.sh"], cwd: "/home/me/music", env: {"HUM_ID": Id})),
```
