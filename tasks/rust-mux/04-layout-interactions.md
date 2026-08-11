# Workspace and Pane Interactions

## Goal
Deliver the workspace, tab, split, and freely rearrangeable pane workflow.

## Context
Layout interaction is the central product experience, but it must operate on service-owned canonical state.

## Requirements
- [ ] Implement create/rename/reorder/delete for workspaces and tabs.
- [x] Implement horizontal and vertical split-tree mutations.
- [x] Implement pane drag/drop with visible targets and cancel-safe behavior.
- [x] Persist layout changes atomically in the session service.
- [ ] Restore focus and layout consistently after desktop restart.
- [x] Display and move local and SSH panes uniformly while retaining explicit connection state.
- [x] Add independent persisted terminal-accent and workspace-color defaults with explicit per-terminal and per-workspace overrides.

## Technical Notes
Use stable IDs in drag payloads and protocol commands. Validate split ratios and reject mutations based on stale revisions.

Current checkpoint: pane headers now pass their own pane ID into add, split, rename, close, and tab activation commands. Two-pane regression tests prove pane-two actions leave pane one unchanged. Dragging a tab displays only the prospective half-pane under the pointer and moves the same live PTY on drop.

Appearance remains intentionally small: a local settings dialog and the existing right-click surfaces share one preset/recent picker. Override precedence is explicit, colors apply live, and terminal/workspace scopes never inherit from one another.

## Acceptance Criteria
- [ ] Every layout mutation has deterministic model tests.
- [ ] Drag/drop works across splits and tabs without losing a local or SSH session.
- [x] Persisted layouts survive service and desktop restarts.
- [x] Appearance defaults and entity overrides survive desired-state migration and restart without changing PTY identity.
