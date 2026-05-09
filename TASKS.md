# CodeRoom UI Repair Tasks

## Goal

Make the first-run CLI experience match the polished terminal UI direction from the design mockups closely enough that a new user does not bounce on first sight.

## Current Plan

- [x] Audit current `cr init` / `cr start` UI paths and identify the release gap.
- [x] Implement a polished default `cr init` flow with role picking, engine assignment, and final file-tree confirmation.
- [x] Keep `cr init -y` useful for scripts while still showing a clean summary.
- [x] Improve `cr start` first-run and steady-state welcome screens.
- [x] Clean README / changelog wording so public promises match shipped behavior.
- [x] Run formatting, tests, and manual CLI output checks before handoff.
- [x] Fix release automation so tag pushes create the GitHub Release before uploading assets.
- [x] Make bare `cr` enter CodeRoom directly and send missing-config users through guided setup.
- [x] Replace the returning `cr start` banner with a persistent config dashboard.
- [x] Add `croom` as an alias for environments where `cr` conflicts.
- [x] Fold live tool traces into one per-turn activity summary while keeping full traces in `cr show`.
- [x] Detect existing host-only `.coderoom/` projects on `cr` start and offer a checkbox role expansion flow.
- [x] Keep generated role priors short and role-shaped; move long procedures / references toward engine skills or docs instead of bloating every prompt.

## Notes

- Default interactive `cr init` should be the good path, not a hidden advanced mode.
- Non-TTY / `-y` paths must not block automation.
- Existing user changes must not be reverted.
- Host-only expansion must append roles without overwriting existing priors or user config.
