# Web Copy Guidelines

This document captures the copywriting principles and patterns established for Heddle's web presence. Apply these whenever editing `web/src/` content.

---

## Brand voice

**Precise. Calm. Conversational.**

- Never casual, never corporate
- Closer to a well-written RFC than a SaaS landing page
- Direct, technical, second-person
- Like a senior colleague explaining something, not a textbook and not a pitch
- No emojis, no exclamation marks, no rhetorical questions
- Write to one reader, not an audience

See also: `CLAUDE.md` → Brand Personality section.

### Voice dimensions at a glance

| Dimension | Heddle is | Heddle is not |
|-----------|---------|-------------|
| Tone | Calm, measured, assured | Casual, hype, salesy |
| Language | Technical, precise | Jargon-heavy, over-explained |
| Relationship | Peer-to-peer | Vendor-to-customer |
| Approach | Outcome-first | Feature-first |
| Complexity | Progressive disclosure | Overloaded, all-at-once |

---

## The core principle: status over specs

Every piece of copy should answer: **"Who does the reader become?"** — not "what does this feature do."

This is the Apple/Nike insight applied to developer tooling. Apple doesn't sell cameras — it sells the identity of someone who captures masterpieces. Nike doesn't sell shoes — it sells the identity of the athlete who rises above.

**For Heddle, the elite identity is:**
> "I operate at a level of rigor others can't reach. Every change is attributed. Every state is permanent. Nothing is ambiguous, nothing is lost."

The reader is already a competent engineer or leader. Copy should *confirm their judgment*, not *educate them*. They've already decided this matters. Heddle is just the system that matches their standard.

### The hierarchy of "why"

Every surface should answer one of these questions, in order of impact:

1. **Who does this make me?** (identity — highest)
2. **What problem does it solve for me?** (outcome)
3. **What does this do?** (mechanism — only when necessary)

Never lead with mechanism. Always consider: "What will this sentence make the reader feel?"

---

## The minimal interference principle

After guiding users forward, get out of the way. Every sentence should transmit meaning almost instantly, then stop.

Users don't read interfaces, they scan them for anchors. This isn't about dumbing down; it's about respecting cognitive load. Front-load keywords. Put the most important information first. If a user only reads the first five words, those five words should still carry the sentence.

**Examples:**
- Instead of: "You can manage your team by clicking here"
- Write: "Manage your team"

- Instead of: "Initiate Browser Extension Data Collection Protocol"
- Write: "Capture screen for analysis"

---

## Progressive disclosure rules

Show essential copy first. Reveal complexity gradually, on demand.

### When to hide complexity

- Advanced settings -> behind "Show advanced" or a collapsible section
- Detailed explanations -> in tooltips or linked help
- Secondary actions -> below the primary
- Contextual help -> linked, not displayed inline

### When to show everything

- Critical warnings or destructive actions
- Forms where skipping a field causes errors
- Anything that prevents the next logical step

### Tooltip guidelines

Keep under 2 sentences. Lead with the key point. Use plain language.

**Good:** "Auto-save: Changes save automatically every 30 seconds."
**Bad:** A paragraph explaining how auto-save works technically.

---

## Hierarchy of copy surfaces

### 1. Marketing hero (highest impact)
- H1: identity-first, 5–10 words, end with a period
- Lede: 2 sentences max — first makes the reader nod, second earns the CTA
- Stats: signal a living, trusted system — not small numbers that underwhelm
- Body copy: one idea per sentence, lead with outcome

### 2. Feature/scene cards
- Lead with outcome or pain eliminated
- One idea per sentence
- Mechanism is supporting detail, not the lead
- 1–3 sentences max

### 3. Page descriptions (subpage heroes)
- Declare what you'll see/control, not what the page is for
- Never: "Browse X across your workspace"
- Always: declarative statement of the value

### 4. App page descriptions (authenticated UI)
- Confirm expertise — do not explain what namespaces, grants, or threads are
- These users already know the domain. Speak to them as peers.
- 1–2 sentences maximum

### 5. CTAs
- Active claiming: "Get access," "Apply," "Inspect"
- Always be specific: "Download report" not "Download"
- Never: "Request," "Submit," "Sign up," or "Learn more" as the primary CTA
- Sentence case: "Get access" not "Get Access"

