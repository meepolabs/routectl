  // ---- tab:config ------------------------------------------------------

  // Config is the one tab that FAILS CLOSED as a whole: its vocabulary decides
  // how every other tab's routing figures are read, so a version mismatch or a
  // malformed payload replaces the entire tab rather than rendering the parts
  // that happen to parse. Single-source, so the tab-wide `safeSection` IS that
  // posture -- there is no second record to confine a failure to, and no
  // section is built before the whole payload has been validated below. The
  // one exception is a section whose rows cannot be drawn as given (two models
  // sharing a nickname): that is a fault of its rows, not of the vocabulary,
  // so it errors that section alone.
  function buildConfig(rec) {
    return safeSection(rec, buildConfigLive);
  }

  // Validate the WHOLE payload before any section is built: a config panel at
  // this version always carries all seven members, so a missing one is a
  // malformed same-version payload and the throw takes the tab with it. A
  // missing FIELD inside a row is a different matter -- the cells below report
  // it as unknown and keep the section.
  function buildConfigLive(rec) {
    var data = rec.data || {};
    var lists = [data.aliases, data.models, data.classes, data.capabilities,
      data.providers, data.activation];
    if (!data.source || lists.some(function (l) { return !Array.isArray(l); })) {
      throw new Error('config payload does not carry the effective view');
    }
    var stack = tabStack();
    stack.appendChild(sourceStrip(data.source));
    // The default-state card claims nothing is configured, so ANY
    // operator-authored surface below it makes it a contradiction -- a named
    // provider is in effect even before an alias or model points at it.
    if (!data.aliases.length && !data.models.length && !data.providers.length) {
      stack.appendChild(defaultConfigEmpty());
    }
    // A table with no rows is a header claiming a reading nobody made, so each
    // one is built only when the effective view carries entries for it. The
    // class policies always resolve (a baked default per class), and the
    // activation inventory describes what routectl WOULD do, so both stay
    // useful on a config that names nothing yet.
    appendIfAny(stack, data.aliases, aliasSection);
    appendIfAny(stack, data.models, function (models) {
      return safeSection(rec, function () { return modelSection(models); });
    });
    appendIfAny(stack, data.providers, providerSection);
    // `pools` joined the payload within its schema version, so a payload
    // without it is still this version and simply names no pool.
    appendIfAny(stack, Array.isArray(data.pools) ? data.pools : [], poolSection);
    appendIfAny(stack, data.capabilities, capabilitySection);
    appendIfAny(stack, data.activation, activationSection);
    appendIfAny(stack, data.classes, classSection);
    return stack;
  }

  function appendIfAny(stack, list, build) {
    if (list.length) { stack.appendChild(build(list)); }
  }

  // Nothing operator-authored is in effect. Welcoming, and specific about the
  // one edit that fills the tables above.
  function defaultConfigEmpty() {
    return emptyCard('Running on the default configuration',
      'No providers, aliases, or models are configured yet. Name one under [providers] and ' +
      'its endpoint and limits appear here; add an alias and its resolved chain and its ' +
      'targets appear too.',
      ['current state', 'defaults in effect']);
  }

  // ---- config: the source strip ----------------------------------------

  // Which config is in effect and which daemon is serving it -- the first
  // question the tab exists to answer, so it sits above every table.
  function sourceStrip(source) {
    var strip = document.createElement('div');
    strip.className = 'cfgstrip';
    strip.appendChild(configPathFact(source.config_path));
    [
      ['loaded', loadedAge(source.loaded_age_ms)],
      ['resolved', countText(source.alias_count, ' alias', ' aliases') + ', ' +
        countText(source.provider_count, ' provider', ' providers')],
      ['listening on', source.listen_addr],
      ['version', source.version]
    ].forEach(function (fact) { strip.appendChild(cfgFact(fact[0], fact[1])); });
    return strip;
  }

  function cfgFact(label, value) {
    var v = document.createElement('span');
    v.textContent = (value === null || value === undefined || value === '') ? '-' : String(value);
    v.title = v.textContent;
    return factRow(label, v);
  }

  function factRow(label, valueNode) {
    var wrap = document.createElement('div');
    wrap.className = 'cfgfact';
    var l = document.createElement('span');
    l.className = 'cfgfact-label';
    l.textContent = label;
    valueNode.classList.add('cfgfact-value');
    wrap.appendChild(l);
    wrap.appendChild(valueNode);
    return wrap;
  }

  // The path is copied, so it is never cut: it takes a row of its own and
  // breaks only after a slash. Each piece ends at a slash and is drawn
  // unbreakable unless it alone is wider than the row; the pieces are text
  // in one element, so a selection copies the path exactly.
  function configPathFact(path) {
    var v = document.createElement('span');
    if (typeof path === 'string' && path !== '') {
      pathPieces(path).forEach(function (piece, i) {
        if (i > 0) { v.appendChild(document.createElement('wbr')); }
        var span = document.createElement('span');
        span.className = 'cfgpath-piece';
        span.textContent = piece;
        v.appendChild(span);
      });
    } else {
      v.textContent = 'none (not loaded from a file)';
    }
    var fact = factRow('config file', v);
    fact.classList.add('cfgfact--path');
    return fact;
  }

  // Built from its char code: page.rs rejects a quoted literal starting
  // with a slash.
  var PATH_SLASH = String.fromCharCode(47);

  // `path` cut after each slash that ends a named segment, so a run of
  // slashes (the root's included) stays with the segment after it.
  function pathPieces(path) {
    var pieces = [];
    var piece = '';
    var named = false;
    for (var i = 0; i < path.length; i++) {
      var ch = path.charAt(i);
      piece += ch;
      if (ch !== PATH_SLASH) {
        named = true;
      } else if (named) {
        pieces.push(piece);
        piece = '';
        named = false;
      }
    }
    if (piece !== '') { pieces.push(piece); }
    return pieces;
  }

  // A load that was never stamped reads as unknown rather than as an age of
  // zero; a stamp ahead of the clock is clamped by humanDuration's caller.
  function loadedAge(ms) {
    var x = Number(ms);
    if (ms === null || ms === undefined || !isFinite(x) || x < 0) { return 'not stamped'; }
    return humanDuration(x) + ' ago';
  }

  function countText(n, one, many) {
    var x = num0(n);
    return x + (x === 1 ? one : many);
  }

  // ---- config: the reference tables ------------------------------------

  // A configured alias and the ordered chain it resolves to. The chain order IS
  // the sequence dispatch walks, so it is listed verbatim. There is deliberately
  // NO per-alias timeout or retry-policy column: neither exists in the config
  // model, and a column of invented values would read as configuration.
  function aliasSection(aliases) {
    var tbl = mkTable('config-aliases', 'Configured aliases',
      [R('alias'), W('resolved chain'), N('steps')]);
    aliases.forEach(function (entry) {
      var steps = Array.isArray(entry.chain) ? entry.chain : [];
      trow(tbl, [entry.alias, steps.length ? steps.join(' then ') : null, steps.length]);
    });
    return card('Aliases', 'what a client name resolves to, in order', tableScroll(tbl));
  }

  // The auto-activation inventory: one row per routectl-owned provider, with
  // the reason a provider could not be resolved. Named CREDENTIAL providers
  // because the source strip counts the ROUTING providers instead, and one tab
  // carrying two unqualified "providers" reads as one number contradicting the
  // other.
  function activationSection(activation) {
    var tbl = mkTable('config-activation', 'Provider activation',
      [R('provider'), C('kind'), C('status'), C('reason'), C('used by aliases')]);
    activation.forEach(function (p) {
      trow(tbl, [
        p.provider_id,
        p.provider_kind,
        tokLabeled('actstatus', 'actstatus', p.status),
        p.reason ? labelCell('activation', p.reason) : null,
        yesNo(p.referenced_by_aliases)
      ]);
    });
    var wrap = card('Credential providers', 'which credentials resolved, and why',
      tableScroll(tbl));
    wrap.appendChild(activationNote());
    return wrap;
  }

  // Both provider tables sit on this tab now, so a footnote merely asserting
  // "a different set" leaves an operator to guess how they relate. Neither
  // contains the other: this inventory is the FIXED universe of routectl-owned
  // OAuth candidates (the server probes every one of them regardless of what
  // the config names), while the routing table lists exactly what the config
  // names. An id can appear here and route nowhere, and a routing provider
  // authenticating by env key never appears here at all.
  function activationNote() {
    var note = document.createElement('p');
    note.className = 'footnote';
    note.textContent = 'Credential activation only: every provider routectl can own an OAuth ' +
      'credential for, probed whether or not the config names it. Not the same set as the ' +
      'routing providers above, which is what the provider count in the strip counts.';
    return note;
  }

  // One row per `[models.X]` entry. The model facts (window, output fill,
  // prices, each with its source) sit in the detail row: they are reference figures an operator opens, not a reading to
  // scan.
  // Sortable by its three visible columns and keyed by nickname, which the
  // config makes unique; a payload breaking that throws in fillRows.
  function modelSection(models) {
    var tbl = mkTable('config-models', 'Configured models', [
      sortBy(R('model'), 'model', function (m) { return textSortValue(m.nickname); }),
      sortBy(C('provider'), 'provider', function (m) { return textSortValue(m.provider); }),
      sortBy(C('upstream'), 'upstream', function (m) { return textSortValue(m.upstream); })
    ], { expandable: true, sortable: true, rowKey: function (m) { return m.nickname; } });
    tbl.t.classList.add('cfgmodels');
    fillRows(tbl, models, function (m, key) { modelRow(tbl, m, key); });
    return card('Models', 'what each name dispatches to, and where its facts come from',
      tableScroll(tbl));
  }

  function textSortValue(v) {
    return typeof v === 'string' ? v : null;
  }

  function modelRow(tbl, m, key) {
    var prices = Array.isArray(m.prices) ? m.prices : [];
    xrow(tbl, key,
      [m.nickname, m.provider, m.upstream],
      presentPairs([
        ['provider kind', m.provider_kind],
        ['context window', windowText(m.context_window)],
        ['output fill', fillText(m.fill)]
      ]).concat(prices.map(pricePair)));
  }

  // The window has one source, the model's own config; an unset one is a fact
  // the gate and /v1/models act on, so it reads as such rather than vanishing.
  function windowText(tokens) {
    if (tokens === null || tokens === undefined) { return 'not configured'; }
    return humanCount(tokens) + ' (model-config)';
  }

  // An absent fill means the egress forwards an omitted limit as omitted.
  function fillText(fill) {
    if (!fill) { return 'not filled'; }
    return humanCount(fill.tokens) + ' (' + fill.source + ')';
  }

  // One price field: its figure and source, the table date beside a table
  // source. An unpriced field reads "unknown", never zero. A lifetime-keyed
  // field (`cache_write_per_mtok.30m`) reads like the fixed ones
  // (`cache write 30m`).
  function priceLabel(field) {
    var parts = String(field).split('.');
    var base = parts[0].replace(/_per_mtok$/, '').replace(/_/g, ' ');
    return (parts.length > 1 ? base + ' ' + parts.slice(1).join('.') : base) + ' $/Mtok';
  }

  function pricePair(p) {
    var label = priceLabel(p.field);
    if (p.per_mtok === null || p.per_mtok === undefined) { return [label, 'unknown']; }
    var source = p.table_date ? p.source + ' ' + p.table_date : p.source;
    return [label, String(p.per_mtok) + ' (' + source + ')'];
  }

  // A model with nothing configured carries none of these figures, and a
  // grid of five dashes is indistinguishable from an expander that failed to
  // open. Dropping the absent pairs lets `buildDefList` say so explicitly.
  function presentPairs(pairs) {
    return pairs.filter(function (p) {
      return p[1] !== null && p[1] !== undefined && p[1] !== '';
    });
  }

  // ---- config: the seat pools ------------------------------------------

  // One row per `[pools.X]` entry: the selection strategy as the config
  // spells it, and the members in configured order. Only fill-first tries
  // them in that order, so only its list is ordered and says so;
  // sticky-least-loaded places by session and pace and walks that order only
  // as its fall-back, so its list does not announce positions. A member is
  // named as every other view names a seat, "entry (label)"; the account key
  // is never parsed for either.
  function poolSection(pools) {
    var tbl = mkTable('config-pools', 'Configured seat pools',
      [R('pool'), C('seat selection'), W('members')]);
    pools.forEach(function (p) {
      trow(tbl, [p.pool, p.seat_selection, poolMembersCell(p)]);
    });
    return card('Pools', 'which provider entries share a pool, and how a request picks one',
      tableScroll(tbl));
  }

  // The wire's `seat_selection` token for the one strategy that drains its
  // members in the order the config lists them.
  var FILL_FIRST_SELECTION = 'fill-first';

  function poolMembersCell(pool) {
    var ordered = pool.seat_selection === FILL_FIRST_SELECTION;
    var cell = document.createDocumentFragment();
    cell.appendChild(poolMembers(pool.members, ordered));
    if (ordered) {
      var note = document.createElement('span');
      note.className = 'cfgpool-order';
      note.textContent = 'tried in the order listed';
      cell.appendChild(note);
    }
    return cell;
  }

  function poolMembers(members, ordered) {
    var list = document.createElement(ordered ? 'ol' : 'ul');
    list.className = 'cfgpool-members';
    (Array.isArray(members) ? members : []).forEach(function (m) {
      var item = document.createElement('li');
      item.className = 'cfgpool-member';
      var label = typeof m.login_label === 'string' && m.login_label !== '' ? m.login_label : null;
      var entry = document.createElement('span');
      entry.className = 'cfgpool-entry';
      entry.textContent = (m.entry === null || m.entry === undefined)
        ? '-' : seatName(m.entry, label);
      item.appendChild(entry);
      if (label === null) { item.appendChild(memberNoLogin()); }
      list.appendChild(item);
    });
    return list;
  }

  // An entry with no OAuth label (an API-key entry, or a login no model
  // reaches) says so rather than leaving a gap that reads as a missing field.
  function memberNoLogin() {
    var span = document.createElement('span');
    span.className = 'cfgpool-login mag-zero';
    span.textContent = 'no login label';
    return span;
  }

  // ---- config: the routing providers -----------------------------------

  // One row per `[providers.X]` entry: the routing shape an operator can act
  // on. Deliberately NOT a credential view -- the server sends the endpoint
  // ORIGIN (a configured base_url may embed a key in userinfo, path, or query)
  // and the credential ref's SCHEME only, so there is nothing here to widen
  // back into a fuller URL or a ref body.
  function providerSection(providers) {
    var tbl = mkTable('config-providers', 'Configured routing providers',
      [R('provider'), C('format'), C('kind'), C('endpoint'), C('auth'), C('credential ref'),
        N('rate limit')]);
    providers.forEach(function (p) {
      trow(tbl, [
        p.provider_id,
        configuredText(p.format, 'none'),
        p.provider_kind,
        configuredText(p.endpoint_origin || p.cloud, 'none'),
        configuredText(p.auth_token, 'none'),
        configuredText(p.credential_ref_scheme, 'none'),
        rpmLimitCell(p.rpm_limit)
      ]);
    });
    var wrap = card('Routing providers', 'what each configured provider dispatches to',
      tableScroll(tbl));
    wrap.appendChild(providerNote());
    return wrap;
  }

  function providerNote() {
    var note = document.createElement('p');
    note.className = 'footnote';
    note.textContent = 'Endpoints are the CONFIGURED origin (scheme, host, port); a cloud ' +
      'entry shows its cloud and region and derives its endpoint at startup. A rate limit of ' +
      '0 RPM means requests are throttled immediately. A limit not set here may still come from a pool or default; ' +
      'config show --effective shows the resolved value and its source.';
    return note;
  }

  // A configured-or-absent text cell. An absent value is a real reading (the
  // config carries none), so it reads as a faint word rather than a dash that
  // could equally mean the field failed to arrive.
  function configuredText(value, absentWord) {
    if (value === null || value === undefined || value === '') {
      return faintFigure(absentWord);
    }
    return String(value);
  }

  // The rate limit, whose THREE states are not interchangeable and whose
  // null-check ORDER is the whole correctness of the cell:
  //
  //   absent field -> 'unknown'      the payload never carried a reading
  //   null         -> 'not set here' this entry sets no cap of its own; a
  //                                  pool or default may still apply, so
  //                                  the cell never claims no cap at all
  //   a number     -> '<n> RPM'      INCLUDING 0, which seeds a
  //                                  zero-capacity bucket and therefore
  //                                  means immediately rate-limited
  //
  // The checks run BEFORE any numeric coercion for that last reason. A
  // truthiness test (`rpm ? n : 'not set here'`) reports a throttled provider as
  // unrestricted because 0 is falsy; running the value through `num0` first
  // reports an absent field as '0 RPM' because it coerces non-finite to 0.
  // Both misreports are one helper call away, so this is the ONE place the
  // field is formatted.
  //
  // A cap of 0 also has to LOOK different from an absence: `figure` faints any
  // '0' on the premise that a zero measurement is nothing to see, which is
  // exactly wrong for a cap that sheds every request, so this cell overrides
  // that weighting.
  function rpmLimitCell(rpm) {
    if (rpm === undefined) { return faintFigure('unknown'); }
    if (rpm === null) {
      var unset = faintFigure('not set here');
      unset.title = 'a pool or default may apply; config show --effective shows the resolved value';
      return unset;
    }
    var x = Number(rpm);
    if (!isFinite(x)) { return faintFigure('unknown'); }
    var cell = figure(String(Math.floor(x)), 'RPM', null);
    if (x <= 0) {
      cell.classList.remove('mag-zero');
      cell.classList.add('neg');
      cell.title = 'a cap of 0 rejects every request immediately';
    }
    return cell;
  }

  // The capability overrides the config resolves, kept behind a default-closed
  // disclosure: a real config carries few, and none of them changes what the
  // tables above say. Reachable (data floor), never dominant.
  function capabilitySection(caps) {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Capability overrides',
      countText(caps.length, ' cell', ' cells') + ' resolved from config'));
    var tbl = mkTable('config-capabilities', 'Capability overrides',
      [R('target'), C('capability'), C('verdict'), C('source')]);
    caps.forEach(function (c) {
      trow(tbl, [
        c.target_spec,
        c.capability_key,
        tokLabeled('verdict', 'verdict', c.verdict),
        tokLabeled('prov', 'prov', c.provenance)
      ]);
    });
    wrap.appendChild(buildExpander('config-capabilities-cells', 'Show the resolved cells',
      tableScroll(tbl)));
    return wrap;
  }

  // What routectl does when an upstream returns each failure class. The columns
  // are exactly the four knobs the config model HAS -- retry cap, fallback,
  // whether the class debits the breaker's health accounting, and the layer
  // that won. `breaker debit` reads the wire's own `debits_breaker`, which the
  // server derives from the router's transient-health set: this page must never
  // restate that set, and it invents no per-class knob beside it.
  function classSection(classes) {
    var tbl = mkTable('config-classes', 'Retry class policy',
      [R('class'), N('retry cap'), C('fallback'), C('breaker debit'), C('source')]);
    classes.forEach(function (c) {
      trow(tbl, [
        labelCell('class', c.class),
        num0(c.retry_cap),
        yesNo(c.fallback),
        yesNo(c.debits_breaker),
        classSourceCell(c.source)
      ]);
    });
    return card('Retry classes', 'what routectl does when an upstream returns each class',
      tableScroll(tbl));
  }

  // A baked default is the ABSENCE of an operator choice, so it reads faint;
  // a class the config names reads at full weight.
  function classSourceCell(source) {
    var span = document.createElement('span');
    span.textContent = (source === null || source === undefined) ? '-' : String(source);
    if (source !== 'config') { span.classList.add('mag-zero'); }
    return span;
  }

  // A configured boolean. The negative reads faint rather than red: an unset
  // switch is a choice, not a failure.
  function yesNo(on) {
    var span = document.createElement('span');
    span.textContent = on ? 'yes' : 'no';
    if (!on) { span.classList.add('mag-zero'); }
    return span;
  }
  // ---- end tab:config --------------------------------------------------

