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

## Notes

- Default interactive `cr init` should be the good path, not a hidden advanced mode.
- Non-TTY / `-y` paths must not block automation.
- Existing user changes must not be reverted.
