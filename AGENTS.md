# runwatch

- Runs are the product object; Slurm/LSF JobIDs are handles.
- SSH goes through `runwatch-ssh` only. GUI/CLI must not call russh.
- Host names are `~/.ssh/config` aliases. Do not invent a second host database.
- GUI must not allocate a console on Windows.
- Keep the light cream / teal / amber palette in assets.