### 6. Empty states
- Empty = potential, not broken
- Tell users what goes here and what to do next
- Three parts: what is empty + why it matters + clear action
- Never: "No X found." or "X may be empty."
- Celebrate positive states ("You're all caught up")

### 7. Error states
- State the problem clearly
- Always provide a next step; never leave the user stranded
- Avoid system codes ("Error 403") when possible
- Never blame the user

**Good:** "Token import failed. Export tokens in the required format, then try again."
**Bad:** "Error: Import failure."

### 8. Loading and processing states
- Tell users what is happening, not just "Processing..."
- For long operations: estimate time or show progress
- If work can continue in the background, say so explicitly

### 9. Confirmations
- Be specific: "Invited 3 team members."
- Include next step when relevant: "Invitation sent. Review grant scope if access should be narrower."
- Use positive language to build momentum

### 10. Destructive actions
- State the action and consequence explicitly
- "Delete repository" (not "Delete") - always include the object
- Add consequence when irreversible: "This can't be undone"

---

## Terminology standards

Inconsistency is a tax on the reader's attention. When one screen says "repository" and another says "project," users hesitate. Define terms once, then use them consistently everywhere.

### Vocabulary rules

| Always use | Avoid |
|------------|-------|
| repository | repo, project |
| namespace | org, folder, project space |
| grant | permission, access (vague) |
| thread | agent, worker, session |
| Inspect | View, See, Manage |
| Apply | Submit, Request |
| Get access | Sign up, Register |

### When a technical term is unavoidable

Provide a brief explanation on first use when the audience may not already know the term:

> "SOC 2 reports are available for teams that require formal vendor review."

Or use a tooltip or linked note for secondary explanation. Do not overload the main sentence.

---

## Capability truth rules

Heddle's web copy may speak about the future, but it must do so accurately.

### Allowed capability labels

- **Shipped** - implemented and safe to describe as current behavior
- **Foundation in place** - partially implemented or structurally supported, but not yet a complete user-facing product surface
- **Planned** - clearly intended future-state documented in `docs/` or `web/PRODUCT_SPEC.md`

### Rules for future-state copy

- Marketing pages may position Heddle around planned differentiators if those differentiators are already rooted in the codebase and roadmap
- Authenticated product surfaces must not present mock-backed or incomplete functionality as if it is fully live
- If a route contains placeholder, local-state-only, or mock data, label the capability as preview, foundation, or planned rather than describing it as a completed system
- Do not invent capabilities that are absent from both the codebase and the roadmap docs
- Do not let a CTA imply an action exists if the route or backend behavior does not exist

### High-risk claims to avoid

- `actor spawn` creates isolated filesystems (it does not — use `heddle start --path` for that)
- context, agent telemetry, or settings flows are fully persisted when the route is still mock-backed
- hosted builds, workflows, logs, or artifact surfaces are live today
- sessions or segments are implemented

---

## Specific patterns established

### Mechanism → outcome rewrites

| Before (mechanism-first) | After (outcome-first) |
|--------------------------|----------------------|
| "Every object is content-addressed with BLAKE3. State IDs are stable across clones, forks, and time." | "Content-addressed with BLAKE3. State IDs hold across clones, forks, and time. You will never chase a disappeared commit or wonder what got force-pushed." |
| "Repositories, teams, and access grants inherit through a single organisational model." | "Set access once at the namespace. Roles cascade to every repo beneath it — no per-repo configuration, no drift, no second-guessing who can see what." |
| "Provider, model, confidence, and verification status sit beside every change — recorded in the object model, not inferred." | "Know exactly who changed what — which human, which model, at what confidence. Recorded in the object model, not scraped from commit messages after the fact." |
| "Heddle communicates scope, hosted role, verification, and agent attribution with the same clarity as history." | "Scope, role, verification, and attribution — surfaced with the same clarity as your repository state. Every denial is explicit. Every grant is auditable." |

### Admin copy: confirm, don't teach

