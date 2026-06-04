# Triage Labels

Use only labels that exist in `HeddleCo/heddle`. Do not apply placeholder
labels from upstream skill templates unless a maintainer creates them first.

## Existing Labels

| Label | Meaning |
| --- | --- | --- |
| `p0` | Immediate priority or release-blocking failure |
| `p1` | High-priority work |
| `p2` | Normal-priority work |
| `p3` | Low-priority or opportunistic work |
| `epic` | Umbrella issue that groups related work |
| `spike` | Research, design, or uncertainty-reduction work |
| `tdd` | Work should proceed test-first or has explicit test-design value |
| `blocked` | Cannot proceed until another dependency or decision lands |
| `bug` | Defect or regression |
| `enhancement` | New capability or product improvement |
| `documentation` | Documentation-only work |
| `question` | Clarification, support, or decision request |
| `wontfix` | Intentionally not actioned |
| `duplicate` | Duplicate of another issue |
| `invalid` | Not actionable in this repository |
| `good first issue` | Good entry point for a new contributor |
| `help wanted` | Maintainers welcome outside implementation help |

## Skill Role Mapping

If a skill mentions generic triage roles, map them into the repo vocabulary:

- `needs-triage`: do not apply a label. Choose a priority label only when the issue already contains enough signal.
- `needs-info`: use `question` only when the issue is primarily asking for clarification. Otherwise leave a comment requesting the missing information.
- `ready-for-agent` / `ready-for-human`: do not apply labels. Readiness belongs in the GitHub Project status and issue body, not in this repo's labels.
- `wontfix`: use `wontfix`.

Project fields such as Status, Priority, Size, Epic, Scope, and DoD Type are
separate from labels. When a task requires project-field updates, use the
Project workflow instead of inventing a label.
