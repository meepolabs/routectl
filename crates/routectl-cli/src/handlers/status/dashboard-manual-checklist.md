# Dashboard manual verification checklist

The dashboard client (`dash_*.js`) is driven in a real headless Chrome by the
browser tests beside it (`page_browser_tests.rs` and its
`page_browser_<topic>_tests.rs` sidecars, run with
`cargo test -p routectl-cli page_browser -- --ignored`). The Rust guards in
`page.rs` scan the concatenated script as TEXT. What neither reaches is
verified BY HAND below, and running it is part of shipping any change to the
transport, render, DOM, or chrome parts.

Serve the page from a local daemon (`routectl serve`, then open the status
port) and work through the sections in order. A run that skips section 2
has not verified the failure that matters most.

## What the browser tests cover

- **The single-flight generation guard** (`dash_30_transport.js`):
  `page_browser_epoch_tests.rs` holds and reorders answers so a late QUERY
  for an old range, an older answer for the same selection, a late answer
  for a view left behind and a late aggregate after a terminal 403 never
  repaint; a held query is neither aborted nor duplicated by the 5 s rounds.
- **Per-pass fault reconciliation** (`dash_40_render.js`):
  `page_browser_states_tests.rs` (an invalid payload errors only its own
  sections while polling continues, a failed refresh keeps the section marked
  stale, a pending selection ends on its answer) and
  `page_browser_verdict_tests.rs` (the verdict and favicon each fault drives).
- **The host-owned section boundary** (`dash_40_render.js`):
  `page_browser_host_tests.rs` (a section with no guard of its own gets the
  loading, unavailable, error and recovered states from `viewSection`).
- **Focus, selection and open state across a refresh** (`dash_40_render.js`,
  `dash_50_dom.js`): `page_browser_tests.rs`, `page_browser_locks_tests.rs`
  and `page_browser_sort_tests.rs` keep the focused node, a text selection,
  an open row and disclosure, the seat dialog, and page and table scroll.
- **The changed-value mark and reduced motion** (`dash_40_render.js`,
  `dash_50_dom.js`, `dash_90_chrome.js`): `page_browser_motion_tests.rs`.
- **The Accounts rows** (`dash_62_view_accounts.js`):
  `page_browser_accounts_tests.rs` (grouping and names, the login cause and
  focusable fix, windows named from their length with level, reset, age and
  stale mark, the poll line, the drivers, and a row kept across a refresh).
- **The address and the views** (`dash_90_chrome.js`):
  `page_browser_url_tests.rs` and `page_browser_views_tests.rs`.
- **Figures read in full at each text size** (`dash_*.css`):
  `page_browser_layout_tests.rs` loads Now, Traffic and Doctor at 720 px,
  at 1440 px and at 1440 px with a 32 px default font, and fails on any text
  a box cuts off, a scroller the keyboard cannot reach, or stat labels drawn
  over each other.

## What is NOT covered by any test

1. **A real slow ledger** (`dash_30_transport.js`). The browser tests hold
   answers on the page's clock; nothing automated supersedes a read that is
   slow on the server, which is what section 2 exercises.
2. **The drawn surface** (`dash_50_dom.js`, `dash_90_chrome.js`). The age
   ticker, sparkline and segment drawing, the look of the layout at each
   width (the layout tests catch hidden text, not a poor arrangement), and
   how the fade and pane transitions look. The tests read classes and computed
   styles, never pixels.
3. **The fade reaching its end by itself.** Headless Chrome under virtual
   time renders too few frames for a CSS animation to finish, so the browser
   test dispatches the fade's end; section 4 watches a real one.

One hazard of the multi-part split IS covered mechanically, and belongs here so
nobody re-files it as manual work: two parts declaring the same top-level name.
The parts share one function scope, so the later declaration silently replaces
the earlier one for every caller of both, and the symptom is a tab stuck on its
`invalid_payload` card with a healthy transport and a well-formed payload.
`page.rs`'s `dashboard_script_parts_declare_no_colliding_top_level_names` scans
the concatenation for it.

