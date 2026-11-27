// ---- scenarios: every figure readable in full at the page's text size ----
//
// Test-only snippet injected after page_browser_driver.js. Once the view on
// screen has settled, `layout-check` walks every rendered element and
// reports what hides text: a box that cuts its text off (hidden overflow, or
// an ellipsis) and a scroller nobody can reach by keyboard. A truncated name
// is allowed when its full text is in its `title`. It lists every table
// scroller with the name it announces. It also reports any two
// text runs in one stat row that draw over each other, and how each
// Doctor fix command fits its box. The checks also run over planted
// controls, so a clean report is not a check that cannot fire.
//
// The fixtures carry small figures and short commands, which fit at any
// text size, so the answers are rewritten to the magnitudes a real ledger
// and a real Doctor report produce.
(function (d) {
  'use strict';

  var DIGIT = /[0-9]/;
  var SLACK_PX = 1;

  // What each checked view shows once its slowest sources have answered.
  var SETTLED_MARKER = {
    now: ['.kpigrid', '.glance-meter'],
    accounts: ['.acct-row'],
    traffic: ['#traffic-groups'],
    models: ['#models-lanes'],
    config: ['.cfgstrip'],
    doctor: ['.docfix-text']
  };

  var LONG_FIX = 'log in at least one of this pool\'s accounts ' +
    '(`routectl login <provider> --label <label>`); requests to a model naming ' +
    'the pool are refused with `credential_unusable` until one member has a credential';
  var LONG_PROVIDER = 'provider-with-a-rather-long-configured-name';

  var WIDE_METRICS = {
    input_tokens: 2912345,
    output_tokens: 10234567,
    cache_read_billed: 1234567890,
    cache_write_tokens: 23456789,
    cache_write_present: 316,
    ttft_avg_ms: 11634,
    ttft_max_ms: 579412,
    throughput_tok_s: 584.2,
    ctx_peak: 449412,
    ctx_avg: 162345,
    stream_count: 7311,
    cost_usd: 1234.56
  };

  function widenMetrics(m) {
    Object.keys(WIDE_METRICS).forEach(function (k) { m[k] = WIDE_METRICS[k]; });
  }

  d.rewrite(function (url, text) {
    if (url.pathname === '/status/query') {
      var q = JSON.parse(text);
      var data = q.data || {};
      (data.groups || []).forEach(function (g) { widenMetrics(g.metrics); });
      if (data.totals) { widenMetrics(data.totals); }
      if (data.groups && data.groups[0]) { data.groups[0].label = LONG_PROVIDER; }
      return JSON.stringify(q);
    }
    if (url.pathname === '/status') {
      var agg = JSON.parse(text);
      var doctor = agg.panels.doctor.data;
      doctor.report.findings.forEach(function (f) {
        if (f.remediation) { f.remediation = LONG_FIX; }
      });
      return JSON.stringify(agg);
    }
    return text;
  });

  function addressView() {
    return location.hash.slice(1).split('?')[0];
  }

  function settled(view, markers) {
    return function () {
      var b = document.getElementById('body-' + view);
      return !!b && !b.querySelector('.skeleton') &&
        markers.every(function (m) { return !!b.querySelector(m); });
    };
  }

  function rendered(el) {
    return el.getClientRects().length > 0;
  }

  function ownText(el) {
    return el.textContent.replace(/\s+/g, ' ').trim();
  }

  function focusable(el) {
    return el.tabIndex >= 0 ||
      !!el.querySelector('a[href], button, input, select, textarea, summary, [tabindex]');
  }

  function describe(el) {
    return el.tagName.toLowerCase() + '.' + String(el.className).split(' ').join('.') +
      ' "' + ownText(el).slice(0, 80) + '"';
  }

  // The name assistive technology announces for `el`, from aria-labelledby
  // or aria-label: the two sources a scroller region can carry.
  function accessibleName(el) {
    var ids = (el.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean);
    if (ids.length) {
      return ids.map(function (id) {
        var ref = document.getElementById(id);
        return ref ? ownText(ref) : '';
      }).join(' ').trim();
    }
    return (el.getAttribute('aria-label') || '').trim();
  }

  // One table scroller as a keyboard and screen reader user meet it.
  function tableScroller(el) {
    var cap = el.querySelector('table > caption');
    return {
      id: el.id,
      self_focusable: el.tabIndex >= 0,
      role: el.getAttribute('role'),
      name: accessibleName(el),
      caption: cap ? ownText(cap) : null
    };
  }

  function scrollsSideways(el) {
    return /auto|scroll/.test(getComputedStyle(el).overflowX) &&
      el.scrollWidth > el.clientWidth + SLACK_PX;
  }

  // How `el` treats text it cannot fit: null when nothing is hidden, else
  // { kind, ... }. A scroller the keyboard can reach (axe
  // scrollable-region-focusable: the scroller or something inside it takes
  // focus) and a name whose full text is its title are allowed; anything
  // else hides text. An inline box
  // reports no client width, so only boxes that clip are judged here; text
  // that spills out of a box shows up as a collision instead.
  function hiding(el) {
    var text = ownText(el);
    if (!text || el.classList.contains('sr-only')) { return null; }
    var cs = getComputedStyle(el);
    var overX = cs.overflowX !== 'visible' && el.scrollWidth > el.clientWidth + SLACK_PX;
    var overY = cs.overflowY !== 'visible' && el.scrollHeight > el.clientHeight + SLACK_PX;
    if (!overX && !overY) { return null; }
    var scrolls = (overX && /auto|scroll/.test(cs.overflowX)) ||
      (overY && /auto|scroll/.test(cs.overflowY));
    if (scrolls) {
      return focusable(el) ? null : { kind: 'unreachable-scroller', el: describe(el) };
    }
    if (cs.textOverflow === 'ellipsis' && overX && el.getAttribute('title') === el.textContent) {
      return { kind: 'titled-name', el: describe(el) };
    }
    return {
      kind: cs.textOverflow === 'ellipsis' ? 'ellipsis' : 'clipped',
      el: describe(el),
      digits: DIGIT.test(text)
    };
  }

  // A box whose text runs past the window's right edge, outside any scroller.
  // The page clips its own sideways overflow, so nothing else reports this.
  function offscreen(el) {
    if (!el.firstChild || el.firstChild.nodeType !== Node.TEXT_NODE) { return null; }
    if (el.getBoundingClientRect().right <= document.documentElement.clientWidth + SLACK_PX) {
      return null;
    }
    for (var a = el.parentElement; a; a = a.parentElement) {
      if (/auto|scroll/.test(getComputedStyle(a).overflowX)) { return null; }
    }
    return { kind: 'offscreen', el: describe(el), digits: DIGIT.test(ownText(el)) };
  }

  function textRects(node) {
    var range = document.createRange();
    range.selectNodeContents(node);
    return Array.prototype.slice.call(range.getClientRects()).filter(function (r) {
      return r.width > 0 && r.height > 0;
    });
  }

  function intersects(a, b) {
    return a.left < b.right - SLACK_PX && b.left < a.right - SLACK_PX &&
      a.top < b.bottom - SLACK_PX && b.top < a.bottom - SLACK_PX;
  }

  // Every label and value run in one stat row, against every other run in
  // that row: a pair whose drawn text shares pixels is a collision.
  function rowCollisions(row) {
    var runs = [];
    Array.prototype.forEach.call(row.querySelectorAll('.ustat-label, .ustat-value'), function (n) {
      textRects(n).forEach(function (r) { runs.push({ node: n, rect: r }); });
    });
    var hits = [];
    for (var i = 0; i < runs.length; i++) {
      for (var j = i + 1; j < runs.length; j++) {
        if (runs[i].node !== runs[j].node && intersects(runs[i].rect, runs[j].rect)) {
          hits.push(ownText(runs[i].node) + ' x ' + ownText(runs[j].node));
        }
      }
    }
    return { runs: runs.length, hits: hits };
  }

  function fixFit(el) {
    var cs = getComputedStyle(el);
    var hidden = el.scrollWidth > el.clientWidth + SLACK_PX;
    return {
      text: ownText(el),
      hidden_overflow: hidden && !(el.tabIndex >= 0 && /auto|scroll/.test(cs.overflowX)),
      box_inside_card: el.getBoundingClientRect().right <=
        el.closest('.docfinding').getBoundingClientRect().right + SLACK_PX
    };
  }

  // The checks over every rendered element under `root`. `tables` lists
  // every table scroller, drawn or not (a closed disclosure holds one);
  // `table_scrollers` the drawn ones too wide for their card.
  function audit(root) {
    var out = { inspected: 0, digit_elements: 0, hidden: [], titled_names: [], tables: [],
      table_scrollers: [], stat_rows: 0, stat_runs: 0, collisions: [], fixes: [] };
    Array.prototype.forEach.call(root.querySelectorAll('*'), function (el) {
      if (!rendered(el)) { return; }
      out.inspected += 1;
      if (DIGIT.test(el.textContent)) { out.digit_elements += 1; }
      var h = hiding(el) || offscreen(el);
      if (!h) { return; }
      (h.kind === 'titled-name' ? out.titled_names : out.hidden).push(h);
    });
    Array.prototype.forEach.call(root.querySelectorAll('.tablewrap'), function (el) {
      out.tables.push(tableScroller(el));
      if (rendered(el) && scrollsSideways(el)) { out.table_scrollers.push(tableScroller(el)); }
    });
    Array.prototype.forEach.call(root.querySelectorAll('.ucard-stats'), function (row) {
      if (!rendered(row)) { return; }
      var c = rowCollisions(row);
      out.stat_rows += 1;
      out.stat_runs += c.runs;
      out.collisions = out.collisions.concat(c.hits);
    });
    Array.prototype.forEach.call(root.querySelectorAll('.docfix-text'), function (el) {
      if (rendered(el)) { out.fixes.push(fixFit(el)); }
    });
    return out;
  }

  // Planted cases the audit must catch, and one it must let through. Each
  // sits in its own box at a fixed width so its outcome does not depend on
  // the view around it.
  function controls(host) {
    var box = document.createElement('div');
    box.id = 'layout-controls';
    box.innerHTML =
      '<div class="ctl" id="ctl-ellipsis" style="width:3rem;overflow:hidden;' +
        'text-overflow:ellipsis;white-space:nowrap">123456789012345</div>' +
      '<div class="ctl" id="ctl-clipped" style="width:3rem;overflow:hidden;' +
        'white-space:nowrap">987654321098765</div>' +
      '<div class="ctl" id="ctl-titled" title="named-provider-with-a-long-name" ' +
        'style="width:3rem;overflow:hidden;text-overflow:ellipsis;' +
        'white-space:nowrap">named-provider-with-a-long-name</div>' +
      '<div class="ctl" id="ctl-scroller" style="width:3rem;overflow-x:auto;' +
        'white-space:pre">routectl login anthropic --label a</div>' +
      '<div class="ctl" id="ctl-offscreen" style="position:relative;' +
        'left:100vw">555555555555</div>' +
      '<div class="ctl" id="ctl-scroller-focus" tabindex="0" style="width:3rem;' +
        'overflow-x:auto;white-space:pre">routectl login anthropic --label a</div>' +
      '<div class="tablewrap" id="ctl-table-wrap" style="width:3rem">' +
        '<table><caption>Planted table</caption><tbody><tr>' +
        '<td style="white-space:nowrap">routectl-planted-table-cell</td>' +
        '</tr></tbody></table></div>' +
      '<div class="ucard-stats" style="display:grid;grid-template-columns:' +
        'repeat(2, 2rem);gap:0">' +
        '<div class="ustat"><span class="ustat-label" style="white-space:nowrap">' +
          'CACHE READ / WRITE</span></div>' +
        '<div class="ustat"><span class="ustat-label" style="white-space:nowrap">' +
          'TTFT AVG / MAX</span></div>' +
      '</div>';
    host.appendChild(box);
    var found = audit(box);
    host.removeChild(box);
    return found;
  }

  // The Config view's densest content is a priced model's detail row, which
  // is closed until opened.
  var PRICED_MODEL_TOGGLE = 'config-models-kglm-toggle';

  function openPricedModel() {
    var toggle = document.getElementById(PRICED_MODEL_TOGGLE);
    if (!toggle) { throw new Error('no element #' + PRICED_MODEL_TOGGLE); }
    if (toggle.getAttribute('aria-expanded') !== 'true') { toggle.click(); }
  }

  // ---- the verdict strip through an outage --------------------------------

  // A sentence as long as a real seat verdict, so the strip has to share its
  // line with the outage note and the link.
  var OUTAGE_VERDICT = {
    severity: 'degraded',
    text: 'seat-b (b): Not logged in, for 40 min',
    target: 'accounts'
  };

  var aggregateDown = false;
  var innerFetch = window.fetch;
  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    return innerFetch(input, init).then(function (resp) {
      if (aggregateDown && url.pathname === '/status') { throw new TypeError('Failed to fetch'); }
      return resp;
    });
  };

  function lineCount(node) {
    var range = document.createRange();
    range.selectNodeContents(node);
    var tops = {};
    Array.prototype.forEach.call(range.getClientRects(), function (r) {
      if (r.width > 0) { tops[Math.round(r.top)] = true; }
    });
    return Object.keys(tops).length;
  }

  d.scenario('layout-verdict-outage', function () {
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      agg.panels.attention.data.verdict = OUTAGE_VERDICT;
      return JSON.stringify(agg);
    });
    var sentence = document.getElementById('verdict-text');
    var note = document.getElementById('verdict-note');
    return d.waitUntil('the served verdict', function () {
      return sentence.textContent === OUTAGE_VERDICT.text;
    }).then(function () {
      aggregateDown = true;
      return d.waitUntil('the outage note', function () {
        return !note.hidden && note.textContent.indexOf('cannot be reached') >= 0;
      }, 30000);
    }).then(function () {
      var banner = document.getElementById('banner');
      var textBox = sentence.getBoundingClientRect();
      return {
        root_font_px: parseFloat(getComputedStyle(document.documentElement).fontSize),
        lead_width: document.querySelector('.verdict-lead').clientWidth,
        text_width: textBox.width,
        text_lines: lineCount(sentence),
        lead_rows_wrapped: Array.prototype.some.call(
          document.querySelector('.verdict-lead').children,
          function (child) {
            return rendered(child) && child.getBoundingClientRect().top >= textBox.bottom - SLACK_PX;
          }),
        note: note.textContent,
        link_text: document.getElementById('verdict-link').textContent,
        banner_hidden: banner.hidden,
        banner_text: banner.textContent
      };
    });
  });

  // ---- the config path at the row's width ---------------------------------

  // Longer than one row at 1440 px, with named segments of many lengths.
  var LONG_CONFIG_PATH = '/home/operator-with-a-long-account-name/.config/routectl/' +
    'profiles/a-rather-long-profile-directory-name/nested/another-level-of-folders/' +
    'with-some-more-depth/and-a-segment-that-is-quite-long-on-its-own/x/' +
    'the-last-folder-before-the-file/one-more/and-again-one-more/config.toml';

  // The same shape as a Windows path: backslashes, and a drive letter.
  var LONG_WINDOWS_CONFIG_PATH = 'C:\\Users\\operator-with-a-long-account-name\\AppData\\' +
    'Roaming\\routectl\\profiles\\a-rather-long-profile-directory-name\\nested\\' +
    'another-level-of-folders\\with-some-more-depth\\' +
    'and-a-segment-that-is-quite-long-on-its-own\\x\\the-last-folder-before-the-file\\' +
    'one-more\\and-again-one-more\\config.toml';

  // Each character of `el`'s text with the top of the line it is drawn on.
  function charTops(el) {
    var walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    var out = [];
    var range = document.createRange();
    for (var node = walker.nextNode(); node; node = walker.nextNode()) {
      for (var i = 0; i < node.data.length; i++) {
        range.setStart(node, i);
        range.setEnd(node, i + 1);
        var r = range.getBoundingClientRect();
        out.push({ ch: node.data.charAt(i), top: Math.round(r.top) });
      }
    }
    return out;
  }

  // Every place the text moves to a new line, as the characters either side.
  function lineBreaks(chars) {
    var breaks = [];
    for (var i = 1; i < chars.length; i++) {
      if (chars[i].top > chars[i - 1].top + SLACK_PX) {
        breaks.push(chars[i - 1].ch + '|' + chars[i].ch);
      }
    }
    return breaks;
  }

  function factValue(strip, label) {
    var found = null;
    Array.prototype.forEach.call(strip.querySelectorAll('.cfgfact'), function (f) {
      if (f.querySelector('.cfgfact-label').textContent === label) {
        found = f.querySelector('.cfgfact-value');
      }
    });
    if (!found) { throw new Error('no strip fact ' + label); }
    return found;
  }

  // Serves `path` as the config file and reads how the strip draws it.
  function configPathLayout(path) {
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      agg.panels.config.data.source.config_path = path;
      return JSON.stringify(agg);
    });
    return d.waitUntil('the long path', function () {
      var strip = document.querySelector('#body-config .cfgstrip');
      return strip && factValue(strip, 'config file').textContent === path && strip;
    }).then(function (strip) {
      var value = factValue(strip, 'config file');
      var fact = value.closest('.cfgfact');
      var range = document.createRange();
      range.selectNodeContents(value);
      var selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      var selected = selection.toString();
      selection.removeAllRanges();
      var chars = charTops(value);
      var lines = {};
      chars.forEach(function (c) { lines[c.top] = true; });
      var others = Array.prototype.filter.call(strip.querySelectorAll('.cfgfact'),
        function (f) { return f !== fact; });
      return {
        inner_width: window.innerWidth,
        wire: path,
        text: value.textContent,
        selected: selected,
        title: value.getAttribute('title'),
        lines: Object.keys(lines).length,
        breaks: lineBreaks(chars),
        cut: value.scrollWidth > value.clientWidth + SLACK_PX,
        right_inside_strip: value.getBoundingClientRect().right <=
          strip.getBoundingClientRect().right + SLACK_PX,
        row_to_itself: others.every(function (f) {
          var a = f.getBoundingClientRect();
          var b = fact.getBoundingClientRect();
          return a.top >= b.bottom - SLACK_PX || a.bottom <= b.top + SLACK_PX;
        }),
        page_scrolls_sideways:
          document.documentElement.scrollWidth > document.documentElement.clientWidth
      };
    });
  }

  d.scenario('layout-config-path', function () {
    return configPathLayout(LONG_CONFIG_PATH);
  });

  d.scenario('layout-config-path-windows', function () {
    return configPathLayout(LONG_WINDOWS_CONFIG_PATH);
  });

  // The pieces the page cuts each of the test's paths into, once the page
  // has drawn an answer; the test patches the page to publish its splitter
  // and the paths.
  d.scenario('layout-config-path-pieces', function () {
    return d.waitFor('/status', 1).then(function () {
      return window.routectlPathPieceInputs.map(function (path) {
        return window.routectlPathPieces(path);
      });
    });
  });

  d.scenario('layout-check', function () {
    var view = addressView();
    var marker = SETTLED_MARKER[view];
    if (!marker) { throw new Error('no settle marker for view ' + view); }
    return d.waitUntil('the view to settle', settled(view, marker)).then(function () {
      if (view === 'config') { openPricedModel(); }
      return d.waitFor('/status', 1);
    }).then(function () {
      var body = document.getElementById('body-' + view);
      var out = {
        view: view,
        inner_width: window.innerWidth,
        root_font_px: parseFloat(getComputedStyle(document.documentElement).fontSize),
        page_scrolls_sideways:
          document.documentElement.scrollWidth > document.documentElement.clientWidth,
        strip: audit(document.getElementById('verdict')),
        body: audit(body),
        controls: controls(body),
        glance_meters: body.querySelectorAll('.glance-meter').length,
        config_open_details: body.querySelectorAll('#config-models tr.detail-row:not([hidden])').length,
        config_pool_members: body.querySelectorAll('#config-pools .cfgpool-member').length,
        traffic_rows: body.querySelectorAll('#traffic-groups tr.exp-row').length
      };
      return out;
    });
  });
})(window.routectlDriver);
