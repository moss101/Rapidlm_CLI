# Themes and key bindings

Both are set in your own `config.toml` (the RapidLM home), never in a project: a checked-in file cannot restyle or
rebind someone's terminal. A setting that does not parse is dropped on its own and reported when the session
starts; the rest still apply. `/keys` shows what is in effect.

## Themes

```toml
[ui.theme]
name = "dark"                  # default, dark, light, mono

[ui.theme.styles]              # optional per-role overrides
status = "black on cyan"
error  = "bold #ff5555"
tool   = "dim"
```

`default` styles nothing, so the screen is exactly what it was without a theme.

| Role | What it styles |
|---|---|
| `status` | the status bar |
| `composer` | the prompt being typed |
| `user` | lines you sent |
| `assistant` | the model's words |
| `tool` | tool activity |
| `evidence` | evidence and verification output |
| `error` | failures |
| `system` | notices and command output |
| `modal` | an approval or protocol-error modal |
| `sidebar` | a side panel |

A style is written the way you would say it: attributes `bold dim italic underline reverse`; colours as one of
`black red green yellow blue magenta cyan white` (with `bright-` for the bright sixteen), a number `0`-`255`, or
`#rrggbb`; and `on` before the background. `bold underline #ff8800`, `black on cyan`, `dim bright-black`, `on 238`.
Two foreground colours, a stray `on`, or an unknown word is an error for that entry.

Colour is off when `NO_COLOR` is set or `TERM=dumb`: the frame is then byte-for-byte the unstyled one. `#rrggbb`
uses 24-bit colour when `COLORTERM` is `truecolor` or `24bit`, and the nearest of the 256 otherwise. Every styled
run is reset before its row ends, so no style leaks into the next row or the terminal after exit.

Styling is applied after the screen is laid out: a theme cannot move text, change what is shown, or hide anything.

## Key bindings

```toml
[ui.keys]
stash       = "ctrl-x"
toggle_plan = ["shift-tab", "alt-p"]
page_up     = "alt-up"
background  = []               # unbind
```

| Action | Default | Does |
|---|---|---|
| `interrupt` | `ctrl-c` | stop the running turn; twice, leave |
| `stash` | `ctrl-s` | set the draft aside, or bring it back |
| `background` | `ctrl-b` | move the foreground command to the background |
| `complete` | `tab` | complete the `@path` or `/command`; on an empty prompt, switch plan mode |
| `toggle_plan` | `shift-tab` | switch between plan and build |
| `page_up` / `page_down` | `pageup` / `pagedown` | scroll the transcript |

A key is optional modifiers (`ctrl-`, `alt-`, `shift-` for tab only) and a key: a single character, `tab`, `esc`,
`pageup`, `pagedown`, `home`, `end`, `up`, `down`, `left`, `right`, or `f1`-`f12`. A value is one key or a list;
an empty list unbinds the action.

Some bindings are refused, with the reason, because they would break the terminal:

- a bare letter (it is how you type); `enter`, `backspace` and `space`;
- `ctrl-i`, `ctrl-m`, `ctrl-j`, `ctrl-h` and `ctrl-[`, which a terminal cannot tell from Tab, Enter, Backspace and
  Escape;
- a key already bound to another action.

An entry with any bad key is dropped whole rather than half applied. `interrupt` always keeps `ctrl-c` as well as
whatever you choose, so a runaway turn can always be stopped.
