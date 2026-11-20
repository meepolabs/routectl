// ---- scenarios: the Now needs-attention section ---------------------------
//
// Test-only snippet injected after page_browser_driver.js. The attention
// panel arrives in the /status aggregate; each scenario rewrites that panel
// per answer (n = answers for /status before this one) and reads the section
// the page drew from it.
(function (d) {
  'use strict';

  var SECTION = '#body-now [data-source="attention"]';

  // The raw-token prefix of the fixture's logged-out seat, the item whose fix
  // command the selection scenario selects.
  var FIX_ITEM_TOKEN = 'credential_unusable';

  function section() {
    var node = document.querySelector(SECTION);
    if (!node) { throw new Error('no attention section on Now'); }
    return node;
  }

  function rows() {
    return Array.prototype.slice.call(section().querySelectorAll('.attn-item'));
  }

  function rowWithToken(prefix) {
    var found = rows().filter(function (row) {
      return row.querySelector('.attn-raw-code').textContent.indexOf(prefix) === 0;
    });
    if (found.length !== 1) { throw new Error(found.length + ' rows carry ' + prefix); }
    return found[0];
  }

  function textIn(row, selector) {
    var node = row.querySelector(selector);
    return node ? node.textContent : null;
  }

  function rewriteAttention(edit) {
    d.rewrite(function (url, text, n) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      edit(agg.panels.attention, n);
      return JSON.stringify(agg);
    });
  }

  function itemWithToken(panel, prefix) {
    return panel.data.items.filter(function (item) {
      return item.raw_token.indexOf(prefix) === 0;
    })[0];
  }

  function waitForRows() {
    return d.waitUntil('the attention items', function () {
      var node = document.querySelector(SECTION);
      return node && node.querySelectorAll('.attn-item').length > 0;
    });
  }

  // The selected item's fix and a sibling's cause both change on every
  // answer: the sibling shows the section repainted, the selected fix shows
  // what the server sent and the held item does not.
  d.scenario('now-fix-selection', function () {
    var served = { fix: null };
    rewriteAttention(function (panel, n) {
      var fixed = itemWithToken(panel, FIX_ITEM_TOKEN);
      fixed.fix = fixed.fix + ' # round ' + n;
      served.fix = fixed.fix;
      panel.data.items[0].cause = panel.data.items[0].cause + ' (round ' + n + ')';
    });
    return waitForRows().then(function () {
      var code = rowWithToken(FIX_ITEM_TOKEN).querySelector('.attn-fix-code');
      var node = code.firstChild;
      var range = document.createRange();
      range.setStart(node, 0);
      range.setEnd(node, node.nodeValue.length);
      var sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(range);
      var picked = sel.toString();
      return d.acrossRounds('/status', 2, function () {
        var now = window.getSelection();
        return {
          picked: picked,
          same_text: picked !== '' && now.toString() === picked,
          same_anchor: now.anchorNode === node,
          connected: node.isConnected,
          shown_fix: textIn(rowWithToken(FIX_ITEM_TOKEN), '.attn-fix-code'),
          served_fix: served.fix,
          figure: textIn(rows()[0], '.attn-cause')
        };
      }).then(function (out) {
        window.getSelection().removeAllRanges();
        return d.waitFor('/status', 1).then(function () {
          var live = rowWithToken(FIX_ITEM_TOKEN).querySelector('.attn-fix-code');
          out.released = { shown_fix: live.textContent, served_fix: served.fix };
          live.focus();
          out.focus = {
            tag: live.tagName.toLowerCase(),
            tab_index: live.tabIndex,
            focused: document.activeElement === live
          };
          return out;
        });
      });
    });
  });

  // A color as the page computes it for `value` (a token reference).
  function resolvedColor(value) {
    var probe = document.createElement('span');
    probe.style.color = value;
    document.body.appendChild(probe);
    var color = getComputedStyle(probe).color;
    document.body.removeChild(probe);
    return color;
  }

  function rowObservation(row) {
    var link = row.querySelector('.attn-target');
    return {
      class_name: row.className,
      severity: textIn(row, '.attn-sev'),
      subject: textIn(row, '.attn-subject'),
      cause: textIn(row, '.attn-cause'),
      recovered: textIn(row, '.attn-recovered'),
      raw: textIn(row, '.attn-raw-code'),
      fix: textIn(row, '.attn-fix-code'),
      href: link ? link.getAttribute('href') : null,
      link_text: link ? link.textContent : null,
      cause_color: getComputedStyle(row.querySelector('.attn-cause')).color
    };
  }

  // Every row as drawn, the section's count strip, the section text, and the
  // grey a recovered row is drawn in. The envelope's as_of moves 7 minutes
  // past the fixture's recovery instant.
  d.scenario('now-attention-list', function () {
    rewriteAttention(function (panel) {
      panel.as_of = new Date(Date.parse(panel.as_of) + 7 * 60000).toISOString();
    });
    return waitForRows().then(function () {
      return d.waitFor('/status', 1);
    }).then(function () {
      var counts = Array.prototype.map.call(
        section().querySelectorAll('.attn-count'),
        function (n) { return n.textContent; });
      return {
        rows: rows().map(rowObservation),
        counts: counts,
        body_text: document.getElementById('body-now').textContent,
        faint: resolvedColor('var(--faint)'),
        fg: resolvedColor('var(--fg)')
      };
    }).then(function (out) {
      var link = rows()[0].querySelector('.attn-target');
      out.clicked_href = link.getAttribute('href');
      link.click();
      return d.waitUntil('the linked view', function () {
        return location.hash === out.clicked_href;
      }).then(function () {
        out.after_click = {
          hash: location.hash,
          pane_shown: !document.getElementById('pane-' + out.clicked_href.slice(1)).hidden
        };
        return out;
      });
    });
  });

  // Daemon strings that look like markup are drawn as text.
  d.scenario('now-attention-markup', function () {
    var markup = '<b>bold</b> & <img src=x>';
    rewriteAttention(function (panel) {
      panel.data.items[0].cause = markup;
      panel.data.items[0].subject = markup;
      panel.data.items[0].raw_token = markup;
    });
    return waitForRows().then(function () {
      var row = rows()[0];
      return {
        markup: markup,
        cause: textIn(row, '.attn-cause'),
        subject: textIn(row, '.attn-subject'),
        raw: textIn(row, '.attn-raw-code'),
        b_elements: document.querySelectorAll('#body-now b').length,
        img_elements: document.querySelectorAll('#body-now img').length
      };
    });
  });

  // The attention panel answers unavailable inside an otherwise good
  // aggregate: its section shows the error card and the traffic tiles still
  // render beside it.
  d.scenario('now-attention-unavailable', function () {
    rewriteAttention(function (panel) {
      panel.unavailable = 'attention_failed';
      panel.data = null;
    });
    return d.waitUntil('the Now tiles and the attention card', function () {
      var body = document.getElementById('body-now');
      return body.querySelectorAll('.kpi').length > 0 &&
        !!body.querySelector('[data-source="attention"] .unavailable-msg');
    }).then(function () {
      var body = document.getElementById('body-now');
      var cards = body.querySelectorAll('.unavailable-msg, .incompatible-msg');
      return {
        error_cards: cards.length,
        cards_in_attention: section().querySelectorAll('.unavailable-msg').length,
        card_text: cards.length ? cards[0].textContent : null,
        attention_title: textIn(section(), '.card-title'),
        attention_rows: section().querySelectorAll('.attn-item').length,
        kpis: body.querySelectorAll('.kpi').length,
        providers: body.querySelectorAll('.provcard').length
      };
    });
  });
})(window.routectlDriver);
