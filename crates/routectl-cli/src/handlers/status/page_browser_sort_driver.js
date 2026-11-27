// ---- scenarios: the shared sortable table ---------------------------------
//
// Test-only snippet injected after page_browser_driver.js. A scenario that
// needs a reading the fixtures do not carry sets `configPatch`, which rewrites
// the config panel of every /status answer before the page sees it; the
// models it edits are the ones the real panel serialized.
(function (d) {
  'use strict';

  var TABLE = 'config-models';
  var configPatch = null;

  var inner = window.fetch;
  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    return inner(input, init).then(function (resp) {
      if (url.pathname !== '/status' || !resp.ok || !configPatch) { return resp; }
      var patch = configPatch;
      return resp.json().then(function (json) {
        patch(json.panels.config.data.models, json.panels.config.data);
        return new Response(JSON.stringify(json), {
          status: resp.status,
          statusText: resp.statusText,
          headers: resp.headers
        });
      });
    });
  };

  function byId(id) {
    var node = document.getElementById(id);
    if (!node) { throw new Error('no element #' + id); }
    return node;
  }

  function headerButton(column) {
    return byId(TABLE + '-sort-k' + column);
  }

  function modelRows() {
    return Array.prototype.slice.call(
      document.querySelectorAll('#' + TABLE + ' tbody tr.exp-row'));
  }

  // The nicknames in the order the table shows them.
  function order() {
    return modelRows().map(function (tr) {
      return tr.querySelector('th[scope="row"]').textContent;
    });
  }

  // Every row's detail row sits right after it.
  function detailsFollowRows() {
    return modelRows().every(function (tr) {
      var next = tr.nextElementSibling;
      return !!next && next.id === tr.id + '-detail';
    });
  }

  // Which header says it is sorted, and how: { <column label>: aria-sort }.
  function ariaSort() {
    var out = {};
    var heads = document.querySelectorAll('#' + TABLE + ' thead th[aria-sort]');
    Array.prototype.forEach.call(heads, function (th) {
      out[th.textContent] = th.getAttribute('aria-sort');
    });
    return out;
  }

  function tableReport(base) {
    return {
      order: order(),
      aria_sort: ariaSort(),
      hash: location.hash,
      added: history.length - base,
      details_follow_rows: detailsFollowRows()
    };
  }

  function waitForTable() {
    return d.waitUntil('the models table', function () {
      return document.getElementById(TABLE) && modelRows().length > 0;
    });
  }

  // Header clicks sort ascending then descending, and the address follows;
  // the descending order survives a reload; a third click restores builder
  // order. The driver runs on each load; sessionStorage says which side of
  // the reload this one is.
  var RELOAD_KEY = 'routectl-sort-reload';
  d.scenario('sort-click-reload', function () {
    var before = sessionStorage.getItem(RELOAD_KEY);
    if (before === null) {
      return waitForTable().then(function () {
        var base = history.length;
        var out = { unsorted: tableReport(base) };
        headerButton('provider').click();
        out.asc = tableReport(base);
        headerButton('provider').click();
        out.desc = tableReport(base);
        sessionStorage.setItem(RELOAD_KEY, JSON.stringify(out));
        location.reload();
        return new Promise(function () {});
      });
    }
    return waitForTable().then(function () {
      var out = JSON.parse(before);
      var base = history.length;
      out.reloaded = tableReport(base);
      headerButton('provider').click();
      out.cleared = tableReport(base);
      return out;
    });
  });

  // A sort keeps the focused header and an open row's nodes, and so do the
  // refreshes after it.
  d.scenario('sort-keeps-nodes', function () {
    var header;
    var toggle;
    var detail;
    var unsorted;
    function observe() {
      return {
        order: order(),
        same_header: header === document.getElementById(header.id),
        header_focused: document.activeElement === header,
        same_detail: detail === document.getElementById(detail.id),
        expanded: byId(toggle.id).getAttribute('aria-expanded') === 'true',
        detail_shown: !byId(detail.id).hidden,
        details_follow_rows: detailsFollowRows(),
        aria_sort: ariaSort(),
        figure: document.querySelector('#body-config .cfgstrip').textContent
      };
    }
    return waitForTable().then(function () {
      toggle = byId(TABLE + '-kglm-toggle');
      toggle.click();
      detail = byId(TABLE + '-kglm-detail');
      unsorted = order();
      header = headerButton('upstream');
      header.focus();
      header.click();
      var sorted = observe();
      return d.acrossRounds('/status', 2, observe).then(function (rounds) {
        return { unsorted: unsorted, sorted: sorted, rounds: rounds };
      });
    });
  });

  // A reading with an absent upstream and two models sharing one, served in
  // reverse nickname order so a sort that kept builder order on a tie would
  // show.
  d.scenario('sort-null-and-tie', function () {
    configPatch = function (models) {
      models.reverse();
      models.forEach(function (m) {
        if (m.nickname === 'bedclaude') { m.upstream = null; }
      });
    };
    return waitForTable().then(function () {
      var base = history.length;
      var out = { unsorted: tableReport(base) };
      out.null_cell = byId(TABLE + '-kbedclaude').querySelectorAll('td')[2].textContent;
      headerButton('upstream').click();
      out.asc = tableReport(base);
      headerButton('upstream').click();
      out.desc = tableReport(base);
      return out;
    });
  });

  // The computed color `color: var(--faint)` resolves to in the page's theme.
  function faintColor() {
    var probe = document.createElement('span');
    probe.style.color = 'var(--faint)';
    document.body.appendChild(probe);
    var color = getComputedStyle(probe).color;
    probe.remove();
    return color;
  }

  // The mark a header button draws after its label, and in which color.
  function markReport(column) {
    var btn = headerButton(column);
    var after = getComputedStyle(btn, '::after');
    return { content: after.content, color: after.color, label_color: getComputedStyle(btn).color };
  }

  // The mark an unpressed header draws while it holds keyboard focus.
  function focusedMarkReport(column) {
    var btn = headerButton(column);
    btn.focus();
    var report = markReport(column);
    report.focus_visible = btn.matches(':focus-visible');
    btn.blur();
    return report;
  }

  // At rest an unpressed sortable header and a plain header draw no mark;
  // keyboard focus on an unpressed one shows the faint neutral hint; a
  // pressed header shows its direction.
  d.scenario('sort-mark', function () {
    return waitForTable().then(function () {
      var plain = document.querySelector('#config-aliases thead th');
      var out = {
        faint: faintColor(),
        unpressed: markReport('provider'),
        unpressed_focused: focusedMarkReport('provider'),
        plain_header: {
          has_button: !!plain.querySelector('button'),
          after: getComputedStyle(plain, '::after').content
        }
      };
      headerButton('provider').click();
      out.ascending = markReport('provider');
      out.other_unpressed = markReport('upstream');
      headerButton('provider').click();
      out.descending = markReport('provider');
      out.pressed_focused = focusedMarkReport('provider');
      return out;
    });
  });

  // A header button's box, the box its focus ring is drawn around (the
  // button grown by the outline's offset and width), where its label's text
  // starts and ends, and the box of the mark beside the label, from the
  // mark's used position and size.
  function markGeometry(btn) {
    var b = btn.getBoundingClientRect();
    var cs = getComputedStyle(btn);
    var ring = parseFloat(cs.outlineOffset) + parseFloat(cs.outlineWidth);
    var after = getComputedStyle(btn, '::after');
    var left = b.left + parseFloat(after.left);
    var label = document.createRange();
    label.selectNodeContents(btn);
    var text = label.getBoundingClientRect();
    return {
      label_left: text.left,
      label_right: text.right,
      content: after.content,
      button: { left: b.left, right: b.right, width: b.width },
      ring: { left: b.left - ring, right: b.right + ring, style: cs.outlineStyle },
      mark: { left: left, right: left + parseFloat(after.width), width: parseFloat(after.width) },
      header_width: btn.closest('th').getBoundingClientRect().width,
      focus_visible: btn.matches(':focus-visible')
    };
  }

  // An unpressed header at rest and with keyboard focus, then the same
  // header pressed (its own mark) with focus.
  d.scenario('sort-hint-box', function () {
    return waitForTable().then(function () {
      var out = { rest: markGeometry(headerButton('provider')) };
      headerButton('provider').focus();
      out.focused = markGeometry(headerButton('provider'));
      headerButton('provider').blur();
      headerButton('provider').click();
      headerButton('provider').focus();
      out.pressed_focused = markGeometry(headerButton('provider'));
      return out;
    });
  });

  // Where each figure of the column under `th` ends: the right edge of its
  // drawn text in every body row.
  function figureRights(th) {
    var col = Array.prototype.indexOf.call(th.parentNode.children, th);
    var table = th.closest('table');
    return Array.prototype.map.call(table.querySelectorAll('tbody tr.exp-row'), function (tr) {
      var range = document.createRange();
      range.selectNodeContents(tr.children[col]);
      return range.getBoundingClientRect().right;
    });
  }

  // The Traffic table's right-aligned REQUESTS header beside its figures:
  // at rest, keyboard-focused, then sorted (by a click) at rest and focused.
  d.scenario('sort-hint-box-numeric', function () {
    var id = 'traffic-groups-sort-krequests';
    function btn() { return byId(id); }
    function geometry() {
      var out = markGeometry(btn());
      var th = btn().closest('th');
      out.header_num = th.classList.contains('num');
      out.figure_rights = figureRights(th);
      return out;
    }
    return d.waitUntil('the traffic table', function () {
      return document.getElementById(id) &&
        document.querySelectorAll('#traffic-groups tbody tr.exp-row').length > 0;
    }).then(function () {
      var out = {
        inner_width: window.innerWidth,
        root_font_px: parseFloat(getComputedStyle(document.documentElement).fontSize),
        rest: geometry()
      };
      btn().focus();
      out.focused = geometry();
      btn().blur();
      btn().click();
      return d.waitUntil('the sorted header', function () {
        return btn().closest('th').getAttribute('aria-sort') === 'ascending';
      }).then(function () {
        out.pressed = geometry();
        btn().focus();
        out.pressed_focused = geometry();
        return out;
      });
    });
  });

  function sectionReport() {
    var body = byId('body-config');
    return {
      titles: Array.prototype.map.call(body.querySelectorAll('.card-title'),
        function (n) { return n.textContent; }),
      tables: Array.prototype.map.call(body.querySelectorAll('table'),
        function (t) { return t.id; }),
      error_cards: body.querySelectorAll('.unavailable-msg').length
    };
  }

  // Two models sharing a nickname error the models section alone. The first
  // observation is the same page before the duplicate arrives.
  d.scenario('sort-duplicate-key', function () {
    return waitForTable().then(function () {
      var out = { clean: sectionReport() };
      configPatch = function (models) {
        models.push(JSON.parse(JSON.stringify(models[0])));
      };
      return d.waitFor('/status', 1).then(function () {
        out.duplicate = sectionReport();
        return out;
      });
    });
  });
})(window.routectlDriver);
