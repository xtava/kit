# Kit settings

Kit settings are operator preferences that change how a tool behaves or renders. They are not
deployment plans, CDP profiles and flows, saved history, or runtime state. Each tool owns its typed
settings model and stores it in its existing XDG-scoped TOML document:

```text
$XDG_CONFIG_HOME/kit/<tool>.toml
```

The settings editor is one shared, embeddable TUI component over those independent documents. This
preserves Kit's module boundary: a tool defines and validates its own settings; the shared framework
persists values; the shared TUI renders typed fields and returns semantic edits. Framework and TUI
code never import a tool module.

## Data model

An editable tool contributes a `SettingsSection` through its normal `Tool` registration. Opening a section produces its typed
`EditableSettings` model. The editor sees only presentation-safe field snapshots:

- a stable field identifier scoped to the section;
- a label and explanation;
- a typed field (`Toggle` with a boolean value, or a finite `Choice` with its current display label);
- semantic edits (`Activate`, `Previous`, `Next`, or `Reset`) that the typed model interprets.

Edits return to the tool-owned model immediately. Arbitrary strings and scalar values never cross
the editor boundary: the model applies the edit to its domain type and persists the corresponding
TOML item through `ConfigStore`. The document editor preserves unrelated keys, comments, whitespace,
and ordering, then publishes the updated file atomically. Invalid existing TOML is reported and
never replaced.

This is deliberately not reflection over arbitrary Serde structs. A field becomes editable only
when its owning tool gives it a human name, help text, control type, validation, and default.
Configuration that carries domain behavior instead of a preference keeps its own purpose-built UI.

## TUI flow

`kit settings` opens the complete editor. The left region selects a contributed tool section; the
right region selects and edits fields. Arrow keys follow rendered geometry, Tab cycles regions,
Enter or Space activates the selected control, `r` restores its default, and `q`/Esc exits. The
footer always shows the concrete file being edited and the last save or validation result.

Tools may embed the same component scoped to their own section. Render's `/configure` surface does
this today: closing it reloads Render's typed configuration from the same persisted document. There
is no tool-specific settings renderer or parallel configuration state.

## First integration: Diff line numbers

Diff owns a `line_numbers` choice with three states:

- `auto` preserves the presentation policy: line numbers appear in split panes and stay hidden in
  the full-width inline projection;
- `always` renders line-number gutters in every textual projection;
- `never` omits them everywhere.

The typed setting lives in `diff.toml`. Any future command-line override must use the normal Kit
precedence: explicit argument, then persisted setting, then typed default.

## Library decision

Kit uses `toml_edit` for lossless TOML mutation. It does not use a generic Ratatui form framework:
the available crates introduce parallel focus, input, configuration, and untyped value systems,
while Kit already owns those primitives. Ratatui remains the rendering substrate; Kit's shared TUI
module owns the settings interaction.
