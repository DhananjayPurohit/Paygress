# docs/solutions/ — institutional learnings

This directory captures recurring patterns, failure modes, and
non-obvious architectural decisions that future contributors (and
future Claude sessions) should know **before** touching the
relevant code.

It is **not** a debugging journal. The entries here are durable: they
describe properties of the system that are easy to forget but
expensive to relearn.

## When to add an entry

Add a solution entry when at least one of these is true:

1. **Footgun**: A piece of code looks correct in isolation but is
   wrong in context (e.g. a function that *appears* to validate a
   payment but doesn't actually spend it).
2. **Cross-cutting invariant**: A property must hold across multiple
   files, and grepping won't reveal the constraint (e.g. "no two
   replicas may be `Live` simultaneously").
3. **External-system contract**: A protocol-level rule whose
   violation is silent (e.g. "Nostr Kind 38384 requires a `d` tag
   for the relay to treat events as distinct addressable records").
4. **Reverted decision**: We tried approach X and rejected it; the
   reasons aren't visible from the current code alone.

Do **not** add entries for:

- Generic Rust / async / git advice.
- Things that are already documented inline in the code.
- One-off bug fixes (those belong in commit messages).

## Layout

```
docs/solutions/
├── README.md                       (this file)
├── patterns/
│   └── critical-patterns.md        seed: footguns and invariants
│                                   tied to the 12-month plan
└── decisions/                      (created on demand)
    └── ADR-NNNN-<slug>.md          reserved for architecture
                                    decision records
```

The `patterns/` subdirectory holds short, rule-shaped entries grouped
by topic. The `decisions/` subdirectory (created when the first ADR
lands) holds longer-form architecture decision records.

## Entry schema

Each entry uses this structure:

```markdown
### <short, imperative title>

**Symptom**: What the wrong thing looks like when it happens.

**Root cause**: Why it happens.

**Fix / rule**: What to do (or not do) to avoid it.

**Where it bites**: File paths or modules where the pattern applies.

**Reference**: Links to plan units, brainstorm sections, ADRs, or
external specs.
```

Keep each entry under ~30 lines. If an entry grows beyond that,
promote it to its own file or to an ADR.

## Relationship to other docs

- **`docs/brainstorms/`**: exploratory, time-stamped strategic notes.
  Source material for plans.
- **`docs/plans/`**: numbered implementation plans. Time-stamped.
  Reference material for `/ce:work` execution.
- **`docs/solutions/`** (this directory): durable knowledge that
  outlives any single plan or sprint. Not time-stamped, kept current
  as the system evolves.

## Maintenance

- Update an entry when the underlying code changes (e.g. once Unit 1
  lands `Wallet::receive`, the "extract-without-redemption" entry
  should be retitled to describe the *new* footgun, if any).
- Delete entries that no longer apply.
- Cross-link aggressively to plan units and source files.