## 1. Initial load

- Load the page with the browser devtools network panel open. Every request
  must target the daemon's own `/status` family -- **zero external requests**,
  no font, no CDN, no favicon fetch.
- The console must be clean: no errors, no warnings.
- Watch **two full healthy poll rounds**. The poll indicator advances, the
  as_of age resets each round, and no tab enters a stale or error state.
- Click through all six tabs (Now, Accounts, Traffic, Models, Config,
  Doctor). Each renders content, not an error card.
- Load straight to `#now` AND straight to `#traffic`, then switch between
  the two in **both orders**. Both tabs must render content on every visit.
  The two tabs issue DIFFERENT `/status/query` shapes -- Now bucketed,
  Traffic series-less -- into their own source records, and a payload of one
  shape cannot render the other tab. A tab that shows the
  `invalid_payload` card here while its transport reads live means the two
  reads have been collapsed back onto one record.

## 2. Selection supersession (the WORST uncovered failure)

This is the failure the generation guard exists to prevent, and the reason
this checklist exists. A superseded response that repaints is a valid
same-version envelope, so the source still reads `live`: no error card, no
banner, no retry signal -- silently mislabeled data.

- Throttle the network (devtools "Slow 3G" or a request-level delay) so QUERY
  responses take seconds to resolve.
- On Now, switch **Today -> Week -> Today rapidly**, and while those are
  still in flight also change the group-by and scope to a provider card.
- Let everything settle, then check that the controls, the URL hash, the
  section labels, AND the figures all describe the **FINAL** selection.
- No late response may repaint an earlier selection: a figure that belongs to
  Today under a Week label, or an unscoped total under a provider scope, is
  the bug.
- Repeat on Traffic with its group-by picker.
- Supersede a SLOW read: on a ledger large enough that the All window takes
  most of a second, select All, then pick another window (or, on Traffic, another
  group-by) while that read is still running. The server answers the new QUERY
  `db_busy` until the aborted read lets go of its permit. The panel must stay
  in Loading and fill with the new selection within about a second: no red
  "unavailable: db_busy" card, no "n/a" tab badge, no "query unavailable" in
  the verdict, and no 10 s wait for a backoff retry.

## 3. Hidden-tab and failure recovery

- Switch away from the browser tab for longer than one poll interval, then
  come back. The page must recover to live cadence, and it must NOT have
  accumulated a second timer (verify by watching that the poll indicator
  advances once per interval, not twice).
- Force an unavailable ledger (stop the source of the usage database, or
  point the daemon at a path it cannot read) while the page polls. Expected:
  a source that had rendered keeps its last values, dimmed and marked stale
  (a source that never rendered shows its error card), the pane status line
  names the not-current source, the verdict line and favicon keep the
  daemon's verdict (a ledger fault is not a routing fault), and
  the data age stays visible rather than the last-good numbers reading as
  current.
- Restore the ledger. Recovery returns to live cadence with no duplicate
  timers and no leftover error card.

## 4. Window and visual truthfulness

- Models, Config, and Doctor must **disable** the window picker (dimmed,
  inert, labeled "Current state"). Now, Accounts and Traffic keep it live;
  on Accounts it moves the traffic lines only.
- A **sparse series draws gaps as BREAKS, never bridged**: produce traffic
  with an idle stretch in the middle of the window and confirm the sparkline
  and segment charts leave a hole instead of interpolating across it.
- Watch two healthy rounds on Accounts: a quota figure that moved flashes
  and fades back within half a second, a meter slides to its new width, and
  a figure that held does not flash. Switching views flashes nothing.
- Enable the OS reduced-motion preference and reload. Pane transitions, the
  changed-value flash and the meter slide must be suppressed.
