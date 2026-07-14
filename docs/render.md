# `kit render`

`kit render` is a full-screen Markdown reader with the same bottom-prompt interaction model as
`kit cdp -i`. Open a file directly:

```bash
kit render README.md
```

Nord is the default theme. Override it for one launch with a built-in name or a custom TOML file:

```bash
kit render --theme terminal README.md
kit render --theme ~/.config/kit/my-render-theme.toml README.md
```

Or start without a path and fuzzy-search Markdown files under the current directory:

```bash
kit render
```

The catalog includes `.md`, `.markdown`, `.mdown`, `.mkd`, and `.mdx` files. Git-ignored Markdown
is shown by default and visibly labeled `ignored`, so private plans remain searchable without
changing the repository's ignore rules. Use `/configure` to show or hide those results; the setting
is persisted in `~/.config/kit/render.toml`. Direct paths may point outside the current directory and
do not need one of those extensions.

## Controls

- Type to fuzzy-search basenames and relative paths.
- Type `/` to discover slash commands; `/configure` opens Markdown discovery settings.
- Type `/theme` to select from available themes, or `/theme <name-or-path>` to apply one directly.
- `Tab` / `Shift-Tab` engages and cycles suggestions.
- `Up` / `Down` cycles suggestions, or scrolls one line when the prompt is empty.
- `Enter` opens the engaged suggestion, an exact path, or the only fuzzy match.
- `Right` completes the engaged suggestion or inline path ghost.
- `PageUp` / `PageDown` scrolls one viewport.
- `Home` / `End` jumps to the beginning or end when the prompt is empty.
- The mouse wheel scrolls the document.
- `Ctrl-U` clears the file prompt.
- `Esc` disengages suggestions, clears the prompt, then exits.
- `Ctrl-C` / `Ctrl-D` exits immediately.

Inside `/configure`, `Enter` or `Space` toggles Git-ignored Markdown, `T` cycles the built-in
themes, and `Esc` returns to the viewer. The header shows both the active catalog size and the
number of ignored Markdown files found. Configuration changes never modify `.gitignore` or other
project files.

Custom themes inherit from a built-in base and override only the roles you name:

```toml
base = "nord"

[colors]
accent = "#88c0d0"
text = "#d8dee9"
surface = "transparent"
border = "#4c566a"
code_background = "#3b4252"
```

Put reusable custom theme files in `~/.config/kit/themes/*.toml` to make them appear in the
`/theme` picker. A theme at any other path can still be loaded directly; the currently selected
custom theme also appears in the picker while that file exists.

Every color role accepts `#RRGGBB`, `reset`/`transparent`, or a named terminal color. Available
roles are `background`, `surface`, `border`, `text`, `text_strong`, `text_muted`, `accent`,
`accent_alt`, `info`, `focus`, `warning`, `selection`, `danger`, `attention`, `success`, `special`,
and `code_background`. A CLI `--theme` is session-only; `/theme` and `T` persist to
`~/.config/kit/render.toml`.

Markdown is parsed as CommonMark with GitHub-oriented extensions and wrapped to the live terminal
width. Source delimiters such as heading hashes, emphasis stars, task brackets, and code fences are
removed. The preview lays out headings, nested ordered and unordered lists, task states,
blockquotes and GFM alerts, horizontal rules, links, image descriptions, tables, footnotes,
definition lists, metadata, inline and display math, and inline or fenced code. Recognized fenced
code languages receive syntax highlighting; unknown languages remain readable styled code. Raw
HTML is shown as inert text, remote images are not fetched, and links are displayed rather than
opened automatically.

The reusable Markdown renderer lives in `src/tui/markdown.rs`, and the reusable suggestion menu
lives in `src/tui/suggestions.rs`. CDP, record, and render each keep their own candidate-generation
and submission semantics.
