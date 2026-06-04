# No Git collaboration export

The Git bridge does not export Heddle collaboration records by default or as an optional projection. Discussions, context annotations, attention views, and collaboration operations are Heddle-only capabilities; sharing them across machines or people requires a Heddle remote backed by Weft.

The Git bridge also should not import GitHub, GitLab, Git notes, or other Git-adjacent comments into Heddle discussions by default. If imported collaboration is needed later, it should be an explicit import workflow using collaboration import roots with source and trust labels, not bridge magic.

**Status:** proposed

**Considered Options:** Git notes, commit trailers, and GitHub/GitLab comment mirroring would make collaboration visible in existing Git hosting tools, but they would flatten Heddle's semantic anchors, capability policy, causal operations, and agent attribution. Collaboration is part of the Heddle product boundary, not a Git compatibility feature.
