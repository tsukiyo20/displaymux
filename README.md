# displaymux

Switch a monitor's input source from the command line or the menu bar, over
DDC/CI.

`displaymux` is a small, native tool for desks where one monitor is shared by more
than one computer. Instead of reaching for the monitor's buttons, pick the input
from a menu — or script it.

```
DELL U2720Q
────────────────────────────────
0x0f  DisplayPort 1          switch
0x10  DisplayPort 2   last sent here
0x11  HDMI 1                 switch
0x12  HDMI 2                 switch
────────────────────────────────
4 inputs available
```

## Contents

- [Why another one](#why-another-one)
- [Platform support](#platform-support)
- [Building](#building)
- [Command line](#command-line)
- [Menu bar](#menu-bar)
- [How it works](#how-it-works)
- [Limitations](#limitations)
- [License](#license)

## Why another one

Most DDC/CI utilities either stop at one platform or expose only brightness and
contrast. displaymux focuses on **input selection**, the one thing you need when
two machines share a display, and it tries to be honest about what it cannot
know:

- **A switch is treated as a one-way command.** The UI never claims to know which
  input the display is on — see [Limitations](#limitations).
- **DDC is driven defensively.** On macOS a write can leave the I2C channel
  unusable for the remainder of the process, so writes run in a throwaway
  subprocess while reads share one long-lived process.
- **The GUI is native.** [GPUI](https://gpui.rs), no webview and no bundled
  browser runtime; roughly a 4 MB binary.

## Platform support

| Platform | Backend | Status |
|---|---|---|
| macOS (Apple Silicon) | private `CoreDisplay` / `IOAVService` via [`ddc-macos`](https://github.com/haimgel/ddc-macos-rs) | Works |
| macOS (Intel) | `IOFramebufferI2CInterface` | Untested |
| Windows | Monitor Configuration API (`dxva2`) | Compiles, untested on hardware |
| Linux | `ddcutil` / `/dev/i2c-*` | Not implemented |

The Windows path needs no platform-specific code: `ddc-hi` already routes to
`ddc-winapi`, which covers enumeration, `GetVCPFeatureAndVCPFeatureReply`,
`SetVCPFeature` and `GetCapabilitiesString`. The CLI and helper should build and
run there as they are.

> On macOS, DDC is unavailable through the built-in HDMI port on some models. A
> monitor attached over USB-C or DisplayPort is the reliable case.

## Building

Rust 1.99 or newer, pinned in `rust-toolchain.toml`.

```bash
# Command line tool and its DDC helper
cargo build --release --bin dmux --bin dmux-ddc-helper

# GUI and menu bar item
cargo build --release --features gui --bin dmux-gui
```

`dmux-ddc-helper` must sit next to `dmux`, be on `PATH`, or be pointed at with
`DMUX_DDC_HELPER`. Building both binaries into one target directory satisfies
that automatically.

The GUI sits behind the `gui` feature so the CLI stays cheap to build: GPUI brings
a large dependency tree that the DDC core does not need.

### A note on the GPUI dependency

The GUI builds against GPUI 1.24 through the community release train
`gpui-unofficial`, pinned with `=` because GPUI's minor version tracks Zed's
release tags and carries no compatibility promise. Two features are load-bearing,
and both live on `gpui_platform`:

```toml
gpui_platform = { package = "gpui-platform-gpui-unofficial", version = "=1.24.1-pre",
                  default-features = false, features = [
    "font-kit",         # without it macOS renders no text at all
    "runtime_shaders",  # build without a full Xcode install
] }
```

- **`font-kit`** belongs to GPUI's default feature set. Because we set
  `default-features = false` (to reach `runtime_shaders`), it must be listed
  explicitly. Omit it and the macOS backend falls back to `NoopTextSystem`:
  layout, styling and font metrics all behave normally, and **not one glyph is
  painted**. Zed hit the same trap in
  [zed-industries/zed#59241](https://github.com/zed-industries/zed/pull/59241).
  A quick check: `cargo tree -e normal | grep -c font-kit` returning `0` is the bug.
- **`runtime_shaders`** has GPUI compile its Metal shaders at run time. Without it
  the build calls `xcrun -sdk macosx metal`, which ships only with a full Xcode
  installation; with it, the Command Line Tools suffice.

## Command line

```
dmux list                              list DDC-capable displays
dmux status [--index=N] [--caps]       ports, capabilities and latency
dmux inputs [--index=N]                just the switchable inputs
dmux set <input> [--index=N]           switch input
dmux toggle [--index=N] [--to=<input>] switch to the next, or the given, input
dmux hold <input> [--index=N]          re-assert an input until it sticks
dmux bright <0-100> [--index=N]        set brightness
```

Inputs may be given as a VCP value (`0x11`, `17`) or by name with separators
ignored: `hdmi1`, `HDMI-1`, `dp2`, `displayport2`, `usbc1`.

```console
$ dmux status
display   : [0] DELL U2720Q
input     : 0x10 (DisplayPort 2)
switchable:
    0x0f     DisplayPort 1
  * 0x10     DisplayPort 2
    0x11     HDMI 1
    0x12     HDMI 2
latency   : 126 ms

$ dmux set hdmi1
switching [0] to 0x11 (HDMI 1) ...
result    : switch sent (monitor moved off this host, so no readback expected)
```

`dmux hold` re-sends the request on an interval, which helps when the monitor
would otherwise fall back to whichever input still carries a signal — for
example while the other machine is shutting down.

## Menu bar

Running `dmux-gui` installs a menu bar item. Closing its window does not quit the
app: the menu is the primary interface, and `Quit dmux` is how you leave.

The rows are the inputs the monitor advertises. A row marked `last sent here`
records a command this app issued; it is not a reading of the monitor.

Devices attached behind the monitor's USB hub are used to tell whether the
display is currently routed to this machine. That wiring is desk-specific, so it
is opt-in:

```bash
export DMUX_DEVICE_HINTS="<substring>,<substring>"   # devices behind the monitor hub
export DMUX_HUB_HINT="<substring>"                   # the monitor's hub itself
```

Without them the app reports the display state as unknown rather than guessing.

## How it works

```
dmux-gui ─┐
          ├─→ displaymux::ddc ─→ dmux-ddc-helper ─→ ddc-hi ─→ platform API
dmux    ──┘        │
                   └─ timeouts, retries, switch verdict
                   displaymux::vcp ── VCP values and capabilities parsing
```

| Module | Responsibility |
|---|---|
| `vcp` | MCCS value naming, capabilities parsing, input specifier resolution |
| `ddc` | Subprocess management, timeouts, retries, and the switch verdict |
| `session` | Whether the display is routed here, from USB device presence |
| `state` | The input this app last sent, persisted per display |
| `tray` | Menu bar item and menu |

`dmux-ddc-helper` performs exactly one DDC operation per invocation and exits.
That is not incidental: on macOS, issuing a write leaves the channel unusable for
further transactions from the same process, and a fresh process is the only
recovery. Reads are safe to share, so they use a persistent `serve` mode instead,
which also avoids paying process startup and device enumeration on every refresh.

The helper warms the channel up before its handshake, because the first
transaction on a freshly created handle reliably fails.

## Limitations

**A switch cannot be undone from the machine that issued it.** Once the display
moves to another computer, that computer drives it — and if the keyboard and
mouse are attached to the monitor's hub, they move with it. No hotkey can bring
the display back; the monitor's own input menu is the way. Treat a switch as a
deliberate, one-way action.

What the tool therefore does *not* do:

- It does not report which input the display is on. Some monitors return a
  constant value for VCP `0x60` regardless of the selected input, so the readback
  is not evidence. A wrong answer would be worse than none: it would aim a
  "switch back" command at the other computer.
- It does not verify that a switch happened. It reports that the monitor
  acknowledged the command, which is all the host can honestly know.
- It does not switch automatically.

If a switch lands on an input with no signal, some monitors hold that input and
display "no signal" rather than falling back. `tools/recover-input.sh` re-asserts
a given input from fresh processes, but it can only help while the DDC channel is
still alive.

## License

MIT — see [LICENSE](LICENSE).
