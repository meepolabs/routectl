// ---- scenarios: the Traffic table ----------------------------------------
//
// Test-only snippet injected after page_browser_driver.js. A scenario that
// needs a QUERY answer the populated fixtures do not hold reads it from
// `window.routectlTrafficAnswers`, keyed by `group_by`, as the server built
// it; every other answer is the harness's own.
(function (d) {
  'use strict';

  var TABLE = 'traffic-groups';
  var SECTION = '#body-traffic [data-source="query"]';

  var served = window.routectlTrafficAnswers || {};
  var inner = window.fetch;
  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    return inner(input, init).then(function (resp) {
      if (url.pathname !== '/status/query' || !init || typeof init.body !== 'string') {
        return resp;
      }
      var body = JSON.parse(init.body);
      var answer = body.bucket ? null : served[body.group_by];
      if (!answer) { return resp; }
      return new Response(JSON.stringify(answer), {
        status: 200,
        headers: { 'Content-Type': 'application/json' }
      });
    });
  };

  function byId(id) {
    var node = document.getElementById(id);
    if (!node) { throw new Error('no element #' + id); }
    return node;
  }

  function rows() {
    return Array.prototype.slice.call(document.querySelectorAll('#' + TABLE + ' tr.exp-row'));
  }

  function rowLabel(tr) {
    return tr.querySelector('th').textContent;
  }

  function cellTexts(tr) {
    return Array.prototype.map.call(tr.querySelectorAll('th, td.num'), function (c) {
      return c.textContent;
    });
  }

  function tableShown() {
    return d.waitUntil('the traffic table', function () {
      return rows().length > 0 && !document.querySelector('#body-traffic .skeleton');
    });
  }

  // The dt -> dd text of one row's opened detail, in order, with the step
  // and remainder counts it shows.
  function detailOf(rowId) {
    var detail = byId(rowId + '-detail');
    return Array.prototype.map.call(detail.querySelectorAll('.def'), function (def) {
      var reqs = def.querySelectorAll('.traffic-req');
      return {
        term: def.querySelector('dt').textContent,
        text: def.querySelector('dd').textContent,
        stepname: (def.querySelector('.traffic-stepname') || {}).textContent || null,
        req: reqs.length ? parseInt(reqs[0].textContent, 10) : null,
        keys: Array.prototype.map.call(def.querySelectorAll('.traffic-key'), function (k) {
          return k.textContent;
        })
      };
    });
  }

  function openRow(rowId) {
    byId(rowId + '-toggle').click();
    return d.waitUntil('the opened detail', function () {
      return !byId(rowId + '-detail').hidden;
    });
  }

  // ---- a dense table fits one screen ----------------------------------------

  d.scenario('traffic-dense', function () {
    return tableShown().then(function () {
      var all = rows();
      return {
        inner_width: window.innerWidth,
        inner_height: window.innerHeight,
        headers: Array.prototype.map.call(
          document.querySelectorAll('#' + TABLE + ' thead th'), function (th) {
            return th.textContent;
          }),
        rows: all.length,
        labels: all.map(rowLabel),
        below_fold: all.filter(function (tr) {
          return tr.getBoundingClientRect().bottom > window.innerHeight;
        }).map(rowLabel),
        page_scrolls: document.documentElement.scrollHeight > window.innerHeight
      };
    });
  });

  // ---- a sorted, opened table survives a refresh ----------------------------

  function keptObservation(row, detail, sortBtn) {
    var live = rows();
    return {
      same_row: row === document.getElementById(row.id),
      same_detail: detail === document.getElementById(detail.id),
      detail_shown: !byId(detail.id).hidden,
      expanded: byId(row.id + '-toggle').getAttribute('aria-expanded'),
      same_sort_button: sortBtn === document.getElementById(sortBtn.id),
      aria_sort: sortBtn.parentNode.getAttribute('aria-sort'),
      order: live.map(rowLabel),
      requests: live.map(function (tr) { return parseInt(tr.querySelector('td.num').textContent, 10); }),
      figure: cellTexts(row).join(' | '),
      hash: location.hash
    };
  }

  d.scenario('traffic-kept', function () {
    return tableShown().then(function () {
      byId(TABLE + '-sort-krequests').click();
      return d.waitUntil('the ascending sort', function () {
        var btn = document.getElementById(TABLE + '-sort-krequests');
        return btn && btn.parentNode.getAttribute('aria-sort') === 'ascending';
      });
    }).then(function () {
      return openRow(TABLE + '-ksonnet');
    }).then(function () {
      var row = byId(TABLE + '-ksonnet');
      var detail = byId(TABLE + '-ksonnet-detail');
      var sortBtn = byId(TABLE + '-sort-krequests');
      return d.acrossRounds('/status/query', 2, function () {
        return keptObservation(row, detail, sortBtn);
      });
    });
  });

  // ---- a range change is pending before it relabels -------------------------

  function sectionState() {
    var section = document.querySelector(SECTION);
    var hint = section && section.querySelector('.card-hint');
    return {
      busy: byId('pane-traffic').getAttribute('aria-busy'),
      section_pending: !!section && section.classList.contains('section--pending'),
      table: !!document.getElementById(TABLE),
      hint: hint ? hint.textContent : null,
      height: section ? section.offsetHeight : 0
    };
  }

  d.scenario('traffic-range-pending', function () {
    var out = { samples: [] };
    var settled = false;
    d.hold(function (url, init) {
      var body = init && typeof init.body === 'string' ? JSON.parse(init.body) : null;
      return body && body.window === 'week' ? 3000 : 0;
    });
    return tableShown().then(function () {
      out.before = sectionState();
      // Every repaint from the click up to and including the first one that
      // is no longer busy.
      var observer = new MutationObserver(function () {
        if (settled) { return; }
        var s = sectionState();
        out.samples.push(s);
        if (s.busy === null) { settled = true; }
      });
      observer.observe(byId('pane-traffic'), {
        subtree: true, childList: true, characterData: true, attributes: true
      });
      document.querySelector('#windowsel button[data-window="week"]').click();
      out.at_click = sectionState();
      return d.waitUntil('the week table', function () {
        var s = sectionState();
        return s.table && s.busy === null;
      }).then(function () { observer.disconnect(); });
    }).then(function () {
      out.after = sectionState();
      return out;
    });
  });

  // ---- an opened row's detail -------------------------------------------------

  d.scenario('traffic-detail', function () {
    var label = window.routectlTrafficRow;
    var rowId = TABLE + '-' + window.routectlTrafficRowKey;
    return tableShown().then(function () {
      return openRow(rowId);
    }).then(function () {
      var row = byId(rowId);
      return {
        label: rowLabel(row),
        wanted: label,
        requests: parseInt(row.querySelector('td.num').textContent, 10),
        detail: detailOf(rowId)
      };
    });
  });

  // ---- first-load layout shift ------------------------------------------------

  // A layout-shift score over the Traffic pane from the first paint until the
  // table is drawn and two more aggregate rounds have landed, by the CLS
  // formula: at each repaint, every element that was already drawn and moved
  // adds the share of the viewport its old and new boxes cover, times the
  // distance it moved over the viewport's larger side. Computed here rather
  // than read from layout-shift entries, which headless Chrome under virtual
  // time does not report for frames after the load. With
  // `window.routectlTrafficPlant` set, a 700 px block sits under the section
  // from the first paint, the way the old routing sections did.
  function shiftScorer(root) {
    var seen = new Map();
    var score = 0;
    function measure() {
      var vw = window.innerWidth, vh = window.innerHeight;
      var area = 0, distance = 0;
      var now = new Map();
      Array.prototype.forEach.call(root.querySelectorAll('*'), function (node) {
        var r = node.getBoundingClientRect();
        if (r.width === 0 && r.height === 0) { return; }
        now.set(node, r);
        var was = seen.get(node);
        if (!was) { return; }
        var moved = Math.max(Math.abs(r.top - was.top), Math.abs(r.left - was.left));
        if (moved < 1 || was.top >= vh) { return; }
        distance = Math.max(distance, moved / Math.max(vw, vh));
        var top = Math.max(0, Math.min(was.top, r.top));
        var bottom = Math.min(vh, Math.max(was.bottom, r.bottom));
        var left = Math.max(0, Math.min(was.left, r.left));
        var right = Math.min(vw, Math.max(was.right, r.right));
        if (bottom > top && right > left) { area += (bottom - top) * (right - left); }
      });
      score += Math.min(1, area / (vw * vh)) * distance;
      seen = now;
    }
    var observer = new MutationObserver(measure);
    measure();
    observer.observe(root, { subtree: true, childList: true, characterData: true, attributes: true });
    return {
      stop: function () { observer.disconnect(); measure(); return score; }
    };
  }

  d.scenario('traffic-layout-shift', function () {
    var pane = byId('pane-traffic');
    if (window.routectlTrafficPlant) {
      var plant = document.createElement('div');
      plant.className = 'traffic-plant';
      plant.style.height = '700px';
      plant.textContent = 'planted';
      pane.appendChild(plant);
    }
    var scorer = shiftScorer(pane);
    return tableShown().then(function () {
      return d.waitFor('/status', 2);
    }).then(function () {
      return { first_load: scorer.stop() };
    });
  });
})(window.routectlDriver);
