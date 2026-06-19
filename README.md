# displayed

`displayed` is a small macOS display CLI for replacing the specific
BetterDisplay workflow of disconnecting the MacBook built-in display when a
target external monitor is present.

The current implementation uses `displayplacer` for visible display state and
for disabling detected displays. It uses a native private-API restore path for
bringing the built-in display back after it disappears from the normal
CoreGraphics/displayplacer online list.

## Install

### Normal local install

For regular use, install through Cargo:

```sh
cargo install --path . --force
```

This installs `displayed` under Cargo's bin directory, normally
`~/.cargo/bin/displayed`. It does not require `sudo` when Rust is installed for
the current user.

### Development symlink

For local development, build the release binary and symlink it into a user-local
PATH directory:

```sh
cargo build --release
ln -sfn "$PWD/target/release/displayed" ~/.local/bin/displayed
```

This keeps the installed command pointed at the repo's release build, so a later
`cargo build --release` updates the command immediately. On macOS, `~/.local/bin`
is not always in PATH by default; use this option only if your shell already
includes it or you explicitly add it.

### System-wide install

For multi-user/system-wide use:

```sh
cargo build --release
sudo cp target/release/displayed /usr/local/bin/displayed
```

This changes a shared system path and should not be the default for personal
development.

After installation, run:

```sh
displayed
```

Running without arguments opens interactive mode.

Runtime state is stored in `~/Library/Application Support/displayed/`, not in
the current working directory.

## Uninstall

Remove the command according to how it was installed:

```sh
cargo uninstall displayed
rm ~/.local/bin/displayed
sudo rm /usr/local/bin/displayed
```

## Commands

```sh
cargo run --
cargo run -- list
cargo run -- snapshot --label before
cargo run -- disable-internal --when-serial s123456 --dry-run
cargo run -- enable-internal --id 37D8832A-2D66-02CA-B9F7-8F30A301B230 --dry-run
cargo run -- restore-internal --display-id 1 --dry-run
cargo run -- restore-display --display-id 2 --dry-run
cargo run -- redetect-displays --dry-run
cargo run -- safe-reset --dry-run
cargo run -- targets
cargo run -- guard --dry-run
cargo run -- install-guard --dry-run
cargo run -- uninstall-guard --dry-run
cargo run -- watch --dry-run
cargo run -- watch --target-serial s123456 --interval 60 --dry-run
```

Remove `--dry-run` only after the generated `displayplacer` command looks right.
When the built-in display is already disconnected, `watch` should stay quiet
instead of treating the remaining external display as an error.

## Interactive Mode

```sh
displayed
```

Interactive mode shows the current detected displays, including whether the
built-in display is currently visible. It uses a keyboard TUI:

- `up/down` or `j/k`: select a detected display.
- `space` or `enter`: toggle the selected display. If macOS removes the disabled
  display from the live display list, `displayed` keeps it as a remembered `off`
  row and uses native restore when you toggle it back on.
- `i`: toggle the internal display. If the internal display is absent, this runs
  the native restore path.
- `a`: save/remove the selected external display as an auto target.
- `s`: safe reset.
- `r`: refresh.
- `q`: quit.

Auto targets are stored under
`~/Library/Application Support/displayed/targets` using monitor identity data:
serial ID first, persistent ID and contextual display ID as fallbacks. Saved
auto targets are merged into the interactive list even when disconnected, so you
can select a disconnected target and press `a` to remove it. Display list numbers
are only for current UI selection and are not used as the watch identity.

While interactive mode is open, saved auto targets are always watched. If a
saved target is connected, `displayed` automatically disables the internal
display. If no saved target is connected and the internal display is not
enabled, `displayed` requests the native internal restore path so unplugging the
external monitor does not leave the Mac without an active display. The watch
path registers a CoreGraphics display reconfiguration callback and also performs
a periodic reconcile. Interactive mode also applies this no-enabled-display
safety restore even when no auto target is saved. When CoreGraphics reports a
non-internal display remove/disable event, interactive mode runs the safety
restore before trusting the latest `displayplacer` state because that state can
be stale during hot unplug. A remembered internal display disable event is not a
restore trigger, so auto-disabling the built-in display cannot loop against the
safety restore. Interactive mode reconciles every 5 seconds; non-interactive
watch uses the requested interval, defaulting to 60 seconds.

## Internal Display Guard

`displayed guard` is a one-shot safety check. It never disables the internal
display and never rearranges displays. It only restores the internal display when
the MacBook lid is open, no enabled internal display is visible, and no enabled
external display is clearly visible. If the lid is closed, it stays idle so
normal clamshell use is not disturbed. If display state is unavailable while the
lid is open, it fails open by restoring the internal display.

```sh
displayed guard
```

When interactive `displayed` is open, that same process is the only long-running
watcher. It already watches CoreGraphics display changes and now also registers
for macOS system power notifications. On `SystemWillSleep`, it proactively
restores the internal display before allowing sleep, so an internal-off layout is
not carried across an eight-hour sleep after the external monitor is unplugged.
On wake, it runs the restore-only guard before refreshing the TUI state.

```sh
displayed
```

For crash/stuck recovery when the interactive process is not running or is not
responding, install the launchd one-shot guard:

```sh
displayed install-guard
```

This writes one user LaunchAgent:

- `~/Library/LaunchAgents/com.stargt.displayed.guard.plist`

It runs `displayed guard --reason launchd --quiet` at load and every 30 seconds.
It is not a kept-alive monitoring process; each run exits after the one-shot
check. This still gives recovery coverage if the interactive TUI or `watch`
exits, crashes, or gets stuck.

