# ui_e2e: Chromium chooses its own debugging port (issue #1101)

`drive()` in `tests/ui_e2e/harness.rs` now launches Chromium with `--remote-debugging-port=0`.

Why: with no port argument, rustwright 0.3.0 picks a free port itself (it binds port 0, reads the number and closes the socket) and gives it to Chromium. A Chromium started by a parallel nextest process could take that port first, so the launch failed with `bind() failed: Address already in use` and timed out waiting for an endpoint that never came.

With port 0, rustwright waits for the `DevToolsActivePort` file in the profile folder (the harness's `BrowserHome` profile) and connects to the port Chromium wrote there. No port is chosen before the browser binds it, so there is nothing to race for.

The launch options are now built by `launch_options`, with a unit test that the port argument is `=0`, appears once, and that the profile folder and IPv4 host rule stay.

Not changed: rustwright always builds the endpoint as `ws://127.0.0.1:<port>` and has no host override, so a Chromium listening on `[::1]` only could not be reached. That case was seen with the old, raced port; whether it can still happen with a port Chromium chose itself is not established, and the harness cannot work around it without bypassing rustwright.
