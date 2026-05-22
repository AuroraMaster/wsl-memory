# Security policy

## Supported versions

Only the latest tagged release is supported with security fixes. Older
tags will not receive patches; if you're stuck on an old version please
open an issue and we can discuss the upgrade path.

## Reporting a vulnerability

Please do **not** open a public GitHub issue for security problems. Email
the maintainer instead (the `Cargo.toml` `authors` field is the canonical
address). A reply within 72 hours is the target; if you do not get one,
follow up by opening a *non-descriptive* issue ("Pending security report,
please contact me") so the message is not silently lost.

When reporting, please include:

- Affected version (`wsl-memory-agent --version`)
- Steps to reproduce, ideally a minimal `config.yaml` and a transcript
- Whether the issue is exploitable remotely or requires local access
- Whether the host or the guest side is affected

## Scope

The host agent runs as a Windows Service (`LocalSystem` by default) and
the guest agent runs as root inside WSL distros. Both are privileged
contexts, so issues that allow:

- Arbitrary file write outside the agent's working directories
- Privilege escalation on host or guest
- Token disclosure (the shared `wsl_agent_token` is sensitive)
- Denial of service against the host (e.g. by forcing endless reclaim
  loops)

…are treated as in-scope.

Out of scope:

- Issues that require the attacker to already have administrator / root
  inside the same context.
- Misconfiguration where the user has manually disabled token auth.

## Disclosure

Once a fix is shipped, the original reporter is credited in `CHANGELOG.md`
unless they ask to remain anonymous.