The plist bakes a `PATH` into `EnvironmentVariables` covering Homebrew
(`/opt/homebrew/bin`, `/usr/local/bin`), the install-time `PATH`, and standard
system directories. launchd otherwise runs agents with a minimal `PATH` that omits
Homebrew, so the guard could not find `displayplacer` and would misread display
state. Reinstall with `install-guard` after moving `displayplacer` or changing
shells so the baked-in `PATH` stays correct.

This is a plain user LaunchAgent, not a packaged macOS Login Item. It may not
appear in System Settings -> Login Items & Extensions. Manage it with the
`install-guard` and `uninstall-guard` commands, or by inspecting
`~/Library/LaunchAgents/com.stargt.displayed.guard.plist` directly.

Uninstall the launchd guard with:

```sh
displayed uninstall-guard
```

The launchd guard path is restore-only, so it preserves the intended auto-target
behavior: saved external targets may still disable the internal display when they
are definitely present, but a MacBook opened without a usable external display is
forced back to an enabled internal display. This cannot cover pre-login/FileVault
or OS/hardware failures before the user's LaunchAgents are allowed to run.

## Auto Targets

From interactive mode, select an external display and press `a`. Interactive
mode starts watching saved targets immediately. To run the same behavior without
the TUI, use:

```sh
displayed watch
```

Saved targets can be inspected or cleared:

```sh
displayed targets
displayed targets --clear
```

## Safe Reset

```sh
displayed safe-reset
```

Safe reset only performs enabling/re-detection work:

1. `SLSDetectDisplays()`
2. `CGSConfigureDisplayEnabled(display_id:<remembered-display-id>, enabled:true)`
   for every remembered contextual display ID
3. `displayplacer "id:<detected-disabled-display> enabled:true"` for any
   detected disabled displays

It does not intentionally disable or rearrange displays.

## Investigation Protocol

The useful comparison is not just display brightness. We need the before/after
state of CoreGraphics, IOKit, `displayplacer`, and BetterDisplay preferences.

1. Start with BetterDisplay running and the external monitor connected.
2. Capture the pre-change state:

   ```sh
   cargo run -- snapshot --label before-internal-disconnect
   ```

3. In BetterDisplay, apply the setting that disconnects only the built-in
   MacBook display.
4. Capture the post-change state:

   ```sh
   cargo run -- snapshot --label after-internal-disconnect
   ```

5. If you re-enable the built-in display, capture that too:

   ```sh
   cargo run -- snapshot --label after-internal-reconnect
   ```

6. Compare the files:

   ```sh
   diff -u snapshots/<before>.txt snapshots/<after>.txt
   ```

## Privacy

Snapshot files can include monitor serial numbers, EDID data, and BetterDisplay
license metadata. `snapshots/` is intentionally gitignored.

## Current Finding

User-run captures from the visible terminal confirmed the BetterDisplay state
transition:

- Before disconnecting the built-in display:
  - Built-in `Color LCD` was present in `displayplacer list` as a MacBook built
    in screen, enabled, and main at `origin:(0,0)`.
  - External `LG FHD` was present, enabled, and positioned at `origin:(-66,-1080)`.
- After BetterDisplay disconnected the built-in display:
  - Built-in `Color LCD` disappeared from the CoreGraphics/displayplacer online
    list.
  - External `LG FHD` remained enabled and became the main display at
    `origin:(0,0)`.
  - BetterDisplay preferences changed `appDisconnected@Display:2` from false to
    true and `connected@Display:2` from true to false.

This means disabling the built-in display can use the same surface that
`displayplacer` already exposes:

```sh
displayplacer "id:<built-in-persistent-id> enabled:false"
```

For the captured setup, the external serial was `s673744`. The direct watch
form remains available:

```sh
cargo run -- watch --target-serial s673744 --interval 60 --dry-run
```

The preferred flow is to run `displayed`, select the external display, press
`a`, then use `displayed watch` or press `w` inside the TUI. That stores monitor
identity information instead of relying on the current display list index.

## Restore Built-In Display

The BetterDisplay-free restore candidate is a native private-API sequence:

```sh
SLSDetectDisplays()
CGSConfigureDisplayEnabled(display_id:<built-in-display-id>, enabled:true)
```

For the captured setup, the built-in display's display ID/contextual ID was `1`:

```sh
cargo run -- restore-internal --display-id 1
```

The same native restore primitive can restore any remembered display ID. For the
captured external display, the contextual display ID was `2`:

```sh
cargo run -- restore-display --display-id 2
```

If the built-in display is currently visible, running `list` first records its
contextual display ID in `~/Library/Application Support/displayed/state`:

```sh
cargo run -- list
cargo run -- restore-internal
```

`redetect-displays` runs only the SkyLight rediscovery call:

```sh
cargo run -- redetect-displays
```

The older `displayplacer` restore attempt is still available via
`enable-internal`, but it can fail with `Unable to find screen ...` once the
built-in display is fully absent from the CoreGraphics/displayplacer online
list:

```sh
cargo run -- enable-internal --id 37D8832A-2D66-02CA-B9F7-8F30A301B230
```

This native restore path is experimental. It is based on BetterDisplay's local
binary imports and disassembly: BetterDisplay imports `SLSDetectDisplays`,
`CGSConfigureDisplayEnabled`, and calls `SLSDetectDisplays()` with no arguments
inside its reconnect flow.

One caveat: Codex-run processes in this environment still saw an empty
CoreGraphics display list, while user-run terminal commands saw the real display
state. Do final validation from the interactive terminal that is visibly attached
to the monitor.
