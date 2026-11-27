# lessons.md: test_net for unreachable addresses (issue #1094)

`tools/agentic/lessons.md` now has a rule next to the race-test timeout rule:

- An address that must refuse connections comes from `test_net::Refused`, never from binding `127.0.0.1:0`, reading the port and dropping the listener, and never from a fixed "dead" port.
- A server that needs a port binds `:0` itself and reports the port it chose (as `openssl s_server -accept 127.0.0.1:0` does in `src/investigator/client_tests.rs`).
- Every connect in such a test is wrapped in `test_net::bounded`.

Why: a bind-then-drop port can be taken by a parallel test between the drop and the use, which brought back the flaky-test race #769 (PR #1091) removed.

Documentation only; no code changed.
