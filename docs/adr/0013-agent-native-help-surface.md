# Agent-native help surface

The curated everyday help surface should include `inbox` and `discuss` as first-class coordination commands. `context` remains behind advanced/topic help until its UX is harder to misuse, and `review` may be surfaced through `inbox`, `ready`, and review-specific help rather than always occupying first-day help.

First-slice local discussion and inbox help should live in the normal CLI help surface once tests and docs gates pass. Conflict resolution, visibility, and migration can appear in advanced sections, but core local `discuss` and `inbox` workflows should not be hidden behind experimental or internal help.

Legacy discussion migration should be discoverable when relevant but not promoted as an everyday workflow. It belongs in advanced or doctor-oriented help because it is transitional repository maintenance, not a daily collaboration action.

**Status:** proposed

**Considered Options:** Keeping `discuss` and `context` entirely behind advanced help matched the previous restraint principle, but agent-native coordination is now part of the core loop. Promoting every collaboration verb would make first-run help too broad, so `inbox` and `discuss` are the everyday entry points while context and deeper review remain more deliberate.
