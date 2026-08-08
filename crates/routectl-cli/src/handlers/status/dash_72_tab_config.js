  // ---- tab:config ------------------------------------------------------

  // Config is the one tab that FAILS CLOSED as a whole: its vocabulary decides
  // how every other tab's routing figures are read, so a version mismatch or a
  // malformed payload replaces the entire tab rather than rendering the parts
  // that happen to parse. Single-source, so the tab-wide `safeSection` IS that
  // posture -- there is no second record to confine a failure to, and no
  // section is built before the whole payload has been validated below.
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
    appendIfAny(stack, data.models, modelSection);
    appendIfAny(stack, data.providers, providerSection);
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
      'its endpoint and limits appear here; add an alias and its resolved chain, its targets, ' +
      'and the catalog rows behind them appear too.',
      ['current state', 'defaults in effect']);
  }

  // ---- config: the source strip ----------------------------------------

  // Which config is in effect and which daemon is serving it -- the first
  // question the tab exists to answer, so it sits above every table.
  function sourceStrip(source) {
    var strip = document.createElement('div');
    strip.className = 'cfgstrip';
    [
      ['config file', source.config_path || 'none (not loaded from a file)'],
      ['loaded', loadedAge(source.loaded_age_ms)],
      ['resolved', countText(source.alias_count, ' alias', ' aliases') + ', ' +
        countText(source.provider_count, ' provider', ' providers')],
      ['listening on', source.listen_addr],
      ['version', source.version]
    ].forEach(function (fact) { strip.appendChild(cfgFact(fact[0], fact[1])); });
    return strip;
  }

  function cfgFact(label, value) {
    var wrap = document.createElement('div');
    wrap.className = 'cfgfact';
    var l = document.createElement('span');
    l.className = 'cfgfact-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'cfgfact-value';
    v.textContent = (value === null || value === undefined || value === '') ? '-' : String(value);
    v.title = v.textContent;
    wrap.appendChild(l);
    wrap.appendChild(v);
    return wrap;
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
    var tbl = mkTable('Configured aliases',
      [R('alias'), W('resolved chain'), N('steps')], false);
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
    var tbl = mkTable('Provider activation',
      [R('provider'), C('kind'), C('status'), C('reason'), C('used by aliases')], false);
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

  // One row per `[models.X]` entry with the catalog layer that won it. The
  // economics and the confirmed context window sit in the detail row: they are
  // reference figures an operator opens, not a reading to scan.
  function modelSection(models) {
    var tbl = mkTable('Configured models',
      [R('model'), C('provider'), C('upstream'), C('catalog layer')], true);
    models.forEach(function (m) { modelRow(tbl, m); });
    return card('Models', 'what each name dispatches to, and which catalog row backs it',
      tableScroll(tbl));
  }

  function modelRow(tbl, m) {
    var econ = m.economics || {};
    xrow(tbl, m.nickname,
      [m.nickname, m.provider, m.upstream, tokLabeled('src', 'src', m.source)],
      presentPairs([
        ['provider kind', m.provider_kind],
        ['max context', econ.max_context_tokens ? humanCount(econ.max_context_tokens) : null],
        ['cache write multiplier', econ.wm],
        ['cache read multiplier', econ.rm],
        ['verified at', m.verified_at]
      ]));
  }

  // A model with no catalog row behind it carries none of these figures, and a
  // grid of five dashes is indistinguishable from an expander that failed to
  // open. Dropping the absent pairs lets `buildDefList` say so explicitly.
  function presentPairs(pairs) {
    return pairs.filter(function (p) {
      return p[1] !== null && p[1] !== undefined && p[1] !== '';
    });
  }

  // ---- config: the routing providers -----------------------------------

  // One row per `[providers.X]` entry: the routing shape an operator can act
  // on. Deliberately NOT a credential view -- the server sends the endpoint
  // ORIGIN (a configured base_url may embed a key in userinfo, path, or query)
  // and the credential ref's SCHEME only, so there is nothing here to widen
  // back into a fuller URL or a ref body.
  function providerSection(providers) {
    var tbl = mkTable('Configured routing providers',
      [R('provider'), C('kind'), C('endpoint'), C('auth'), C('credential ref'), N('rate limit')],
      false);
    providers.forEach(function (p) {
      trow(tbl, [
        p.provider_id,
        p.provider_kind,
        configuredText(p.endpoint_origin, 'derived at startup'),
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
    note.textContent = 'Endpoints are the CONFIGURED origin (scheme, host, port); a kind that ' +
      'derives its endpoint at startup shows none. A rate limit of 0 RPM means requests are ' +
      'throttled immediately, which is not the same as unlimited.';
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
  //   absent field -> 'unknown'   the payload never carried a reading
  //   null         -> 'unlimited' no cap is configured
  //   a number     -> '<n> RPM'   INCLUDING 0, which seeds a zero-capacity
  //                               bucket and therefore means immediately
  //                               rate-limited
  //
  // The checks run BEFORE any numeric coercion for that last reason. A
  // truthiness test (`rpm ? n : 'unlimited'`) reports a throttled provider as
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
    if (rpm === null) { return faintFigure('unlimited'); }
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
    var tbl = mkTable('Capability overrides',
      [R('target'), C('capability'), C('verdict'), C('source')], false);
    caps.forEach(function (c) {
      trow(tbl, [
        c.target_spec,
        c.capability_key,
        tokLabeled('verdict', 'verdict', c.verdict),
        tokLabeled('prov', 'prov', c.provenance)
      ]);
    });
    wrap.appendChild(buildExpander('Show the resolved cells', tableScroll(tbl)));
    return wrap;
  }

  // What routectl does when an upstream returns each failure class. The columns
  // are exactly the four knobs the config model HAS -- retry cap, fallback,
  // whether the class debits the breaker's health accounting, and the layer
  // that won. `breaker debit` reads the wire's own `debits_breaker`, which the
  // server derives from the router's transient-health set: this page must never
  // restate that set, and it invents no per-class knob beside it.
  function classSection(classes) {
    var tbl = mkTable('Retry class policy',
      [R('class'), N('retry cap'), C('fallback'), C('breaker debit'), C('source')], false);
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