- Config's routing-provider table must distinguish all three rate-limit
  states. Serve a config with one provider at `rpm_limit = 0`, one with no
  `rpm_limit`, and one with a positive cap; the rows must read **0 RPM**
  (at attention weight, not faint), **not set here** (faint, its tooltip
  saying a pool or default may apply), and **`<n>` RPM**. A `0` reading as
  "not set here" tells an operator a throttled provider is unrestricted,
  which is the misreport this column exists to prevent; an unset cap reading
  as no cap at all hides one a pool or default still applies.
- Config's default-state card must not contradict the tables under it. Serve
  a config naming ONLY providers -- no aliases, no models -- and confirm the
  "Running on the default configuration" card is ABSENT while the provider
  table renders. Then serve a config naming nothing at all and confirm the
  card IS present. Each operator-authored surface added to this tab has to
  join that card's suppression condition, or the tab tells an operator their
  configuration is not in effect while displaying it.
- Config's model detail row names where each fact comes from. Serve a config
  with one `anthropic-api` model carrying `context_window`, a
  `[models.X.pricing] output_per_mtok`, and no `max_output_tokens`, plus an
  `openai-compat` model with none of them, with a `prices.json` refreshed into
  the config folder. Expand both rows: the first reads its window with
  `(model-config)`, output fill `64K (default)`, input with `refreshed` and the
  table date, output with `(model-config)`, and the 1h write `(derived)`; the
  second reads `not configured`, `not filled`, and `unknown` for every price it
  has no row for, never `0`. No provider id or host from the table appears.
- The cache write figure keeps an unreported write apart from a zero. In a
  ledger copy, give one model only rows whose provider sent no write field
  (`cache_write_tokens` NULL) and another a row that reported `0`. On Traffic,
  CACHE READ / WRITE reads a faint `-` on the write side for the first and
  `0` for the second; scoped to a provider with no reported write, the
  Now WRITE tile reads `-` and its note says no cache write was reported.
- Check the next item at 720, 1024 and 1440 px wide and at 1440 px with
  200% text, in both the dark and the light theme, and once more with the
  OS reduced-motion preference on (no value animates, nothing is lost).
  Expanded Config model rows stay readable: no label runs into another
  pair's value, nothing is cut off at the row edge, and a pair too wide for
  its column drops the value under its label. The Now EST. COST, TIME TO
  FIRST TOKEN and CACHE HIT notes wrap instead of truncating, and hovering
  each shows the full sentence.

## 5. Doctor tab capability surface

- Serve a config with two nicknames on ONE upstream of one provider entry,
  plus a model on a pool of two member entries, against a ledger holding
  learned rows for those lanes. The Doctor tab's capability matrix must list
  one row per `<provider>#<upstream>` lane -- the shared lane naming BOTH
  nicknames, the pooled model on one row per member -- and each row's lane
  text must be exactly what `routectl capability purge` accepts.
- Hover a learned cell: the tooltip names its source, its routing action,
  its first-seen / last-seen / expiry times, and its value (`value
  ceiling:<n>` / `value shape:<v>`, or `value -` for a verdict that writes
  none, such as a dropped field). An unknown cell reads `-`, and a cell no
  learned entry backs names no value.
- The replay line names rows read, replayed, and every skip reason. Seed a
  row whose provider entry the config no longer has and confirm the owner
  skip count moves.
- Point the daemon at a ledger whose post-boundary slice cannot be read. The
  matrix state line must say UNAVAILABLE with its code, never "empty".
- The matrix header carries a source badge: `ledger replay` when the panel
  was built from a read-only ledger replay, `resident` when it reflects the
  daemon's in-memory registry. Hover it: the tooltip names that origin.
- A `resident` panel shows a "warm at boot" line with the boot outcome (an
  `unreadable` warm adds its failure class in parentheses) and, when the warm
  replay ran, its tally, and no replay line. Its state line reads "learned
  registry resident" when entries are present, never "replayed"; an
  unavailable resident state line ends its code with "(boot warm)". A `ledger replay`
  panel shows the replay line and no warm line. A skip count of 0 reads `0`,
  never `-`.
