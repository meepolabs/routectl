# Public-API baselines

This directory holds one checked-in text baseline per library crate,
listing that crate's public surface as reported by `cargo-public-api`.
The `scripts/public-api.sh` driver generates and checks them.

## What this is

An **informational change detector** for the crates' public API. It makes
surface changes visible without standing in the way of them.

It is **not a gate** and does not fail the build. CI's `public-api` job
runs `scripts/public-api-report.sh`, which wraps `scripts/public-api.sh
--check all` and always exits 0, reporting one of three outcomes as an
annotation and a job-summary line:

- **clean** -- every baseline matches the live surface;
- **drift** -- the named crates' surfaces differ from their baselines;
- **could not run** -- the tooling was unavailable or the check failed,
  with the reason.

The job continues on error and is not one of the jobs the `required`
check depends on, so neither drift nor a broken toolchain bootstrap
blocks a merge. No hook runs it. Locally it runs on demand once
`cargo-public-api` and the pinned nightly are installed:

```
scripts/public-api.sh --check all
```

## Workflow

- **No per-change regeneration.** A change that alters a crate's public
  surface does not regenerate that crate's baseline, and drift reported by
  CI is expected between refreshes. Read the drift report as a list of the
  surface changes made since the last refresh.

- **One refresh after the 0.10 release.** The baselines are regenerated
  once, after the 0.10 release, so they describe that release's surface:

  ```
  scripts/public-api.sh generate all
  ```

  Any later change to this cadence is a policy change made here.

## Baseline properties

- **No timestamps, no machine paths.** Baselines carry only API items --
  no generation date, no absolute paths. Staleness is judged purely by
  content diff, never by age. `public-api.sh` refuses to write or accept a
  baseline that contains an absolute path rooted at a user home or system
  root directory.

- **Deterministic feature set.** Each crate is listed at a fixed feature
  set (its default features), pinned in `public-api.sh` and shared by both
  `generate` and `--check` so the two modes always agree.

- **Pinned nightly.** `cargo-public-api` builds rustdoc JSON with the
  nightly toolchain pinned in `public-api.sh` (`PUBLIC_API_NIGHTLY`), so
  the surface listing is reproducible across machines.

- **Re-exported types are listed once per public path -- do NOT
  hand-deduplicate.** `cargo-public-api` emits an inherent-impl block once for
  each public path a type is reachable through. `Router` is reachable both as
  `routectl_router::router::Router` (`pub mod router`) and through the crate-root
  re-export in `crates/routectl-router/src/lib.rs`, so its `carry_over_*` methods
  appear twice in `routectl-router.txt`. That is deterministic, correct output,
  not an append bug: `generate_one` in `public-api.sh` writes a fresh listing to
  a temp file and `mv`s it over the baseline, so nothing accumulates. Editing the
  duplicate lines out is wasted work -- the next regen restores them and
  `--check` fails. The check is an exact `diff -u` over that deterministic
  output, so a moved, renamed or removed entry still shows as -/+; tolerating
  these duplicates costs no detection power.

## Coverage

One baseline per library crate:

- `routectl-core.txt`
- `routectl-providers.txt`
- `routectl-router.txt`
- `routectl-auth.txt`
- `routectl-usage.txt`
- `routectl-testkit.txt`

`routectl-cli` is **exempt**: it is a bin crate with no public library
surface, so it has no baseline.