| Before (pedagogical) | After (confirms expertise) |
|----------------------|---------------------------|
| "Control who can access which namespaces and repositories. Namespace grants apply to all repositories inside them; repository grants apply only to that repo." | "Namespace grants cascade. Repository grants scope precisely. Both are always auditable." |
| "Create and organize namespaces and teams that contain your repositories." | "Define your organizational hierarchy. Access cascades automatically." |
| "Manage namespaces, repositories, and access grants for your workspace." | "Namespaces, repositories, and grants. One surface for your entire organizational structure." |

### CTA language

| Before | After |
|--------|-------|
| "Request early access" | "Get early access" |
| "Request access" | "Get access" |
| "Submit request" | "Apply" / "Apply for access" |
| "Review grant surface" | "Inspect grant surface" |

### Scarcity and belonging (access forms)

The request-access flow should signal selectivity, not openness. The reader is qualifying themselves, not asking for a favor.

| Before | After |
|--------|-------|
| "Tell us what you're building and we'll get you set up." | "We're onboarding a small number of teams who take provenance seriously." |
| "Heddle is in private preview." | "Heddle is onboarding teams who take provenance seriously." |
| "Side projects, open source, research — whatever you'd use Heddle for." | "What does your current version control not solve?" |

### Closing with identity

The final line of any section should close with the reader's identity, not a feature. This is the Nike "Just Do It" — it reaffirms who they are.

| Before | After |
|--------|-------|
| "One verified state. Immutable history. Clear attribution for every actor." | "One verified state. Nothing lost. Nothing ambiguous. This is how the best teams ship." |
| "The teams that move fastest are the ones that always know who did what." *(already good)* | — |
| "Built for teams where ambiguity in access control is not acceptable." | — |

---

## Key files for web copy

All user-facing copy lives in:

| File | What it controls |
|------|-----------------|
| `web/src/lib/content.ts` | Hero stats, capabilities, product pillars, scene labels |
| `web/src/lib/components/HeddleSequence.svelte` | Hero H1, lede, feature cards, stable CTA |
| `web/src/lib/components/MarketingHeader.svelte` | Nav CTA label |
| `web/src/lib/components/MarketingFooter.svelte` | Tagline, status line, nav links |
| `web/src/routes/+page.svelte` | Page title, meta description, request-access section |
| `web/src/routes/request-access/+page.svelte` | Dedicated access form copy |
| `web/src/routes/product/+page.svelte` | Product page sections and pillars |
| `web/src/routes/namespaces/+page.svelte` | Namespaces marketing page |
| `web/src/routes/security/+page.svelte` | Security page hero and cards |
| `web/src/routes/app/agents/+page.svelte` | Agents page descriptions |
| `web/src/routes/app/namespaces/+page.svelte` | App namespaces description |
| `web/src/routes/app/worktrees/+page.svelte` | Worktrees description and arch note |
| `web/src/routes/app/activity/+page.svelte` | Activity feed description |
| `web/src/routes/app/admin/+page.svelte` | Admin landing and cards |
| `web/src/routes/app/admin/grants/+page.svelte` | Grants admin description |
| `web/src/routes/app/admin/repositories/+page.svelte` | Repos admin description |
| `web/src/lib/components/DashboardContent.svelte` | Dashboard empty states |

---

## Scoring rubric

Apply this when reviewing any copy change. Read each surface out loud. If it sounds stiff, vague, or slow, it needs work.

### Overall score

| Score | Signal | Action |
|-------|--------|--------|
| 5 | Identity-first. Reader feels elite in <3 seconds. No mechanism. | Ship it. |
| 4 | Strong outcome, minor mechanism leak. | Minor polish. |
| 3 | Mixed. Informative but not landing emotionally. | Needs rewrite. |
| 2 | Mechanism-first with outcome appended. Reads like documentation. | Rewrite. |
| 1 | Pure feature description. Reader learns what exists, feels nothing. | Delete and restart. |

**Target:** nothing below 3 on any visible surface. Marketing hero and section cards: target 4–5.

### Checklist by surface

**H1 (marketing):**
- [ ] Identity-first (who do I become?)
- [ ] 5–10 words
- [ ] Ends with a period
- [ ] No mechanism

