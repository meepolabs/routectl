// ---- scenarios: a refresh holds a selected section and keeps scroll -------
//
// Test-only snippet injected after page_browser_driver.js. The Doctor
// scenarios replace the fixture's findings with a scripted list per answer
// (`findingsAt(n)`, n = answers for /status before this one), so a round
// can change, move or drop the finding the operator has selected.
(function (d) {
  'use strict';

  var DOCTOR_FINDINGS = '#body-doctor .ovsection[data-unit="findings"]';

  function finding(name, status, detail, remediation) {
    return { section: 'config', name: name, status: status, detail: detail,
      remediation: remediation };
  }

  var ALPHA = finding('validation', 'Warn', 'alias alpha names no target',
    'point alias alpha at a configured model');
  var BETA = finding('validation', 'Warn', 'alias beta names no target',
    'point alias beta at a configured model');

  function ticker(n) {
    return finding('freshness', 'Warn', 'last validated ' + n + ' minutes ago', null);
  }

  // Serve `findingsAt(n)` as the Doctor report's findings on every /status
  // answer, and remember the last list served.
  function scriptFindings(findingsAt) {
    var served = { last: null };
    d.rewrite(function (url, text, n) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      served.last = findingsAt(n);
      agg.panels.doctor.data.report.findings = served.last;
      return JSON.stringify(agg);
    });
    return served;
  }

  function findingsUnit() {
    var unit = document.querySelector(DOCTOR_FINDINGS);
    if (!unit) { throw new Error('no doctor findings section'); }
    return unit;
  }

  // The details of the finding cards on screen, in order.
  function shownDetails() {
    return Array.prototype.map.call(
      findingsUnit().querySelectorAll('.docfinding-detail'),
      function (n) { return n.textContent; });
  }

  function cardWithDetail(detail) {
    var cards = findingsUnit().querySelectorAll('.docfinding');
    for (var i = 0; i < cards.length; i++) {
      if (cards[i].querySelector('.docfinding-detail').textContent === detail) { return cards[i]; }
    }
    throw new Error('no finding card shows ' + detail);
  }

  // Select the whole remediation text of the card showing `detail`.
  function selectFixOf(detail) {
    var node = cardWithDetail(detail).querySelector('.docfix-text').firstChild;
    var range = document.createRange();
    range.setStart(node, 0);
    range.setEnd(node, node.nodeValue.length);
    var sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
    return { node: node, text: sel.toString() };
  }

  function selectionObservation(picked) {
    var sel = window.getSelection();
    return {
      selection: sel.toString(),
      same_text: picked.text !== '' && sel.toString() === picked.text,
      same_anchor: sel.anchorNode === picked.node,
      connected: picked.node.isConnected,
      details: shownDetails(),
      figure: document.querySelector('#body-doctor .docmatrix-replay').textContent
    };
  }

  function waitForFindings(count) {
    return d.waitUntil(count + ' doctor findings', function () {
      var unit = document.querySelector(DOCTOR_FINDINGS);
      return unit && unit.querySelectorAll('.docfinding').length === count;
    });
  }

  // Select, watch `held` rounds with the selection standing, then collapse
  // it and watch one more round.
  function heldThenReleased(served, detail, held) {
    var picked = selectFixOf(detail);
    return d.acrossRounds('/status', held, function () {
      var obs = selectionObservation(picked);
      obs.served = served.last.map(function (f) { return f.detail; });
      return obs;
    }).then(function (out) {
      window.getSelection().removeAllRanges();
      return d.waitFor('/status', 1).then(function () {
        out.released = {
          details: shownDetails(),
          served: served.last.map(function (f) { return f.detail; })
        };
        return out;
      });
    });
  }

  // The selected card stays; a sibling in the same section changes text
  // every round.
  d.scenario('lock-retained', function () {
    var served = scriptFindings(function (n) { return [ALPHA, ticker(n)]; });
    return waitForFindings(2).then(function () {
      return heldThenReleased(served, ALPHA.detail, 2);
    });
  });

  // The selected card and its same-section, same-name sibling trade places
  // from the second answer on.
  d.scenario('lock-moved', function () {
    var served = scriptFindings(function (n) { return n ? [BETA, ALPHA] : [ALPHA, BETA]; });
    return waitForFindings(2).then(function () {
      return heldThenReleased(served, ALPHA.detail, 2);
    });
  });

  // The selected card's finding is gone from every answer after the first.
  d.scenario('lock-removed', function () {
    var served = scriptFindings(function (n) { return n ? [BETA, ticker(n)] : [ALPHA, BETA]; });
    return waitForFindings(2).then(function () {
      return heldThenReleased(served, ALPHA.detail, 2);
    });
  });

  // Every finding is gone from every answer after the first, so the findings
  // section itself leaves the page while its text is selected.
  d.scenario('lock-section-gone', function () {
    scriptFindings(function (n) { return n ? [] : [ALPHA]; });
    return waitForFindings(1).then(function () {
      var picked = selectFixOf(ALPHA.detail);
      return d.waitFor('/status', 1).then(function () {
        return {
          findings_shown: !!document.querySelector(DOCTOR_FINDINGS),
          stale_text_shown: document.getElementById('body-doctor').textContent
            .indexOf(ALPHA.detail) >= 0,
          selected_connected: picked.node.isConnected
        };
      });
    });
  });

  // Two findings sharing section and name swap order every round; nothing is
  // selected, so the section morphs each time.
  d.scenario('doctor-ids-swap', function () {
    scriptFindings(function (n) { return n % 2 ? [BETA, ALPHA] : [ALPHA, BETA]; });
    return waitForFindings(2).then(function () {
      var nodes = {};
      Array.prototype.forEach.call(findingsUnit().querySelectorAll('.docfinding'), function (c) {
        nodes[c.id] = c;
      });
      return d.acrossRounds('/status', 2, function () {
        return Array.prototype.map.call(findingsUnit().querySelectorAll('.docfinding'), function (c) {
          return {
            id: c.id,
            detail: c.querySelector('.docfinding-detail').textContent,
            same_node: nodes[c.id] === c
          };
        });
      });
    });
  });

  // A narrow table scroller and a tall page, both scrolled away from their
  // origin before the rounds.
  d.scenario('lock-scroll', function () {
    var style = document.createElement('style');
    style.textContent = '#config-models-wrap { max-width: 160px; } ' +
      '#body-config { padding-bottom: 3000px; }';
    document.head.appendChild(style);
    return d.waitForElement('config-models-wrap').then(function (wrap) {
      wrap.scrollLeft = 60;
      window.scrollTo(0, 400);
      return d.acrossRounds('/status', 2, function () {
        var live = document.getElementById('config-models-wrap');
        return {
          table_left: live.scrollLeft,
          page_y: window.scrollY,
          figure: document.querySelector('#body-config .cfgstrip').textContent
        };
      });
    });
  });
})(window.routectlDriver);
