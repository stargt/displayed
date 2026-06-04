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
safety restore even when no auto target is saved. The default reconcile interval
is 60 seconds, so idle CPU use stays low while display hot-plug events can still
be handled quickly.

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
