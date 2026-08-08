# Dashboard manual verification checklist

The dashboard client (`dash_*.js`) has **no automated runtime harness**. The
Rust guards in `page.rs` scan the concatenated script as TEXT -- they pin the
wire vocabularies the client renders against, and they cannot observe a single
line of its behavior. Everything below is therefore verified BY HAND, and
running it is part of shipping any change to the transport, render, DOM, or
chrome parts.

Serve the page from a local daemon (`routectl serve`, then open the status
port) and work through the four sections in order. A run that skips section 2
has not verified the failure that matters most.

## What is NOT covered by any test

Three transport rules are manual-only, deliberately. A test asserting that
their source text still contains a particular expression would pin a STRING,
not the behavior -- it passes on a file where the rule below it is commented
out -- and it would make this gap invisible in the coverage story. Naming the
gap here is the honest alternative.

1. **The single-flight generation guard** (`dash_30_transport.js`). Each QUERY
   aborts the previous one and bumps a generation counter; a response whose
   generation is no longer current is dropped instead of rendered. Nothing
   automated observes this. It is the rule section 2 exercises.
2. **Per-pass fault reconciliation** (`dash_40_render.js`, `dash_50_dom.js`).
   Which sources are recorded as faulted on a pass, and the pane status line /
   tab badge / page verdict / favicon that each fault drives, are pure DOM
   bookkeeping across passes.
3. **The DOM and animation surface** (`dash_50_dom.js`, `dash_90_chrome.js`).
   Pane transitions, the age ticker, sparkline and segment drawing, and
   reduced-motion behavior. Motion is CSS; the JS only decides what is drawn.

## 1. Initial load

- Load the page with the browser devtools network panel open. Every request
  must target the daemon's own `/status` family -- **zero external requests**,
  no font, no CDN, no favicon fetch.
- The console must be clean: no errors, no warnings.
- Watch **two full healthy poll rounds**. The poll indicator advances, the
  as_of age resets each round, and no tab enters a stale or error state.
- Click through all six tabs. Each renders content, not an error card.

## 2. Selection supersession (the WORST uncovered failure)

This is the failure the generation guard exists to prevent, and the reason
this checklist exists. A superseded response that repaints is a valid
same-version envelope, so the source still reads `live`: no error card, no
banner, no retry signal -- silently mislabeled data.

- Throttle the network (devtools "Slow 3G" or a request-level delay) so QUERY
  responses take seconds to resolve.
- On Overview, switch **Today -> Week -> Today rapidly**, and while those are
  still in flight also change the group-by and scope to a provider card.
- Let everything settle, then check that the controls, the URL hash, the
  section labels, AND the figures all describe the **FINAL** selection.
- No late response may repaint an earlier selection: a figure that belongs to
  Today under a Week label, or an unscoped total under a provider scope, is
  the bug.
- Repeat on Usage with its group-by picker.

## 3. Hidden-tab and failure recovery

- Switch away from the browser tab for longer than one poll interval, then
  come back. The page must recover to live cadence, and it must NOT have
  accumulated a second timer (verify by watching that the poll indicator
  advances once per interval, not twice).
- Force an unavailable ledger (stop the source of the usage database, or
  point the daemon at a path it cannot read) while the page polls. Expected:
  the affected sources go stale and then dead, the pane status line names the
  not-current source, the favicon reflects the degraded state, and the data
  age stays visible rather than the last-good numbers reading as current.
- Restore the ledger. Recovery returns to live cadence with no duplicate
  timers and no leftover error card.

## 4. Window and visual truthfulness

- Routing, Config, and Doctor must **disable** the window picker (dimmed,
  inert, labeled "All history"). Overview, Usage, and Health keep it live.
- A **sparse series draws gaps as BREAKS, never bridged**: produce traffic
  with an idle stretch in the middle of the window and confirm the sparkline
  and segment charts leave a hole instead of interpolating across it.
- Enable the OS reduced-motion preference and reload. Pane transitions and
  any other motion must be suppressed.
- Config's routing-provider table must distinguish all three rate-limit
  states. Serve a config with one provider at `rpm_limit = 0`, one with no
  `rpm_limit`, and one with a positive cap; the rows must read **0 RPM**
  (at attention weight, not faint), **unlimited**, and **`<n>` RPM**. A `0`
  reading as "unlimited" tells an operator a throttled provider is
  unrestricted, which is the misreport this column exists to prevent.