- The capability-writes card shows three separate counters, all faint at
  zero on a healthy daemon.

## 6. Models tab learned negatives

- Serve a config with two nicknames on ONE upstream of one provider entry and
  a third nickname on a different upstream, against a ledger holding a
  `broken` row for the shared `<provider>#<upstream>` lane and nothing for the
  other. On the Models tab, the learned-negatives chip for that capability
  must appear on the card of EVERY nickname that maps to the shared lane, and
  the third nickname's card must read `none`. A card reading `none` for a
  nickname on a lane the ledger holds a negative for means the client joined
  learned rows on the runtime `state_key` instead of the target's
  `learned_lane`.
- Seed a learned output ceiling (`ceiling:<n>`) on a lane. Hover its chip:
  the tooltip ends in `value ceiling:<n>`; a chip for a capability whose
  verdict writes no value ends in `value -`. The chip text stays the bare
  capability key, the one to copy into `force_supported`.
- A `verified` row on the same lane must not render as a chip. Seed it with a
  recognized `evidence_class` (for example `schema_parse`): replay skips a
  `verified` row whose class is missing or unrecognized, so a row seeded
  without one never reaches the registry and the check passes vacuously.

## 7. Models and Accounts tabs: eligibility, accounts by pool and the seat drivers

- Serve a config with a pool of two `oauth://anthropic#<label>` members,
  one logged in and one never logged in, against a throwaway credentials
  file. Before any request, the never-logged-in seat's card must sit under
  "Needs attention", never "Not observed yet": its reason line reads `1 of 2
  seats not eligible`, and its eligibility cell names the seat,
  `credential_unusable (not_logged_in)`, how long ago it was marked, and the
  exact `routectl login anthropic --label <label>` command, selectable as one
  unit. A logged-in seat's eligibility cell reads `eligible`.