**Lede:**
- [ ] First sentence earns a nod
- [ ] Second sentence earns the CTA
- [ ] 2 sentences max

**Feature cards:**
- [ ] Leads with outcome or pain eliminated
- [ ] Mechanism only as supporting detail
- [ ] One idea per sentence

**CTAs:**
- [ ] Active claiming (not supplicating)
- [ ] Specific (not generic)
- [ ] Sentence case

**Empty states:**
- [ ] Reframes potential (not broken)
- [ ] Clear next action
- [ ] No "No X found"

**Error states:**
- [ ] Problem stated clearly
- [ ] Next step provided
- [ ] No user-blaming language

---

## Anti-patterns to avoid

- **Feature-first leads**: "Every object is content-addressed with BLAKE3..." -> ask what this means for the reader
- **Passive voice**: "is designed to," "can be used to," "allows you to"
- **Corporate hedging**: "helps you manage," "enables teams to"
- **Explaining the obvious**: Do not define namespaces, grants, or threads for people already using them
- **Dead empty states**: "No X found." or "X may be empty."
- **Supplicating CTAs**: "Request," "Submit," "Sign up"
- **Filler transitions**: "in one place," "at a glance," "seamlessly," "easily"
- **Generic button labels**: "Submit," "OK," "Click here"
- **Raw system language**: "Error 403" instead of the user-facing consequence
- **Dead-end errors**: a problem with no next step
- **Title case on buttons**: use sentence case ("Get access" not "Get Access")

---

## Testing and iteration

Words written in a vacuum are guesses. Validate copy against real use whenever possible.

### Fast review

- Read every visible surface out loud
- Ask: would this still make sense without surrounding context?
- Check the first five words of each sentence; that is where the meaning should start
- Replace any sentence that explains the system before the outcome

### Qualitative testing (5-8 users)

- Use think-aloud protocol: ask users to verbalize thoughts while interacting
- Watch for hesitation before clicks; hesitation means the copy was not clear enough
- Ask: "What do you expect to happen when you click this?"
- If users ask questions, the copy failed to answer them

### A/B testing (high-traffic surfaces)

For critical copy like CTAs and value propositions:

- Test two variants with clear success metrics
- Run until statistical significance
- Small improvements to CTAs compound; do not skip this for hero copy

### Analyze support for copy failures

Support tickets reveal where copy failed. If users repeatedly ask what a term means or what a button will do, the UI copy needs revision.

---

## Slash command

Use `/web-copy <file>` to run a targeted review and rewrite pass on any web copy file.

---

## Icons, imagery, and visual decoration

Research basis: Nielsen Norman Group (Icon Usability, Aesthetic and Minimalist Design), UX Myths, Carbon Design System, WCAG 2.1, PMC cognitive performance studies, Smashing Magazine 2024.

### The core rule

**Every visual element must earn its place or be removed.** NN/G Heuristic #8 is explicit: every extra element competes with the relevant elements and diminishes their visibility. The question is never "what can we put here?" — it is "does removing this change comprehension or orientation?" If not, remove it.

Restraint signals precision to expert audiences. Decorative icons signal marketing. A CTO auditing grant access and a staff engineer reviewing a diff are in a different cognitive mode than a consumer browsing an app — visual decoration is read as a sign of immaturity, not polish.

### Icon decision table

| Surface | Rule |
|---|---|
| Destructive action buttons (delete, revoke, drop) | Icon + label + color signal. All three. Redundant encoding for accessibility (WCAG 1.4.1). |
| Primary CTAs (Create, Merge, Push) | Label only. The label is sufficient. |
| Sidebar navigation | Icon + label. Collapsed state: icon-only with tooltip and aria-label. |
| Data table type indicators (branch, tag, blob, agent) | Small icon left of name column. No label needed when column header provides type context. |
| Data table status indicators (running, error, idle) | Icon + semantic color. Never color alone — 4.5% of users have color vision deficiency. |
| Empty states | One semantic icon + one line of text. No illustration, no decorative art. |
| Section headers on settings/multi-section pages | Icon when orientation across sections is ambiguous (e.g. "Danger Zone"). |
| Section headers on single-purpose pages (repo detail, activity log) | No icon. Typography carries the hierarchy. |
| Form inputs | Icon for search (magnifying glass), password (lock), and actionable suffixes (copy, reveal) only. Not for free-text configuration fields. |
| Page/route titles and breadcrumbs | No icon. Typography only. |
| Dense toolbar / action rows (diff view, branch graph) | Icon-only acceptable when: tooltip present, aria-label in DOM, user is task-mode expert. |
| Dead/negative space | Leave it. Do not fill with imagery, patterns, or background decoration. Whitespace is structural. |

