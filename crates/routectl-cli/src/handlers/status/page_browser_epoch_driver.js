// Late and reordered answers: scenarios for page_browser_epoch_tests.rs.
//
// A second fetch layer over the driver's own. A rule may hold a request's
// answer on the page's timers, deliver it even after the page aborted the
// request (a fetch that does not honour its signal, so the page's own
// currency check is the only thing between a late answer and the screen), or
// replace its status. Every request is logged with when it was issued and
// delivered, whether and when the page aborted it, the view selected when it
// was issued, and the `totals.requests` of the answer the server sent (and,
// for a seat-grouped answer, each account's own count), so a test can tell
// which answer is on screen.
(function (d) {
  'use strict';

  var inner = window.fetch;
  var rules = [];
  var log = [];

  function waitMs(ms, signal) {
    return new Promise(function (resolve, reject) {
      function onAbort() {
        clearTimeout(timer);
        reject(new DOMException('The operation was aborted.', 'AbortError'));
      }
      var timer = setTimeout(function () {
        if (signal) { signal.removeEventListener('abort', onAbort); }
        resolve();
      }, ms);
      if (signal) {
        if (signal.aborted) { onAbort(); return; }
        signal.addEventListener('abort', onAbort);
      }
    });
  }

  function withoutSignal(init) {
    var copy = {};
    Object.keys(init || {}).forEach(function (k) { if (k !== 'signal') { copy[k] = init[k]; } });
    return copy;
  }

  function totalRequests(text) {
    try {
      var data = JSON.parse(text).data;
      return (data && data.totals) ? data.totals.requests : null;
    } catch (e) {
      return null;
    }
  }

  // Each account's request count in a seat-grouped answer, or null for an
  // answer of any other grouping.
  function seatRequests(text) {
    try {
      var data = JSON.parse(text).data;
      if (!data || !Array.isArray(data.groups)) { return null; }
      var seats = data.groups.filter(function (g) { return Object.prototype.hasOwnProperty.call(g, 'account'); });
      if (!seats.length) { return null; }
      var out = {};
      seats.forEach(function (g) { if (g.account) { out[g.account] = g.metrics.requests; } });
      return out;
    } catch (e) {
      return null;
    }
  }

  function selectedView() {
    var tab = document.querySelector('[role="tab"][aria-selected="true"]');
    return tab ? tab.id : null;
  }

  function ruleFor(entry) {
    var picked = null;
    rules.forEach(function (rule) { picked = rule(entry) || picked; });
    return picked;
  }

  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    var entry = {
      seq: log.length,
      path: url.pathname,
      method: (init && init.method) || 'GET',
      body: (init && typeof init.body === 'string') ? JSON.parse(init.body) : null,
      issued_at: Date.now(),
      view: selectedView(),
      aborted: false,
      aborted_at: null,
      delivered_at: null,
      status: null,
      requests: null,
      seat_requests: null
    };
    log.push(entry);
    var signal = init && init.signal;
    if (signal) {
      signal.addEventListener('abort', function () {
        entry.aborted = true;
        entry.aborted_at = Date.now();
      });
    }
    var rule = ruleFor(entry) || {};
    var deaf = !!rule.ignoreAbort;
    return inner(input, deaf ? withoutSignal(init) : init).then(function (resp) {
      return resp.text().then(function (text) {
        return waitMs(rule.ms || 0, deaf ? null : signal).then(function () {
          entry.delivered_at = Date.now();
          entry.status = rule.status || resp.status;
          entry.requests = totalRequests(text);
          entry.seat_requests = seatRequests(text);
          return new Response(rule.status ? '' : text, {
            status: entry.status,
            headers: resp.headers
          });
        });
      });
    });
  };

  function isQuery(entry) { return entry.method === 'QUERY'; }

  function queries() { return log.filter(isQuery); }

  function nthQuery(n) { return queries()[n] || null; }

  function delivered(entry) { return !!entry && entry.delivered_at !== null; }

  function waitDelivered(label, pick, limitMs) {
    return d.waitUntil(label, function () {
      var entry = pick();
      return delivered(entry) ? entry : null;
    }, limitMs);
  }

  function windowButton(name) {
    var btn = document.querySelector('#windowsel button[data-window="' + name + '"]');
    if (!btn) { throw new Error('no window button ' + name); }
    return btn;
  }

  function textOfAll(selector) {
    return Array.prototype.map.call(document.querySelectorAll(selector), function (n) {
      return n.textContent;
    }).join(' | ');
  }

  // What the query-backed view shows: the verdict's request figure (read from
  // the applied QUERY record) and the group cards or probe line built from it.
  function queryShown() {
    var probe = document.getElementById('seat-probe');
    return {
      stats: document.getElementById('verdict-stats').textContent,
      cards: textOfAll('#traffic-groups tr.exp-row'),
      probe: probe ? probe.textContent : null
    };
  }

  function pageShown() {
    var banner = document.getElementById('banner');
    return {
      banner_hidden: banner.hidden,
      banner_class: banner.className,
      banner_text: banner.textContent,
      verdict: document.getElementById('verdict-text').textContent,
      main: document.querySelector('main').textContent
    };
  }

  function holdFirstQuery(rule) {
    var held = false;
    rules.push(function (entry) {
      if (held || !isQuery(entry)) { return null; }
      held = true;
      return rule;
    });
  }

  // Q0 today is held and delivered after the page aborted it; the page has
  // moved to Week by then and Week's answer has applied.
  d.scenario('epoch-old-range-late', function () {
    holdFirstQuery({ ms: 2000, ignoreAbort: true });
    return d.waitUntil('the first QUERY', function () { return nthQuery(0); }).then(function () {
      windowButton('week').click();
      return waitDelivered('the week QUERY', function () { return nthQuery(1); });
    }).then(function () {
      var before = queryShown();
      return waitDelivered('the held today QUERY', function () { return nthQuery(0); })
        .then(function () { return { before: before, after: queryShown(), queries: queries() }; });
    });
  });

  // Today, Week, Today again: the held first Today answer carries the same
  // state key as the current request and is still older than it.
  d.scenario('epoch-same-range-reissued-late', function () {
    holdFirstQuery({ ms: 3000, ignoreAbort: true });
    return d.waitUntil('the first QUERY', function () { return nthQuery(0); }).then(function () {
      windowButton('week').click();
      return waitDelivered('the week QUERY', function () { return nthQuery(1); });
    }).then(function () {
      windowButton('today').click();
      return waitDelivered('the second today QUERY', function () { return nthQuery(2); });
    }).then(function () {
      var before = queryShown();
      return waitDelivered('the held first QUERY', function () { return nthQuery(0); })
        .then(function () { return { before: before, after: queryShown(), queries: queries() }; });
    });
  });

  // One QUERY held for most of its 12 s budget while the 5 s aggregate
  // rounds keep landing; the abort is honoured here, so a superseding nudge
  // would show as an aborted, never delivered request.
  d.scenario('epoch-held-query-cadence', function () {
    holdFirstQuery({ ms: 11000 });
    return waitDelivered('the held QUERY', function () { return nthQuery(0); }, 15000)
      .then(function () {
        return { shown: queryShown(), log: log };
      });
  });

  // The operator leaves the query view while its QUERY is out; the late
  // answer is a refusal of that selection. Coming back to the same selection
  // must fetch it again rather than treat it as refused.
  d.scenario('epoch-left-view-late-refusal', function () {
    holdFirstQuery({ ms: 2000, ignoreAbort: true, status: 400 });
    var queryTab = null;
    return d.waitUntil('the first QUERY', function () { return nthQuery(0); }).then(function () {
      queryTab = document.querySelector('[role="tab"][aria-selected="true"]');
      document.getElementById('tab-config').click();
      return waitDelivered('the held refusal', function () { return nthQuery(0); });
    }).then(function () {
      var returnedAt = Date.now();
      queryTab.click();
      return d.sleep(2000).then(function () {
        var after = queries().filter(function (q) { return q.issued_at >= returnedAt; });
        return {
          refusal: nthQuery(0),
          returned_at: returnedAt,
          queries_after_return: after,
          shown: queryShown()
        };
      });
    });
  });

  // The second aggregate round is held past a QUERY 403 and then delivered
  // even though the page aborted it.
  d.scenario('epoch-terminal-late-aggregate', function () {
    var lateAggregate = null;
    var terminal = {};
    return waitDelivered('the first QUERY', function () { return nthQuery(0); }).then(function () {
      var armedAt = log.length;
      rules.push(function (entry) {
        if (entry.seq < armedAt) { return null; }
        if (entry.path === '/status' && !lateAggregate) {
          lateAggregate = entry;
          return { ms: 3000, ignoreAbort: true };
        }
        if (isQuery(entry)) { return { status: 403 }; }
        return null;
      });
      return d.waitUntil('the held aggregate request', function () { return lateAggregate; });
    }).then(function () {
      document.getElementById('usage-groupby-kalias').click();
      return d.waitUntil('the terminal banner', function () {
        return document.querySelector('#banner.banner--terminal');
      });
    }).then(function () {
      terminal = { at: Date.now(), requests: log.length, shown: pageShown() };
      return waitDelivered('the late aggregate', function () { return lateAggregate; });
    }).then(function () {
      return d.sleep(12000);
    }).then(function () {
      return {
        terminal: terminal,
        late_aggregate: lateAggregate,
        shown: pageShown(),
        requests: log.length,
        log: log
      };
    });
  });
  // ---- Traffic to Accounts ------------------------------------------------

  var ACCOUNTS_SEAT_TRAFFIC = 'accounts-row-kseats-kanthropic_23_a-traffic';

  // What the labeled seat's traffic line says: its request figure, or the
  // words standing in for one.
  function seatTrafficFigure() {
    var line = document.getElementById(ACCOUNTS_SEAT_TRAFFIC);
    if (!line) { return null; }
    var shown = line.querySelector('.acct-traffic-req') || line.querySelector('.acct-traffic-note');
    return shown ? shown.textContent : line.textContent;
  }

  // Every distinct figure the labeled seat's traffic line shows, from the
  // first paint on: a mutation observer, so a payload drawn and replaced
  // between two samples is still seen.
  function watchSeatTraffic() {
    var seen = [];
    function sample() {
      var text = seatTrafficFigure();
      if (text !== null && seen.indexOf(text) < 0) { seen.push(text); }
    }
    new MutationObserver(sample).observe(document.getElementById('body-accounts'), {
      subtree: true, childList: true, characterData: true
    });
    return seen;
  }

  function accountsShown() {
    var lines = document.querySelectorAll('#body-accounts .acct-traffic');
    return {
      stats: document.getElementById('verdict-stats').textContent,
      seat_line: seatTrafficFigure(),
      lines: Array.prototype.map.call(lines, function (n) { return n.textContent; })
    };
  }

  // Traffic's first QUERY is held and delivered after the page aborted it.
  // The operator opens Accounts while it is out; Accounts asks for its seat
  // grouping and draws it, then the late Traffic answer lands.
  d.scenario('epoch-traffic-to-accounts', function () {
    holdFirstQuery({ ms: 3000, ignoreAbort: true });
    var seen = watchSeatTraffic();
    var out = {};
    return d.waitUntil('the Traffic QUERY', function () { return nthQuery(0); }).then(function () {
      document.getElementById('tab-accounts').click();
      return waitDelivered('the Accounts QUERY', function () { return nthQuery(1); });
    }).then(function () {
      return d.waitUntil('the seat traffic figures', function () {
        var line = document.getElementById(ACCOUNTS_SEAT_TRAFFIC);
        return line && line.querySelector('.acct-traffic-req');
      });
    }).then(function () {
      out.before = accountsShown();
      return waitDelivered('the held Traffic QUERY', function () { return nthQuery(0); });
    }).then(function () {
      return d.sleep(300);
    }).then(function () {
      out.after = accountsShown();
      out.queries = queries();
      out.seen = seen;
      return out;
    });
  });

  // The seat probe view's first QUERY, answered and drawn.
  d.scenario('epoch-seat-probe-first-query', function () {
    return waitDelivered('the first QUERY', function () { return nthQuery(0); }).then(function () {
      return d.waitUntil('the probe line', function () {
        return document.getElementById('seat-probe');
      });
    }).then(function () {
      return { queries: queries(), shown: queryShown() };
    });
  });

  // Every QUERY is held long enough to still be out when the operator moves:
  // probe view, then Traffic while the probe's QUERY is out, then back while
  // Traffic's is out, then a cadence round on the probe view.
  d.scenario('epoch-seat-probe-switch', function () {
    rules.push(function (entry) { return isQuery(entry) ? { ms: 1500 } : null; });
    return d.waitUntil('the first QUERY', function () { return nthQuery(0); }).then(function () {
      document.getElementById('tab-traffic').click();
      return d.waitUntil('the Traffic QUERY', function () { return nthQuery(1); });
    }).then(function () {
      document.getElementById('tab-models').click();
      return waitDelivered('the probe QUERY on return', function () { return nthQuery(2); });
    }).then(function () {
      return d.waitUntil('a cadence QUERY', function () { return nthQuery(3); }, 15000);
    }).then(function () {
      return waitDelivered('the cadence QUERY', function () { return nthQuery(3); });
    }).then(function () {
      return { queries: queries() };
    });
  });

  // A seat QUERY is held past a 403 on the next aggregate round, then
  // delivered although the page aborted it.
  d.scenario('epoch-seat-probe-terminal', function () {
    var held = null;
    var refused = null;
    var terminal = {};
    return waitDelivered('the first QUERY', function () { return nthQuery(0); }).then(function () {
      rules.push(function (entry) {
        if (isQuery(entry) && !held) {
          held = entry;
          return { ms: 8000, ignoreAbort: true };
        }
        if (held && !refused && entry.path === '/status') {
          refused = entry;
          return { status: 403 };
        }
        return null;
      });
      return d.waitUntil('the terminal banner', function () {
        return document.querySelector('#banner.banner--terminal');
      }, 30000);
    }).then(function () {
      terminal = { at: Date.now(), shown: pageShown(), query: queryShown() };
      return waitDelivered('the held seat QUERY', function () { return held; }, 15000);
    }).then(function () {
      return d.sleep(1000);
    }).then(function () {
      return {
        terminal: terminal,
        held: held,
        shown: pageShown(),
        query: queryShown()
      };
    });
  });
})(window.routectlDriver);