- On Accounts the pool is listed with `1 of 2 usable` beside its name, and
  each member is a row named `<provider entry> (<label>)`. The never-logged-in
  seat reads `unusable` with the daemon's sentence (`Not logged in, for
  <age>`), when it began, and its `routectl login anthropic --label <label>`
  fix in a code box Tab reaches. Log the seat in (against the same throwaway
  file); within one poll the header reads `2 of 2 usable`, the row reads
  `logged in` and the fix is gone, with no restart. A seat named by two pools
  is listed under each; an API-key or cloud entry is listed under "Not in a
  pool" and reads `this provider reports no usage windows`.
- Each window of a polled seat is named from its reported length (`5 h`,
  `7 days`; a Codex five-hour window reads `5 h`, never `weekly`), with
  `<n>% used`, `<n>% left`, `resets in <time>`, `read <age> ago` and its
  source. The meter is the accent color, amber with `nearly used` past 90%
  and red with `at limit` at 100%. Stop the usage poll (block its vendor
  endpoint) and wait 7 minutes: the window dims, reads `room unknown` and
  carries a `stale` mark, and its age keeps advancing. A seat with no
  reading yet says `no reading yet`, never a meter at zero.
- Send a few requests through one pool seat. On Accounts the heading above
  the pools reads `Traffic today - requests and cost per account`, and that
  seat's `traffic` line shows its request count and `~$<n> At API prices`
  (never a bare dollar figure for subscription traffic). A configured seat
  that carried nothing reads `no traffic today`, never `0 req`. Send a
  request through an API-key entry: its row shows its own request count and
  cost; an API-key or cloud entry that carried nothing reads `no traffic
  today`.
  Pick Week: the heading and the counts move to this week, while every
  login state, window, reset time and poll line stays exactly as it was.
  Remove a seat from the config and reload: its traffic does not appear as
  a row.
- The "Background seat drivers" section shows the credential refresh and
  usage poll drivers, each `running` on a daemon with a credential store,
  `never` for the sweep stamps before the first sweep, and `0` (not `-`) for
  zero counts. After a minute the last sweep age appears and `sweeps` reads
  `1`. On a daemon with no credential store both read `stopped` in amber.
- With both pool seats logged out, the status line at the top reads the
  daemon's sentence naming the empty pool in red, with a `see Accounts`
  link, and the tab icon is red. With one of two seats out it reads the
  sentence naming that seat in amber, with an amber icon; with both logged
  in it reads `All routes healthy` with no link. Clicking `see Accounts`
  opens Accounts and keeps the selected range in the address.
- With a seat still unusable, block `/status/usage?window=all` in the
  browser devtools (request blocking; it is the one source fetched on its
  own) and wait two polls. The status line still reads the seat sentence:
  only the attention panel moves it, and the blocked source shows its
  failure only on its own sections.
- With a seat logged out, stop the daemon and wait two polls. The status
  line keeps the same sentence, dimmed, followed by `as of <time> - the
  daemon cannot be reached`; the icon stays red for a broken verdict. It
  never returns to `Checking routing state`. Start the daemon again: within
  a poll the current sentence replaces it and the dimming clears.
- The Models tab badge counts every target with an eligibility reason or a
  non-closed breaker once: two logged-out seats badge `2`, and a seat that is
  both logged out and breaker-open still counts once.
- The Traffic "Live target state" row for the same target shows a red dot,
  a non-green pill (`no seat` when every seat is out, otherwise the reason),
  and `<n> of <m> not eligible`; never `unknown - nothing dispatched to it
  yet`. The Models card's state pill shows the same label, never a green `ok`.
- Select a fix command with the mouse and wait through three polls with
  nothing changing: the selection stays, and Ctrl+C copies the full command.
  The `since <age> ago` text beside it keeps advancing (ages update every
  poll; only the nodes that changed are replaced).
- At 800 px wide, the eligibility cell spans the whole card row and each fix
  command sits on one line; a command wider than the card scrolls inside its
  own box instead of wrapping at a hyphen.
- Provider-card seat dots follow the current config. Serve a config whose
  provider entry name differs from its login's vendor (for example entry
  `work` with `credential = "oauth://anthropic#a"`) against a ledger copy
  holding quota rows for that seat and for a seat the config no longer names.
  On Now the `work` card shows one seat dot (`1 seat`) and its modal
  lists that seat alone; no card, dot or Accounts seat quota tile shows the
  retired seat. Drop the entry from the config and reload: within one poll the
  dot, the modal and the Accounts tile for it are gone, and the Traffic figures
  are unchanged.
- Zero counts render faint: `0` in `eligible 0 of 2` and the seat driver's
  zero failure counts, never full brightness and never red.
- In the light theme, faint text (hints, labels, zeros) stays readable on
  both the page background and a card; the faint token clears 4.5:1 on each.

## UI review cases

The UI review of Now and Accounts runs over five cases. Each one is a small
config, a seeded ledger and the daemon state the case needs, built by the
panels' own builders in `ui_case_fixtures.rs` (test-only); no real account
or vendor is involved. `page_browser_cases_tests.rs` pins each case's
signature in headless Chrome:

| Case (`ROUTECTL_UI_CASE`) | What it shows |
|---|---|
| `empty` | An empty ledger and no subscription seat: Now reads `No requests yet` with the base URL and `No subscription login in this config.`; Accounts lists the API-key and cloud entries under `Not in a pool`. |
| `one-seat` | One seat alone in pool `main`, with two windows and traffic today. |
| `five-seat-pool` | Five seats in pool `seats`: two carry traffic (one window 95% used), three are idle (a stale reading, a seat never read, a window not started). |
| `logged-out` | Two seats in pool `seats`, `seat-b (b)` logged out for 40 min: its plain cause, its login fix and an `account_unusable` item on Now. |
| `poll-failing` | One seat whose usage poll has failed for 15 min: a `usage_poll_failing` item on Now. |

### Serving a case to a browser

The review server is an ignored test that serves the released page and one
case's payloads, built at the wall clock on every answer, on a loopback port
the OS picks. It binds `127.0.0.1` only, reads no config folder (the case
config is parsed from text and its ledger lives in a temp dir) and opens no
outbound connection. Its name has no `page_browser` in it, so the browser
test run never starts it. Run it from the repo root, one case at a time:

```
ROUTECTL_UI_CASE=logged-out \
  cargo test -p routectl-cli --lib ui_review_case_server -- --ignored --nocapture
