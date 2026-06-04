# Issue Tracker: GitHub

Issues and PRDs for this repo live in GitHub Issues for `HeddleCo/heddle`. Use the `gh` CLI for issue-tracker operations.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, fetching comments and labels.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments` with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`
- **Apply or remove labels**: `gh issue edit <number> --add-label "..."` or `gh issue edit <number> --remove-label "..."`
- **Close an issue**: `gh issue close <number> --comment "..."`

Run `gh` from inside this clone so it infers the repository from `git remote -v`.

## When a Skill Says "Publish to the Issue Tracker"

Create a GitHub issue in `HeddleCo/heddle`.

## When a Skill Says "Fetch the Relevant Ticket"

Run `gh issue view <number> --comments`.
