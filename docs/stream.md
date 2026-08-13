# Stream Slot on macOS

Stream Slot turns the BetterDisplay `TV` virtual display into one reversible parking spot for a
window. Focus a window and press **Cmd+Shift+M**—no terminal is needed after setup.

## Set up once

Install BetterDisplay, Sunshine, and Karabiner-Elements, then install Kit's managed Karabiner rule:

```bash
kit stream shortcut install
kit stream shortcut status
```

Installation may ask for Accessibility access. Enable **Kit** (or the terminal that launched it) in
**System Settings → Privacy & Security → Accessibility**. Moonlight remains the client: open and
connect it normally from the device where you want to see the `TV` display.

Kit preserves a one-time copy of the original Karabiner configuration at
`~/.config/karabiner/karabiner.json.kit-stream-backup`. Removal deletes only Kit's managed rule.

Karabiner stores the absolute path to the current Kit binary. If that binary moves, remove and
reinstall the rule:

```bash
kit stream shortcut remove
kit stream shortcut install
```

## Daily use

- **Send:** focus a window and press **Cmd+Shift+M**. Kit connects the `TV` display, remembers the
  window's original frame, moves it to the display, targets Sunshine there, and starts Sunshine if
  needed.
- **Recall:** press **Cmd+Shift+M** again while the streamed window is focused. It returns to its
  original display, position, and size.
- **Switch:** focus a different window and press **Cmd+Shift+M**. Kit recalls the previous window
  before sending the new one, so the display remains a single predictable slot.

The same action is available from the terminal through `kit stream toggle`. Karabiner writes the
latest result or error to `~/Library/Logs/kit-stream-shortcut.log`.

## Dashboard

Run `kit stream` in an interactive terminal to open the existing Stream dashboard. It shows the
slot, virtual-display, Sunshine, and shortcut state and provides actions to:

- recall the active streamed window without moving the terminal itself;
- recover an interrupted slot;
- install the global shortcut;
- refresh status or open the command palette. Status also refreshes automatically.

Use the arrow keys and **Enter** (or click) to choose an action. Direct keys are **s** for
recall, **e** for recovery, **i** to install the shortcut, **r** to refresh, **Ctrl+P** to search
actions, and **q** to quit. Send or switch with **Cmd+Shift+M** while the target app is focused.
Closing the dashboard does not recall an active slot; the state is intentionally durable.

For a quick non-interactive check:

```bash
kit stream status
```

## Recovery

If Kit, Sunshine, or the terminal exits during a move, run:

```bash
kit stream recover
```

Recovery uses the saved ownership record to restore the window's original frame, restore
Sunshine's previous `output_name`, stop only the Sunshine service Kit started, and disconnect the
`TV` display only when Kit connected it. Kit never kills an unrelated Sunshine process or deletes
an existing virtual display. If Sunshine is already running for another display, Kit leaves it
untouched and asks you to stop or reconfigure it instead of restarting it.

## Limits

- Exit macOS native full screen before sending a window. Native full-screen windows cannot be
  moved safely through Accessibility.
- The app must expose a normal focused window through macOS Accessibility, and it must remain open
  for reliable recall or recovery.
- Stream Slot holds one window at a time and currently targets the BetterDisplay display named
  `TV`. A newly created display is 1920×1080.
- There is no automatic idle recall or stop yet. Press the shortcut again or run
  `kit stream recover` when you are finished.
- The global shortcut requires Karabiner-Elements. `kit stream toggle` and the dashboard remain
  available without it.
- This window-slot workflow is macOS-specific. Linux `kit stream` continues to provide the
  Hyprland/Sunshine host inspection and setup control plane.