```

It prints the address it serves and keeps serving until killed (Ctrl+C in
its terminal):

```
review case logged-out at http://127.0.0.1:<port>/
```

Browse that address normally:

```
agent-browser --session ui-review open http://127.0.0.1:<port>/#now
agent-browser --session ui-review open http://127.0.0.1:<port>/#accounts
```

Payloads come back unchanged between rounds (only ages move with the
clock), so a changed-value flash is not expected here; section 4 covers it
on a live daemon.

For each case check, on Now and on Accounts:

- Widths 720, 1024 and 1440 px (`agent-browser set viewport <w> 900`), and
  1440 px at 200% text: a browser profile whose `Default/Preferences` sets
  `webkit.webprefs.default_font_size` to 32 (`--profile <dir>`), not page
  zoom.
- Both themes (`agent-browser set media dark`, then `light`).
- Reduced motion (`agent-browser set media dark reduced-motion`): nothing
  animates and nothing is lost.
- The outage: with the page showing the case, block the aggregate
  (`agent-browser network route 'http://127.0.0.1:<port>/status' --abort`;
  or stop the server) and wait two polls. The status line keeps the last
  sentence, dimmed, followed by `as of <time> - the daemon cannot be
  reached`; tab badges keep their numbers dimmed, never an amber `stale`;
  the open view keeps its sections, dimmed. Lift the block
  (`agent-browser network unroute`) and the page recovers within a poll.
  Run it on the `logged-out` case, where the verdict and badges carry a
  problem.

No case carries API-key or cloud traffic: in `empty` those rows read `no
traffic today` like an idle seat. Their own request counts are checked on a
live daemon (section 7).

### Real-state checks on a hermetic daemon

What only a running daemon shows (a login landing, a seat logged out
against a credentials file, the drivers' first sweep) is checked on a
hermetic daemon that cannot reach the network or the operator's config:

```
# one network namespace with only loopback, held by a sleeping process
unshare -rn sh -c 'ip link set lo up; exec sleep infinity' &
HOLDER=$!
enter() { nsenter --preserve-credentials -t "$HOLDER" -U -n "$@"; }
enter curl -sS -m 3 http://1.1.1.1 ; echo "rc=$?"   # must FAIL: positive control

SCRATCH=$(mktemp -d)
mkdir -p "$SCRATCH/routectl"
# write "$SCRATCH/routectl/config.toml": `version = 5`, `[server] host =
# "127.0.0.1"` and a free `port`, a pool of `oauth://anthropic#<label>`
# members and one model on it; no credentials.json means every seat
# starts logged out
cargo build -p routectl-cli     # the binary lands in $CARGO_TARGET_DIR/debug
enter env -u ROUTECTL_CONFIG HOME="$SCRATCH" XDG_CONFIG_HOME="$SCRATCH" \
  XDG_DATA_HOME="$SCRATCH/data" "$CARGO_TARGET_DIR/debug/routectl" serve &
```

The namespace has its own loopback, so the host browser cannot reach the
daemon directly: read it from inside (`enter curl -s
http://127.0.0.1:<port>/status`), or run the browser there (`enter
agent-browser --session hermetic open http://127.0.0.1:<port>/`). Never point a built binary at the real
config folder, and never send anything to the live daemon's port.

Stop it by pid, never by name: find the pid listening on the port
(`enter ss -ltnp`), confirm it is this run with
`tr '\0' '\n' < /proc/<pid>/environ | grep '^XDG_CONFIG_HOME='` naming
`$SCRATCH`, then `kill <pid>`; finally `kill "$HOLDER"` and remove
`$SCRATCH`.
