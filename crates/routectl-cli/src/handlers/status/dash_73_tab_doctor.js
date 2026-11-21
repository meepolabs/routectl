  // ---- tab:doctor ------------------------------------------------------

  // The report's severity vocabulary is a fixed triad. A token outside it
  // means the wire's meaning has moved, and an unknown severity cannot be
  // sorted into "a problem" or "fine" without guessing -- so the tab fails
  // closed on one rather than interpreting it. `LABELS.status` carries the
  // humanized text for exactly these three.
  var DOCTOR_SEVERITIES = { Pass: 1, Warn: 1, Fail: 1 };

  // The reachability verdicts the panel folds from each target's last settled
  // outcome, worst-known-first for the summary line. Same fail-closed posture
  // as the severities above.
  var REACHABILITY_TOKENS = ['reachable', 'degraded', 'unknown'];

  // Doctor is the second tab that FAILS CLOSED as a whole: an unrecognized
  // severity would have to be guessed at to be placed, and a diagnostic that
  // guesses is worse than one that says it cannot read the report. Single-
  // source, so the tab-wide `safeSection` IS that posture -- the whole payload
  // is validated below before any section is built.
  //
  // The report also carries structured panels, none of which renders here:
  // the steady-state trim panel is not a check with a verdict, and what each
  // lane has learned is the Models view's subject. Below the checks sit the
  // daemon's live capability-write failure counters, which say whether what
  // it learns can survive a restart.
  function buildDoctor(rec) {
    return safeSection(rec, buildDoctorLive);
  }

  function buildDoctorLive(rec) {
    var report = validatedReport(rec.data);
    var fails = findingsWith(report.findings, 'Fail');
    var warns = findingsWith(report.findings, 'Warn');
    var passes = findingsWith(report.findings, 'Pass');
    var attention = fails.concat(warns);
    var counts = { fail: fails.length, warn: warns.length, pass: passes.length };
    var stack = tabStack();
    stack.appendChild(doctorVerdict(counts, rec.data.reachability));
    if (attention.length) { stack.appendChild(doctorFindings(attention)); }
    else if (passes.length) { stack.appendChild(doctorAllClear(passes.length)); }
    else { stack.appendChild(doctorNoChecks()); }
    if (passes.length) { stack.appendChild(doctorPasses(passes)); }
    stack.appendChild(doctorCapWrites(rec.data.capability_writes));
    return stack;
  }

  // Validate the WHOLE payload before a section is built, INCLUDING every
  // severity and reachability token: a value outside the known vocabulary is
  // not an additive change (those do not bump a version), so reading past it
  // would render a finding whose meaning this build does not know.
  function validatedReport(data) {
    var report = (data || {}).report;
    var reachability = (data || {}).reachability;
    if (!report || !Array.isArray(report.findings) || !Array.isArray(reachability)) {
      throw new Error('doctor payload does not carry a report and its reachability');
    }
    report.findings.forEach(function (f) {
      if (!f || !DOCTOR_SEVERITIES[f.status]) {
        throw new Error('doctor finding carries a severity this build cannot read');
      }
    });
    reachability.forEach(function (t) {
      if (!t || REACHABILITY_TOKENS.indexOf(t.reachability) < 0) {
        throw new Error('doctor reachability carries a verdict this build cannot read');
      }
    });
    return report;
  }

  // Server order is preserved WITHIN a severity (section, then name), so the
  // findings list reads failures first without resorting the report.
  function findingsWith(findings, status) {
    return findings.filter(function (f) { return f.status === status; });
  }

  // ---- doctor: the verdict card ----------------------------------------

  function doctorVerdict(counts, reachability) {
    var wrap = markRefreshUnit(document.createElement('div'), 'verdict');
    wrap.className = 'card docverdict';
    wrap.appendChild(doctorVerdictLead(counts, reachability));
    wrap.appendChild(doctorCounts(counts));
    return wrap;
  }

  function doctorVerdictLead(counts, reachability) {
    var lead = document.createElement('div');
    lead.className = 'docverdict-lead';
    var dot = document.createElement('span');
    dot.className = 'docdot docdot--' + verdictTone(counts);
    dot.setAttribute('aria-hidden', 'true');
    var text = document.createElement('div');
    text.className = 'docverdict-text';
    text.appendChild(docText('docverdict-head', verdictHeadline(counts)));
    text.appendChild(docText('docverdict-sub',
      'no-network checks - ' + reachSummary(reachability)));
    lead.appendChild(dot);
    lead.appendChild(text);
    return lead;
  }

  // A report nobody could produce a check for is UNKNOWN, never clear: zero
  // findings is an absence of evidence, not a clean bill of health.
  function verdictTone(counts) {
    if (counts.fail) { return 'bad'; }
    if (counts.warn) { return 'warn'; }
    return counts.pass ? 'ok' : 'unknown';
  }

  function verdictHeadline(counts) {
    if (counts.fail) {
      return countText(counts.fail, ' check failed', ' checks failed') +
        ' - routing stays degraded until they are fixed';
    }
    if (counts.warn) {
      return countText(counts.warn, ' warning', ' warnings') + ' - nothing is broken yet';
    }
    return counts.pass ? 'Everything checks out' : 'No checks reported';
  }

  // The reachability rollup, which is the ONLY reachability reading on the
  // page: the per-target last outcome it folds is the Models view's subject, so
  // repeating those rows here would show the same fact twice.
  function reachSummary(reachability) {
    if (!reachability.length) { return 'no dispatch target resolved yet'; }
    var parts = [];
    REACHABILITY_TOKENS.forEach(function (token) {
      var n = reachability.filter(function (t) { return t.reachability === token; }).length;
      if (n) { parts.push(n + ' ' + token); }
    });
    return 'last settled outcome: ' + parts.join(', ');
  }

  function doctorCounts(counts) {
    var row = document.createElement('div');
    row.className = 'doccounts';
    [
      ['failed', counts.fail, 'bad'],
      ['warnings', counts.warn, 'warn'],
      ['passed', counts.pass, null]
    ].forEach(function (c) { row.appendChild(doctorCount(c[0], c[1], c[2])); });
    return row;
  }

  // A zero count reads faint: no failures is the good state, so it must not
  // carry the failure color.
  function doctorCount(label, value, tone) {
    var cell = document.createElement('div');
    cell.className = 'doccount';
    cell.appendChild(docText('doccount-label', label));
    var v = docText('doccount-value', String(value));
    if (!value) { v.classList.add('mag-zero'); }
    else if (tone) { v.classList.add('doccount-value--' + tone); }
    cell.appendChild(v);
    return cell;
  }

  // ---- doctor: findings + passes ---------------------------------------

  function doctorFindings(findings) {
    var wrap = markRefreshUnit(document.createElement('div'), 'findings');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Findings',
      countText(findings.length, ' check needs attention', ' checks need attention')));
    var list = document.createElement('div');
    list.className = 'doclist';
    var seen = Object.create(null);
    findings.forEach(function (f) {
      list.appendChild(doctorFinding(f, doctorItemId('doctor-finding', f, seen)));
    });
    wrap.appendChild(list);
    return wrap;
  }

  // A check's id comes from its own record, never its position: the lists
  // reorder as severities change, and a card matched by position would carry
  // a selection inside it over to another check. Section and name alone do
  // not identify a check (every config validation error shares them), so
  // the detail and the remediation join them; severity stays out, so a check
  // that turns from warning to failure keeps its card. Only byte-identical
  // records fall back to their order among themselves.
  function doctorItemId(prefix, f, seen) {
    var base = prefix + '-' + idKey(f.section) + '-' + idKey(f.name) + '-' + idKey(f.detail);
    if (f.remediation) { base += '-' + idKey(f.remediation); }
    return uniqueItemId(base, seen);
  }

  // One card per finding: the severity, what was checked, what was found, and
  // the remediation when the check carries one. A finding with no remediation
  // renders no empty slot for it.
  function doctorFinding(f, id) {
    var cardEl = document.createElement('div');
    cardEl.id = id;
    cardEl.className = 'docfinding' + (f.status === 'Fail' ? ' docfinding--fail' : '');
    cardEl.appendChild(doctorFindingHead(f));
    cardEl.appendChild(docText('docfinding-detail', f.detail));
    if (f.remediation) { cardEl.appendChild(doctorFix(f.remediation)); }
    return cardEl;
  }

  function doctorFindingHead(f) {
    var head = document.createElement('div');
    head.className = 'docfinding-head';
    head.appendChild(tokLabeled('status', 'status', f.status));
    head.appendChild(docText('docfinding-name', f.name));
    head.appendChild(docText('docfinding-section', f.section));
    return head;
  }

  function doctorFix(remediation) {
    var wrap = document.createElement('div');
    wrap.className = 'docfix';
    wrap.appendChild(docText('docfix-label', 'remediation'));
    wrap.appendChild(docText('docfix-text', remediation));
    return wrap;
  }

  // The passing checks stay reachable (data floor) behind a default-closed
  // disclosure: they are the report's bulk and none of them is something to
  // act on, so they must not push the findings off the screen.
  function doctorPasses(passes) {
    var wrap = markRefreshUnit(document.createElement('div'), 'passes');
    wrap.className = 'card';
    var grid = document.createElement('div');
    grid.className = 'docpassgrid';
    var seen = Object.create(null);
    passes.forEach(function (p) {
      grid.appendChild(doctorPass(p, doctorItemId('doctor-pass', p, seen)));
    });
    wrap.appendChild(buildExpander('doctor-passes',
      countText(passes.length, ' check passed', ' checks passed'), grid));
    return wrap;
  }

  function doctorPass(p, id) {
    var row = document.createElement('div');
    row.id = id;
    row.className = 'docpass';
    var dot = document.createElement('span');
    dot.className = 'docpass-dot';
    dot.setAttribute('aria-hidden', 'true');
    row.appendChild(dot);
    row.appendChild(docText('docpass-name', p.name));
    row.appendChild(docText('docpass-detail', p.detail));
    return row;
  }

  // Nothing to act on. Welcoming, and explicit that the passing checks below
  // are what the verdict rests on.
  function doctorAllClear(passCount) {
    return emptyCard('Nothing needs attention',
      countText(passCount, ' check ran', ' checks ran') + ' and none of them reported a ' +
      'problem. A warning or a failure appears here the moment one does.',
      ['current state', 'no upstream was dialed']);
  }

  // A report with no check at all: nothing is known to be wrong, and nothing
  // has been verified either. Said in those words rather than as an all-clear.
  function doctorNoChecks() {
    return emptyCard('No checks reported',
      'The report came back without a single check. Nothing is known to be wrong here, ' +
      'and nothing has been confirmed healthy either.',
      ['current state']);
  }

  // ---- doctor: capability-write counters -------------------------------

  // Three counters, never summed: a full channel is load, a write error is a
  // store fault, an unavailable writer is teardown -- each sends an operator
  // somewhere different. A zero reads faint; a nonzero one carries the
  // attention tone, because any of them means a learned verdict may not
  // survive a restart.
  var CAP_WRITE_COUNTERS = [
    ['capability_events_dropped_full', 'dropped (channel full)'],
    ['write_errors', 'write errors'],
    ['capability_writer_unavailable', 'writer unavailable']
  ];

  function doctorCapWrites(writes) {
    var w = writes || {};
    var row = document.createElement('div');
    row.className = 'doccounts';
    CAP_WRITE_COUNTERS.forEach(function (c) {
      row.appendChild(doctorCount(c[1], num0(w[c[0]]), 'bad'));
    });
    return card('Capability writes',
      'since this daemon started - nonzero means a learned verdict may not survive a restart',
      row);
  }

  function docText(cls, value) {
    var span = document.createElement('span');
    span.className = cls;
    span.textContent = (value === null || value === undefined || value === '')
      ? '-' : String(value);
    return span;
  }
  // ---- end tab:doctor --------------------------------------------------
