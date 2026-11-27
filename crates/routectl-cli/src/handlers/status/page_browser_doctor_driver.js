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
  // Setting `clear` serves that panel with nothing in it from then on.
  function freezeAttention() {
    var frozen = { panel: null, clear: false };
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      if (!frozen.panel) { frozen.panel = agg.panels.attention; }
      agg.panels.attention = frozen.clear ? clearedAttention(frozen.panel) : frozen.panel;
      return JSON.stringify(agg);
    });
    return frozen;
  }

  function clearedAttention(panel) {
    var cleared = JSON.parse(JSON.stringify(panel));
    cleared.data.items = [];
    Object.keys(cleared.data.counts).forEach(function (key) { cleared.data.counts[key] = 0; });
    return cleared;
  }

  // Every /status answer's doctor report carries only its passing checks.
  function passesOnly() {
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      var report = agg.panels.doctor.data.report;
      report.findings = report.findings.filter(function (f) { return f.status === 'Pass'; });
      return JSON.stringify(agg);
    });
  }

  function attentionSectionNode() {
    return document.querySelector('#body-doctor [data-source="attention"]');
  }

  // The Doctor section head over the attention list: its count, its dot, its
  // accessible name, and the tab badge beside it (as shown, and its text and
  // hidden state read apart, so a hidden badge's text is seen too).
  function doctorHead() {
    var section = attentionSectionNode();
    var head = section && section.querySelector('.card-head');
    var dot = head && head.querySelector('.docdot');
    var badge = document.getElementById('badge-doctor');
    return {
      count: head ? textOrNull('#body-doctor [data-source="attention"] .card-head .docattn-count') : null,
      name: head ? d.accessibleName(head.querySelector('.card-title')) : null,
      dot: dot ? dot.className : null,
      badge: badge.hidden ? null : badge.textContent,
      badge_text: badge.textContent,
      badge_hidden: badge.hidden,
      clear_line: section ? section.querySelectorAll('.attn-clear').length : null,
      head_lines: section ? section.querySelectorAll('.card-head').length : null
    };
  }

  // Now draws its list, then the Doctor tab is opened and draws its own;
  // both stay in the document, the Now pane hidden. The attention panel is
  // then served empty and the Doctor head read again.
  d.scenario('doctor-same-list', function () {
    var frozen = freezeAttention();
    var out = {};
    return waitForItems('body-now').then(function () {
      document.getElementById('tab-doctor').click();
      return waitForItems('body-doctor');
    }).then(function () {
      out.now = keysIn('body-now');
      out.doctor = keysIn('body-doctor');
      out.duplicate_ids = duplicateIds();
      out.active = doctorHead();
      out.served_counts = frozen.panel.data.counts;
      out.served_keys = frozen.panel.data.items.map(function (item) { return item.key; });
      out.doctor_findings = document.querySelectorAll('#body-doctor .docfinding').length;
      out.doctor_text = document.getElementById('body-doctor').textContent;
      out.old_headline_unit = !!document.querySelector('#body-doctor [data-unit="headline"]');
      frozen.clear = true;
      return d.waitUntil('the doctor attention list emptied', function () {
        return itemsIn('body-doctor').length === 0 &&
          textOrNull('#body-doctor [data-source="attention"] .docattn-count') === '0';
      });
    }).then(function () {
      out.healthy = doctorHead();
      out.healthy_text = attentionSectionNode().textContent;
      return out;
    });
  });

  // The report's checks all pass while the attention list has items, then
  // the attention panel is served empty: the checks' all-clear is read in
  // both states.
  d.scenario('doctor-checks-card', function () {
    var frozen = freezeAttention();
    passesOnly();
    function checksState() {
      var body = document.getElementById('body-doctor');
      var doctor = body.querySelector('[data-source="doctor"]');
      var line = doctor.querySelector('.docchecks');
      return {
        badge: doctorHead().badge,
        line: line ? line.textContent : null,
        line_font: line ? getComputedStyle(line).fontFamily : null,
        ui_font: getComputedStyle(document.body).fontFamily,
        cards: Array.prototype.map.call(doctor.querySelectorAll('.card.empty .empty-title'),
          function (n) { return n.textContent; }),
        findings: doctor.querySelectorAll('.docfinding').length,
        passes: textOrNull('#doctor-passes summary')
      };
    }
    var out = {};
    return d.waitUntil('doctor checks drawn beside attention items', function () {
      return itemsIn('body-doctor').length > 0 && document.querySelector('#doctor-passes');
    }).then(function () {
      out.active = checksState();
      frozen.clear = true;
      return d.waitUntil('the full all-clear card', function () {
        return document.querySelector('#body-doctor [data-source="doctor"] .card.empty');
      });
    }).then(function () {
      out.healthy = checksState();
      return out;
    });
  });

  // The doctor panel's reachability replaced by the row's list, then by the
  // row's `next` list, which carries a verdict this build does not know.
  d.scenario('doctor-reach', function () {
    var row = window.routectlDoctorRow;
    var state = { list: row.reachability };
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      agg.panels.doctor.data.reachability = state.list;
      return JSON.stringify(agg);
    });
    var out = {};
    return d.waitUntil('the reachability line', function () {
      return document.querySelector('#body-doctor .docreach');
    }).then(function (line) {
      out.line = line.textContent;
      out.line_font = getComputedStyle(line).fontFamily;
      out.ui_font = getComputedStyle(document.body).fontFamily;
      state.list = row.next;
      return d.waitUntil('the doctor section after an unknown verdict', function () {
        return !document.querySelector('#body-doctor .docreach');
      });
    }).then(function () {
      var section = document.querySelector('#body-doctor [data-source="doctor"]');
      out.after = section.textContent;
      out.after_error = !!section.querySelector('.unavailable-msg');
      return out;
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
