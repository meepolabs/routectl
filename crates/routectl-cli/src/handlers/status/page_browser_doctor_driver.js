// ---- scenarios: the Doctor view -------------------------------------------
//
// Test-only snippet injected after page_browser_driver.js. A row a test
// supplies arrives as `window.routectlDoctorRow`, set by a snippet ahead of
// this one. Every wait is on a page state or an answer count.
(function (d) {
  'use strict';

  var ITEM_ID = /^(now|doctor)-attention-(.*)$/;

  function itemsIn(bodyId) {
    return Array.prototype.slice.call(
      document.querySelectorAll('#' + bodyId + ' .attn-item'));
  }

  function waitForItems(bodyId) {
    return d.waitUntil('attention items in #' + bodyId, function () {
      return itemsIn(bodyId).length > 0;
    });
  }

  // The item ids drawn in one view, with the view's prefix taken off.
  function keysIn(bodyId) {
    return itemsIn(bodyId).map(function (row) {
      var m = ITEM_ID.exec(row.id);
      return m ? { prefix: m[1], key: m[2] } : { prefix: null, key: row.id };
    });
  }

  function duplicateIds() {
    var seen = Object.create(null);
    var dups = [];
    Array.prototype.forEach.call(document.querySelectorAll('[id]'), function (node) {
      if (seen[node.id]) { dups.push(node.id); }
      seen[node.id] = true;
    });
    return dups;
  }

  function textOrNull(selector) {
    var node = document.querySelector(selector);
    return node ? node.textContent : null;
  }

  // Every /status answer carries the attention panel of the first one, so
  // both views draw their lists from one payload whichever round each read.
  function freezeAttention() {
    var frozen = { panel: null };
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      if (!frozen.panel) { frozen.panel = agg.panels.attention; }
      agg.panels.attention = frozen.panel;
      return JSON.stringify(agg);
    });
    return frozen;
  }

  // Now draws its list, then the Doctor tab is opened and draws its own;
  // both stay in the document, the Now pane hidden.
  d.scenario('doctor-same-list', function () {
    var frozen = freezeAttention();
    return waitForItems('body-now').then(function () {
      document.getElementById('tab-doctor').click();
      return waitForItems('body-doctor');
    }).then(function () {
      var badge = document.getElementById('badge-doctor');
      return {
        now: keysIn('body-now'),
        doctor: keysIn('body-doctor'),
        duplicate_ids: duplicateIds(),
        headline: textOrNull('#body-doctor .dochead-count'),
        badge: badge.hidden ? null : badge.textContent,
        served_counts: frozen.panel.data.counts,
        served_keys: frozen.panel.data.items.map(function (item) { return item.key; }),
        doctor_findings: document.querySelectorAll('#body-doctor .docfinding').length,
        old_headline_gone: !document.querySelector('#body-doctor .doccounts .doccount-label') ||
          Array.prototype.every.call(
            document.querySelectorAll('#body-doctor .doccount-label'),
            function (n) { return ['failed', 'warnings', 'passed'].indexOf(n.textContent) < 0; })
      };
    });
  });

  // The doctor report's findings replaced by the row's list on every answer;
  // each finding card is read back beside the remediation the page was
  // served for it.
  d.scenario('doctor-remediations', function () {
    var row = window.routectlDoctorRow;
    var served = { findings: null };
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      agg.panels.doctor.data.report.findings = row.findings;
      var out = JSON.stringify(agg);
      served.findings = JSON.parse(out).panels.doctor.data.report.findings;
      return out;
    });
    return d.waitUntil(row.findings.length + ' doctor findings', function () {
      return document.querySelectorAll('#body-doctor .docfinding').length === row.findings.length &&
        itemsIn('body-doctor').length > 0;
    }).then(function () {
      var cards = Array.prototype.map.call(
        document.querySelectorAll('#body-doctor .docfinding'), function (card) {
          var text = card.querySelector('.docfix-text');
          return {
            detail: card.querySelector('.docfinding-detail').textContent,
            text: text ? text.textContent : null,
            codes: Array.prototype.map.call(card.querySelectorAll('.docfix code'), function (c) {
              return { text: c.textContent, tab_index: c.tabIndex };
            })
          };
        });
      var fixes = Array.prototype.map.call(
        document.querySelectorAll('#body-doctor .attn-fix-code'), function (c) {
          return { tag: c.tagName.toLowerCase(), text: c.textContent, tab_index: c.tabIndex };
        });
      return { cards: cards, served: served.findings, fixes: fixes };
    });
  });
  // The Models view is drawn first, then the Doctor tab; both stay in the
  // document. Every models payload served is kept, so the test can check
  // the premise that its lanes outnumber its causes.
  d.scenario('doctor-model-problems', function () {
    var served = { rows: null };
    d.rewrite(function (url, text) {
      if (url.pathname === '/status') { served.rows = JSON.parse(text).panels.models.data.rows; }
      return text;
    });
    return d.waitUntil('models lanes drawn', function () {
      var body = document.getElementById('body-models');
      return !!document.getElementById('models-lanes') && !body.querySelector('.skeleton');
    }).then(function () {
      document.getElementById('tab-doctor').click();
      return d.waitUntil('the doctor model-problem count', function () {
        return document.querySelector('#body-doctor .docmodels-count');
      });
    }).then(function () {
      var link = document.querySelector('#body-doctor .docmodels a');
      var badge = document.getElementById('badge-models');
      return {
        cause_lines: document.querySelectorAll('#body-models .models-cause-row').length,
        models_badge: badge.hidden ? null : badge.textContent,
        doctor_count: document.querySelector('#body-doctor .docmodels-count').textContent,
        doctor_lanes: textOrNull('#body-doctor .docmodels-lanes'),
        doctor_line: document.querySelector('#body-doctor .docmodels').textContent,
        link_action: link ? link.getAttribute('data-arg') : null,
        served_rows: served.rows
      };
    });
  });
})(window.routectlDriver);
