# `kit render`

`kit render` is a full-screen source-code and Markdown reader with the same bottom-prompt
interaction model as `kit cdp -i`. Open a file directly:

```bash
kit render README.md
kit render src/main.rs
kit render src/components/App.tsx
```

Nord is the default theme. Override it for one launch with a built-in name or a custom TOML file:

```bash
kit render --theme terminal README.md
kit render --theme ~/.config/kit/my-render-theme.toml README.md
```

Or start without a path and fuzzy-search source and Markdown files under the current directory:

```bash
kit render
```

To exercise all supported Markdown elements and syntax highlighting, render the included showcase:

```bash
kit render examples/markdown-showcase.md
```

The catalog recognizes filenames, extensions, and first-line markers covered by Kit's bundled
syntax definitions, including Rust, TypeScript, TSX, JavaScript, JSX, Python, and many more.
Markdown extensions `.md`, `.markdown`, `.mdown`, `.mkd`, and `.mdx` retain rich document
rendering. Git-ignored supported files are shown by default and visibly labeled `ignored`; use
`/configure` to show or hide those results. Direct paths may point outside the current directory,
and any UTF-8 text file can be opened even when its extension is unknown; unknown formats use
readable plain-text styling.

## Controls

- Type to fuzzy-search basenames and relative paths.
- Type `/` to discover slash commands; `/configure` opens Render discovery settings.
- Type `/theme` to select from available themes, or `/theme <name-or-path>` to apply one directly.
- `Tab` / `Shift-Tab` engages and cycles suggestions.
- `Up` / `Down` cycles suggestions, or scrolls one line when the prompt is empty.
- `Enter` opens the engaged suggestion, an exact path, or the only fuzzy match.
- `Right` completes the engaged suggestion or inline path ghost.
- `PageUp` / `PageDown` scrolls one viewport.
- `Home` / `End` jumps to the beginning or end when the prompt is empty.
- The mouse wheel scrolls the document.
- `Ctrl-T` collapses or expands the table of contents when it is available.
- Click `[−]` / `[+]` in the contents control to collapse or expand it with the mouse.
- `Ctrl-U` clears the file prompt.
- `Esc` disengages suggestions, clears the prompt, then exits.
- `Ctrl-C` / `Ctrl-D` exits immediately.

Inside `/configure`, `Enter` or `Space` toggles Git-ignored files, `T` cycles the built-in themes,
and `Esc` returns to the viewer. Configuration changes never modify `.gitignore` or other project
files.

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

Source files retain their original lines and are highlighted from the full filename, extension, or
first-line shebang/mode marker. Highlighting keeps parser state across lines, so multiline strings
and comments remain correctly colored. Kit delegates tokenization and language grammars to
`syntect` and the `two-face` syntax collection; the active terminal palette controls the resulting
syntax colors.

The reusable Markdown renderer lives in `src/tui/markdown.rs`, whole-source syntax highlighting
lives in `src/tui/syntax.rs`, and the reusable suggestion menu lives in `src/tui/suggestions.rs`.
CDP, record, and render each keep their own candidate-generation and submission semantics.
