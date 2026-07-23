# Tmux Embedding Options for Prism

## Recommendation

Use **one outer tmux client and `switch-client`** as the smallest robust
architecture, provided Prism can make "run inside tmux" part of its launch
contract. Keep Prism in a dedicated dashboard session/window and switch that
same client to the selected agent pane. Return with `switch-client -l` (tmux's
default `Prefix L`) or a Prism-installed, documented return binding. Do not
suspend Ratatui or start a second attached client.

This is the only option that both avoids exposing the terminal's normal screen
and leaves resize, keyboard protocols, mouse, paste, copy mode, and terminal
escape handling entirely with tmux and the user's terminal emulator. tmux
documents that `attach-session` from inside tmux switches the current client;
`switch-client -t` may target a pane and changes its session, window, and pane,
while `switch-client -l` returns to the last session. [tmux manual: clients and
sessions][tmux-man]

The launch contract is the tradeoff: outside tmux, Prism would first need to
start/attach a dashboard session running Prism (with a recursion marker), and
agent sessions must use the same tmux server/socket. Prism also needs an
explicit return UX because normal `detach-client` detaches the outer client and
would reveal its parent shell. There is no child process whose exit means
"returned"; if Prism needs that state, observe the client with
`#{client_session}`/hooks or simply redraw when its pane becomes visible again.

If requiring an outer tmux client is unacceptable, the robust standalone
fallback is **an embedded `tui-term` + `vt100` screen backed by
`portable-pty`**. It is materially larger because Prism becomes part of the
terminal emulator: input encoding, terminal replies, mouse, paste, clipboard,
and parser compatibility all become Prism responsibilities.

This also fits Prism's accepted boundaries: [ADR 0001](../adr/0001-tmux-backed-agent-sessions.md)
makes tmux the only interactive runtime, and [ADR 0003](../adr/0003-ratatui-crossterm-tui-runtime.md)
keeps external-tool handoff outside the Ratatui rendering responsibility.

## Comparison

| Architecture | Resize | Input, mouse, paste | Escapes/rendering | Leave detection | Portability | Risk |
|---|---|---|---|---|---|---|
| 1. Raw PTY bridge to an attached tmux client | Exact PTY support: call `MasterPty::resize`; the crate updates kernel winsize and signals the child. | `take_writer` accepts raw bytes, so an exclusive byte-level stdin relay can preserve keys, mouse reports, and bracketed paste. Crossterm `Event`s are not raw bytes and must not be used for this path. | No parser: tmux output goes to the real terminal. Highest fidelity, but tmux/app `smcup`/`rmcup` and other global modes also reach it. Therefore it does **not strictly guarantee** that the normal screen is never selected during teardown. | `Child::wait`/`try_wait` reports when the attached tmux client exits after detach, server loss, or error. It does not distinguish causes without additional tmux state. | `portable-pty` is cross-platform and selects implementations at runtime, but tmux availability remains the practical platform limit. | **Medium.** Small transport, but exact stdin ownership, resize races, signal/error cleanup, and restoring terminal modes without a visible flash are subtle. |
| 2. Embedded terminal widget + PTY | Resize both `MasterPty` and `vt100::Screen`; the official `tui-term` example does both. | PTY writes are easy; encoding is not. `tui-term`'s example manually maps keys, leaves several keys as `todo!`, ignores mouse, and leaves paste unimplemented. `vt100::Screen` exposes application cursor/keypad, bracketed-paste, and mouse mode/encoding so Prism can encode correctly, but does not do it for Prism. | `vt100::Parser::process` builds screen state; `tui-term::PseudoTerminal` renders that screen into Ratatui. This isolates the outer screen. `vt100::Callbacks` also exposes unhandled sequences, clipboard, title, bell, and resize requests, showing the integration surface beyond cells. | Wait for the PTY child; as above, inspect tmux separately to classify detach versus failure. | `portable-pty` is cross-platform; `tui-term` currently supports only the `vt100` backend. tmux still limits useful targets. | **High.** Robust isolation, but Prism now owns a terminal emulator boundary and input protocol. `tui-term` calls its controller unstable and says persistent programs still require manual handling. |
| 3. tmux control mode | `refresh-client -C widthxheight` sizes a control client/window; without it, control clients do not affect pane size. | The stream accepts tmux commands, not raw terminal input. Keys require `send-keys` (including `-H` where useful), paste requires tmux buffers/commands, and mouse requires application-aware translation rather than transparent forwarding. | `%output` contains the pane's bytes with control characters octal-escaped, may be non-UTF-8, and uses tmux's `TERM`; Prism must unescape and emulate them. Critically, tmux-generated copy/choose-mode output is **not sent**, so an embedded view cannot match a normal tmux client. | Strong protocol signals: `%exit`, `%session-changed`, `%sessions-changed`, window/pane notifications, and guarded command responses. Flow control can pause and recover with `capture-pane`. | Text protocol works over SSH and avoids a local PTY, but still requires tmux and a terminal parser for display. | **High.** Excellent automation API, poor interactive-client replacement. It combines protocol parsing, terminal emulation, and input translation while remaining behaviorally incomplete. |
| 4. Same outer tmux client / `switch-client` | tmux owns client and pane sizing. No Prism resize bridge. | Native tmux path; keyboard, mouse, paste, copy mode, and user tmux bindings retain normal behavior. | tmux renders directly to its existing client; Prism parses nothing and its Ratatui alternate screen remains inside the dashboard pane. | Switching is immediate, not a blocking attach. Return is a second switch (`-l` or explicit target); observe client state only if Prism needs it. | Requires Prism and target sessions on one tmux server and makes tmux-hosted launch mandatory. | **Low** once the launch/return contract is accepted; otherwise it is unavailable rather than a fallback. |

