# runwatch

- Runs are the product object; Slurm/LSF JobIDs are handles.
- V1 is Pi-first: finish `runwatch` + `pi-runs` before any new Codex/Claude/Grok/other-agent productization. Existing Codex code is reference evidence only; do not expand it during the current release cycle.
- SSH goes through `runwatch-ssh` only. GUI/CLI must not call russh.
- Host names are `~/.ssh/config` aliases. Do not invent a second host database.
- Agent-specific identity/resume/settlement should ultimately live in separate Agent Integration projects; keep new runwatch work agent-neutral unless required by the Pi v1 path.
- Formal release/package tooling should be Rust-native; do not make Python a runtime prerequisite for packaging.
- GUI must not allocate a console on Windows.
- Keep the light cream / teal / amber palette in assets.
