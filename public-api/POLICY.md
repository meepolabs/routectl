# Public-API baselines

This directory holds one checked-in text baseline per library crate,
listing that crate's public surface as reported by `cargo-public-api`.
The `scripts/public-api.sh` driver generates and checks them.

## What this is

An **informational change detector** for the crates' public API. It makes
every surface change a visible, reviewable artifact in the same diff that
causes it.

It **IS a CI gate** and fails the build when a baseline is out of date
with the live surface. That is the only place it runs unconditionally:
locally it is a commit-gate leg, but a conditional one, blocking where
`cargo-public-api` and its pinned nightly are installed and warning
where they are not -- so the local leg is an early warning and CI is the
guarantee.

A surface diff is expected and fine whenever the change was intended.
The gate does not object to the change; it objects to the baseline not
being regenerated alongside it, which is what keeps every surface change
a visible artifact in the diff that causes it.

## Workflow

- **Author, same commit.** When a change alters a crate's public surface,
  the author regenerates that crate's baseline in the **same commit** that
  makes the change:

  ```
  scripts/public-api.sh generate <crate>   # or: generate all
  ```

  The regenerated baseline rides along with the surface change, so the
  reviewer sees the API delta and the code delta together.

- **Validator, feature boundary.** At a feature boundary the validator
  runs:

  ```
  scripts/public-api.sh --check all
  ```

  A non-zero exit means a baseline is out of date with the live surface --
  i.e. someone changed the API without regenerating its baseline. The fix
  is to regenerate, not to treat it as a build failure.

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