## Library Findings

### `portable-pty`

`openpty` returns master/slave sides; the slave spawns the tmux client, while
`try_clone_reader` and the single `take_writer` carry output and input.
`MasterPty::resize` explicitly updates kernel winsize and generates a signal for
the child. `Child::try_wait` is nonblocking and `wait` blocks for exit. These are
all the primitives needed for a PTY bridge, but the crate does not parse terminal
output or encode high-level Crossterm events. [crate overview][portable-pty]
[master API][portable-master] [child API][portable-child]

For Prism, a raw bridge must pause Ratatui drawing and Crossterm event reads,
give one loop exclusive stdin/stdout ownership, propagate every resize, and
force a full terminal reinitialization/redraw afterward. Keeping Prism's
alternate-screen flag in memory is not sufficient isolation: raw child output
is intentionally allowed to change the physical terminal's screen and modes.

### `tui-term` and `vt100`

`tui-term` is a renderer, not a complete terminal session manager. Its
`PseudoTerminal` takes a screen reference and implements Ratatui `Widget`; the
only documented parser backend is `vt100`. [tui-term overview][tui-term]
[widget API][tui-term-widget]

`vt100` tracks cells, cursor, alternate screen, application cursor/keypad,
bracketed paste, and mouse protocol mode/encoding, and supports screen resize.
That is enough state to build correct input encoding, but there is no API that
turns a Crossterm key/mouse/paste event into the required bytes. The official
`tui-term` nested-shell example demonstrates this gap directly. [vt100 parser]
[vt100 screen] [nested-shell example][tui-term-example]

`termwiz` does not reduce this integration for a Ratatui embed: its `Surface` is
explicitly not connected to a terminal device, and its `Terminal` abstraction
targets the real console (`/dev/tty` or Windows console). It has rich typed key,
mouse, paste, and resize input, but adopting it would overlap Prism's existing
Crossterm/Ratatui runtime rather than supply a drop-in emulator widget.
[termwiz Surface][termwiz-surface] [termwiz terminal][termwiz-terminal]
[termwiz input][termwiz-input]

`alacritty_terminal` is the heavier full-emulator alternative. It supplies a
PTY layer, race-free child events, terminal modes/grid/damage/alternate screen,
and events for PTY replies, clipboard, and size requests. It has no Ratatui
widget, so Prism would still need an adapter from `renderable_content` to
Ratatui plus input encoding and event-loop integration. It is more capable than
`vt100`, but not the smallest change. [terminal state][alacritty-term]
[PTY API][alacritty-pty] [child events][alacritty-child]

### tmux control and client behavior

Control mode is designed for applications to control tmux with a text protocol.
Pane output is asynchronous and exact after octal unescaping, but output from
tmux's own modes is omitted. It offers explicit sizing, notifications, command
guards, subscriptions, and flow control; those strengths suit Prism's existing
automation/status surfaces, not a faithful interactive terminal.
[official control-mode guide][tmux-control]

Normal tmux clients already provide the behavior Prism needs: a client displays
one session, sessions persist independently, attaching from inside switches the
current client, and `switch-client` can target a pane or the last session. Reuse
that client instead of recreating it behind Ratatui.

## Decision Boundary

Choose option 4 if Prism may guarantee a tmux-hosted dashboard and can define a
return binding. Choose option 2 only if standalone launch is a hard requirement
and exact no-normal-screen isolation outweighs implementing/testing a terminal
frontend. Do not choose option 3 for interactive fidelity. Option 1 is a useful
prototype or best-effort handoff, but not the strict isolation solution.

[portable-pty]: https://docs.rs/portable-pty/0.9.0/portable_pty/
[portable-master]: https://docs.rs/portable-pty/0.9.0/portable_pty/trait.MasterPty.html
[portable-child]: https://docs.rs/portable-pty/0.9.0/portable_pty/trait.Child.html
[tui-term]: https://docs.rs/tui-term/0.3.4/tui_term/
[tui-term-widget]: https://docs.rs/tui-term/0.3.4/tui_term/widget/struct.PseudoTerminal.html
[tui-term-example]: https://github.com/a-kenji/tui-term/blob/release/examples/nested_shell.rs
[vt100 parser]: https://docs.rs/vt100/0.16.2/vt100/struct.Parser.html
[vt100 screen]: https://docs.rs/vt100/0.16.2/vt100/struct.Screen.html
[termwiz-surface]: https://docs.rs/termwiz/0.23.3/termwiz/surface/struct.Surface.html
[termwiz-terminal]: https://docs.rs/termwiz/0.23.3/termwiz/terminal/
[termwiz-input]: https://docs.rs/termwiz/0.23.3/termwiz/input/enum.InputEvent.html
[alacritty-term]: https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/term/struct.Term.html
[alacritty-pty]: https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/tty/
[alacritty-child]: https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/tty/trait.EventedPty.html
[tmux-control]: https://github.com/tmux/tmux/wiki/Control-Mode
[tmux-man]: https://github.com/tmux/tmux/blob/master/tmux.1
