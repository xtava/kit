# `kit tsgo locus` placement-evidence contract

`locus` is a read-only evidence case for deciding what source anchors to inspect
before choosing where to edit. It does not rank anchors or infer behavioral
authority.

## Minimal case

```json
{
  "schema_version": 2,
  "goal": "Expose the shared caller of two feature paths.",
  "seeds": [
    {
      "id": "seed-a",
      "label": "registerA",
      "selector": {
        "selector": "position",
        "file": "src/a.ts",
        "line": 10,
        "character": 16
      }
    },
    {
      "id": "seed-b",
      "label": "registerB",
      "selector": {
        "selector": "position",
        "file": "src/b.ts",
        "line": 20,
        "character": 16
      }
    }
  ],
  "obligations": [
    {
      "id": "shared-path",
      "statement": "Witness one caller from both paths.",
      "acquisition_ids": ["callers-a", "callers-b"],
      "gap_ids": []
    }
  ],
  "non_goals": [],
  "assumptions": [],
  "acquisitions": [
    {
      "id": "callers-a",
      "seed_id": "seed-a",
      "required": true,
      "accept_no_call_item": false,
      "operation": {
        "kind": "incoming-calls",
        "limits": { "max_depth": 1, "max_nodes": 32 }
      }
    },
    {
      "id": "callers-b",
      "seed_id": "seed-b",
      "required": true,
      "accept_no_call_item": false,
      "operation": {
        "kind": "incoming-calls",
        "limits": { "max_depth": 1, "max_nodes": 32 }
      }
    }
  ],
  "supplied_candidates": [],
  "discovery": [
    {
      "id": "shared-caller",
      "strategy": {
        "strategy": "call-witness-intersection",
        "seed_ids": ["seed-a", "seed-b"],
        "direction": "callers",
        "require_complete": true
      }
    }
  ],
  "declared_gaps": []
}
```

Run it with:

```bash
kit tsgo locus --case placement.case.json --workspace /path/to/worktree --json
```

## Native acquisitions

Each acquisition names one seed, whether it is required, and one operation:

- `definition` with `max_results`;
- `references` with `include_declaration` and `max_results`;
- `implementations` with `max_results`;
- `incoming-calls` with `max_depth` and `max_nodes`;
- `outgoing-calls` with `max_depth` and `max_nodes`.

Every acquisition also declares `accept_no_call_item`. Set it to `true` only
when a non-callable symbol is an expected, explicit outcome for that obligation;
it is rejected for non-call operations. Otherwise `no-call-item` remains open.

A position seed retains the exact requested cursor as query provenance. A
successful call acquisition separately records `semantic_root`, the prepared
call-hierarchy item used to prove that the retained edge prefix is connected.

An obligation may depend only on required acquisitions. Result states preserve
`complete-within-capture`, `cut`, `unsupported`, `no-call-item`,
`ambiguous-call-item`, and `failed`. Retained evidence from a cut is labeled
`retained-before-cut`; it never closes an omitted requirement.

## Candidate discovery

Discovery is allowlisted:

- `supplied-anchors` — exact user-provided positions;
- `seed-definitions` — targets returned by declared definition acquisitions;
- `returned-implementations` — targets returned by declared implementation
  acquisitions;
- `call-witness-intersection` — the exact source anchor shared by at least two
  declared incoming-call acquisitions, or the exact target anchor shared by at
  least two outgoing-call acquisitions.

There is no source-text scan, name similarity, score, fallback, or implicit
candidate promotion.

## Declared gaps

Use a declared gap when native TypeScript operations cannot prove a required
relationship. Supported families include event/reducer, DI registration,
runtime observation, resource flow, upstream policy, another language, dynamic
dispatch, and generated code. Required gaps force `investigation-required` and
remain visible in every candidate's obligation matrix.

## Freshness and replay

Kit validates supplied and seed positions against synchronized UTF-16 source,
hashes every workspace file it synchronizes or reads directly for the case,
reads those files again before returning, and blocks the case if any recorded
input changed or became unreadable. Within one case, repeat operations reuse the
first synchronized bytes for a file; the final recheck detects drift rather than
mixing snapshots. One file is capped at 16 MiB; all source captured by one case
is capped at 64 MiB. Evidence, callsites, ambiguity details,
matrix cells, and observed files also have aggregate caps. Native tsgo can depend
on project files that Kit did not synchronize; those dependencies are outside
this freshness receipt. The result fingerprint binds the normalized case, exact
tsgo server version, and Kit-observed file hashes.

For replay evidence, require:

- equal `result.fingerprint`;
- equal semantic result fields except timing;
- equal service instance and child identity;
- increasing `service.request_count`.

Do not modify a target repository solely to force a freshness failure unless it
is an explicit disposable fixture or the user authorized that verifier.
