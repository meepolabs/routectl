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
  // The report also carries structured panels (steady-state trim, capability
  // matrix). Neither is a check with a verdict, so neither belongs among the
  // findings; the capability cells the config resolves already have their home
  // on the Config tab.
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
    var wrap = document.createElement('div');
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
  // page: the per-target last outcome it folds is the Health tab's subject, so
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
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Findings',
      countText(findings.length, ' check needs attention', ' checks need attention')));
    var list = document.createElement('div');
    list.className = 'doclist';
    findings.forEach(function (f) { list.appendChild(doctorFinding(f)); });
    wrap.appendChild(list);
    return wrap;
  }

  // One card per finding: the severity, what was checked, what was found, and
  // the remediation when the check carries one. A finding with no remediation
  // renders no empty slot for it.
  function doctorFinding(f) {
    var cardEl = document.createElement('div');
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
    var wrap = document.createElement('div');
    wrap.className = 'card';
    var grid = document.createElement('div');
    grid.className = 'docpassgrid';
    passes.forEach(function (p) { grid.appendChild(doctorPass(p)); });
    wrap.appendChild(buildExpander(
      countText(passes.length, ' check passed', ' checks passed'), grid));
    return wrap;
  }

  function doctorPass(p) {
    var row = document.createElement('div');
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

  function docText(cls, value) {
    var span = document.createElement('span');
    span.className = cls;
    span.textContent = (value === null || value === undefined || value === '')
      ? '-' : String(value);
    return span;
  }
  // ---- end tab:doctor --------------------------------------------------

  var BUILDERS = {
    overview: buildOverview,
    usage: buildUsage,
    routing: buildRouting,
    health: buildHealth,
    config: buildConfig,
    doctor: buildDoctor
  };