### Buttons

- Destructive actions (delete, revoke, force-push): icon reinforces intent before the click. Use the trash icon for delete, a warning symbol for revoke. Always combine with text label — never icon-only on destructive.
- Primary create/save/confirm CTAs: label only. A "Create Repository" button needs no plus icon.
- Directional navigation (prev/next, back, breadcrumb): chevron/arrow icons are universally recognized and justified.
- Icon-only buttons: only acceptable in toolbars where (a) icon is universally recognizable, (b) user has expert-level spatial memory from repeated use, (c) tooltip and aria-label are always present. Never as a space-saving measure alone.
- Low-stakes secondary buttons (Cancel, Close): no icon. The icon adds weight competing with the primary action.

### Navigation

- Icon + label in sidebar navigation is the research-dominant pattern. Icon-only toolbars went essentially unused until labels were added (Microsoft Outlook case study — NN/G).
- Icon serves as a scannable anchor; the label confirms. The combination outperforms either alone.
- Collapsed icon-only sidebar is acceptable as a user-controlled persistent state — not as default.
- Active/selected state: combine background highlight + label weight change + icon fill-vs-outline state change. Never rely on color alone.

### Empty states

- One semantic icon (not decorative, not illustrative) + one short line of text explaining why it's empty and what triggers content to appear.
- No illustrations, no character art, no marketing copy. Expert audiences read decorative empty states as immature and unserious.
- First-use empty (no data yet) vs. zero-result empty (search returned nothing) are different: first-use may include a single CTA; zero-result should be explanation only.
- Never two competing CTAs in an empty state.

### Data tables and status indicators

- Type icons (branch, tag, worktree, agent, blob) are a legitimate and valuable use — they enable visual pre-sorting before reading labels. Concrete icons (icons that look like the thing) outperform abstract ones. (PMC icon familiarity study)
- Status indicators must encode redundantly: icon + color. Relying on color alone fails WCAG 1.4.1.
- Maximum five or six distinct status types per view. More than that defeats scannability.
- Badges (unread count, error count) are justified for async notification state — unread activity, failed runs, pending grants. A badge that never clears trains users to ignore it; clear aggressively.

### Dark-first icon behavior

- Contrast requirements are stricter on dark surfaces. WCAG 2.1 requires 3:1 contrast for non-text elements against background. Icons designed for light mode often fail when naively inverted — audit specifically.
- Filled vs. outline state meaning inverts on dark backgrounds. On dark surfaces: outline = inactive/secondary; filled = active/selected. This is the inverse of the typical light-mode convention. Be explicit.
- Color in icons is reserved for status semantics (green active, red error, amber warning). Never for visual interest or brand expression.
- Use a monochrome icon system tinted warm (towards amber neutrals) to match the warm-dark surface palette. Cold grey icons on warm-dark surfaces create tonal discord.
- Multi-color and illustrative icons read as consumer/toyish in professional dark UIs. Avoid.

### Imagery and negative space

- Empty space is not a problem to be solved. It is a signal of confidence and restraint.
- Whitespace reduces cognitive load, improves focus, and signals professionalism. Dense platforms (GitHub, Jira) fill space habitually — this is not aspirational.
- Never add background graphics, patterns, or decorative illustrations to fill perceived visual gaps. If a panel is sparse because data is sparse, that is correct.
- Restraint only works when typography is doing structural work. Flat typography + sparse content reads as unfinished. Strong typographic hierarchy + sparse content reads as precise.

### Vocabulary size

Consistency matters more than presence. An interface with 12 carefully chosen universally recognizable icons signals better design than 40 icons covering every surface. Every icon that exists was earned. Every icon that could be removed should be removed until it is missed.
