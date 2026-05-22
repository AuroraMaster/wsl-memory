# Contributing

Thanks for taking the time to look at this — it's a small project and most
issues / patches get a same-day response.

## Reporting bugs

Please include in the issue:

- WSL version (`wsl --version` on the Windows side)
- Distro and kernel inside WSL (`uname -a`)
- Output of `wsl-memory-agent --version`
- The relevant section of `wsl-memory-agent.log` (host or guest, depending
  on where the symptom shows up)

Memory issues in particular are easier to reproduce when you can share:

- A graph of `vmmem` over the window where it grew unexpectedly
- The cgroup snapshot (`cat /sys/fs/cgroup/memory.stat` inside the distro)

## Patches

Workflow:

1. Open an issue first if the change is non-trivial — saves you the time of
   writing code we can't merge.
2. Fork → branch (`feat/<slug>`, `fix/<slug>`).
3. `cargo fmt && cargo clippy --all-targets -- -D warnings` before pushing.
4. New code paths need a unit test. The reclaim logic in particular is
   tested without a real WSL by injecting `MemoryReader` mocks — keep that
   pattern.
5. Open the PR against `main`. CI runs `cargo test --workspace`.

## Coding conventions

- 2024 edition, MSRV pinned in `Cargo.toml` — bump it only in its own PR.
- Public API surface is small on purpose. Prefer adding fields to an
  existing config struct over introducing a new top-level type.
- No `unwrap()` in non-test code paths.
- Logs go through `tracing` with a stable key for grep-ability.

## Release process

Maintainers only — bump version in `Cargo.toml`, update `CHANGELOG.md`, tag
`v<major.minor.patch>`. CI builds host + guest installers and attaches them
to the release.
