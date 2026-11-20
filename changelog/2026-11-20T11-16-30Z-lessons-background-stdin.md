## Changed
- `tools/agentic/lessons.md` (Process): a new rule against leaving a bare stdin reader (`cat`, `read`, `tee`) in a background command. Its stdin never closes, so it hangs with no output, and whatever follows it never runs. Seen during #430.
