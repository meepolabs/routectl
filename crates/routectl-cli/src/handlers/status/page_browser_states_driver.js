// Panel states: scenarios for page_browser_states_tests.rs.
//
// A fetch layer over the driver's own. A rule picks, per request, whether
// its answer is held on the page's timers, fails the way an unreachable
// daemon does, comes back with another status, or has its JSON rewritten.
// Every answer handed to the page is counted per path, so a scenario can
// wait for the next one and a test can count the rounds that landed.
(function (d) {
  'use strict';

  var inner = window.fetch;
  var rules = [];
  var answered = {};

  function abortError() {
    return new DOMException('The operation was aborted.', 'AbortError');
  }

  function waitMs(ms, signal) {
    return new Promise(function (resolve, reject) {
      if (signal && signal.aborted) { reject(abortError()); return; }
      var timer = setTimeout(function () {
        if (signal) { signal.removeEventListener('abort', onAbort); }
        resolve();
      }, ms);
      function onAbort() { clearTimeout(timer); reject(abortError()); }
      if (signal) { signal.addEventListener('abort', onAbort); }
    });
  }

  function withoutSignal(init) {
    var copy = {};
    Object.keys(init || {}).forEach(function (k) { if (k !== 'signal') { copy[k] = init[k]; } });
    return copy;
  }

  function ruleFor(entry) {
    var picked = null;
    rules.forEach(function (rule) { picked = rule(entry) || picked; });
    return picked || {};
  }

  function countAnswer(path) {
    answered[path] = (answered[path] || 0) + 1;
  }

  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    var entry = {
      path: url.pathname,
      method: (init && init.method) || 'GET',
      body: (init && typeof init.body === 'string') ? JSON.parse(init.body) : null
    };
    var rule = ruleFor(entry);
    // A deaf rule delivers the answer even after the page aborted the
    // request, as a fetch that ignores its signal would.
    var signal = rule.ignoreAbort ? null : (init && init.signal);
    return inner(input, rule.ignoreAbort ? withoutSignal(init) : init).then(function (resp) {
      return resp.text().then(function (text) {
        return waitMs(rule.ms || 0, signal).then(function () {
          countAnswer(entry.path);
          if (isSeriesQuery(entry)) { countAnswer(SERIES_ANSWERS); }
          if (rule.fail) { throw new TypeError('Failed to fetch'); }
          var body = rule.rewrite ? JSON.stringify(rule.rewrite(JSON.parse(text))) : text;
          return new Response(rule.status ? '' : body, {
            status: rule.status || resp.status,
            headers: resp.headers
          });
        });
      });
    });
  };

  // Answers to the bucketed QUERY Now reads, counted apart from Traffic's.
  var SERIES_ANSWERS = 'series query';

  function answers(path) { return answered[path] || 0; }

  function nextAnswer(path, limitMs) {
    var target = answers(path) + 1;
    return d.waitUntil('the next ' + path + ' answer', function () {
      return answers(path) >= target;
    }, limitMs);
  }

  function isAggregate(entry) { return entry.path === '/status'; }

  function isWeekQuery(entry) {
    return entry.method === 'QUERY' && !!entry.body && entry.body.window === 'week';
  }

  function query(selector) { return document.querySelector(selector); }

  function textOf(selector) {
    var node = query(selector);
    return node ? node.textContent : null;
  }

  function errorCards(root) {
    return root.querySelectorAll('.unavailable-msg, .incompatible-msg').length;
  }

  function windowButton(name) {
    var btn = query('#windowsel button[data-window="' + name + '"]');
    if (!btn) { throw new Error('no window button ' + name); }
    return btn;
  }

  // ---- a failed refresh keeps what the source rendered ------------------

  var CONFIG_TOGGLE = 'config-models-kglm-toggle';
  var CONFIG_FIGURE = '#body-config .cfgstrip';
  var CONFIG_SECTION = '#body-config [data-source="config"]';

  function configObservation(toggle, strip) {
    var section = query(CONFIG_SECTION);
    return {
      same_toggle: toggle === document.getElementById(CONFIG_TOGGLE),
      same_figure_node: strip === query(CONFIG_FIGURE),
      connected: toggle.isConnected && strip.isConnected,
      figure: textOf(CONFIG_FIGURE),
      section_stale: !!section && section.classList.contains('section--stale'),
      status: textOf('#status-config'),
      error_cards: errorCards(document.getElementById('body-config'))
    };
  }

  d.scenario('states-failure-after-good-round', function () {
    var failing = false;
    rules.push(function (entry) { return (failing && isAggregate(entry)) ? { fail: true } : null; });
    var toggle = null;
    var strip = null;
    var out = {};
    return d.waitForElement(CONFIG_TOGGLE).then(function (node) {
      toggle = node;
      return nextAnswer('/status');
    }).then(function () {
      strip = query(CONFIG_FIGURE);
      out.good = configObservation(toggle, strip);
      failing = true;
      return d.waitUntil('a not-current status line', function () {
        return textOf('#status-config') !== '';
      });
    }).then(function () {
      out.failed = configObservation(toggle, strip);
      failing = false;
      return d.waitUntil('the recovery', function () {
        return textOf('#status-config') === '';
      }, 30000);
    }).then(function () {
      out.recovered = configObservation(toggle, strip);
      return out;
    });
  });

  d.scenario('states-failure-on-first-round', function () {
    rules.push(function (entry) { return isAggregate(entry) ? { fail: true } : null; });
    return d.waitUntil('an error card in Config', function () {
      return query('#body-config .unavailable-msg');
    }).then(function () {
      return {
        card: textOf('#body-config .unavailable-msg'),
        status: textOf('#status-config'),
        error_cards: errorCards(document.getElementById('body-config'))
      };
    });
  });

  // ---- an invalid health payload costs the sections that read it --------

  function brokenHealth(json) {
    json.panels.health.data = { targets: 7, pools: 7, seat_driver: 7 };
    return json;
  }

  function sectionsOf(view) {
    var body = document.getElementById('body-' + view);
    var sections = body.querySelectorAll('[data-source]');
    return {
      sections: Array.prototype.map.call(sections, function (s) {
        return {
          source: s.getAttribute('data-source'),
          error_cards: errorCards(s),
          text_length: s.textContent.trim().length
        };
      }),
      error_cards: errorCards(body),
      status: textOf('#status-' + view)
    };
  }

  function settledView(view) {
    return d.waitUntil(view + ' to settle', function () {
      var body = document.getElementById('body-' + view);
      return body.childElementCount > 0 && !body.querySelector('.skeleton');
    });
  }

  function visit(view) {
    return function () {
      document.getElementById('tab-' + view).click();
      return settledView(view).then(function () { return sectionsOf(view); });
    };
  }

  d.scenario('states-invalid-health', function () {
    rules.push(function (entry) { return isAggregate(entry) ? { rewrite: brokenHealth } : null; });
    var out = { views: {} };
    var chain = settledView('accounts').then(function () {
      out.views.accounts = sectionsOf('accounts');
    });
    ['models', 'now', 'traffic'].forEach(function (view) {
      chain = chain.then(visit(view)).then(function (seen) { out.views[view] = seen; });
    });
    return chain.then(function () {
      out.rounds_before = answers('/status');
      return d.sleep(11000);
    }).then(function () {
      out.rounds_after = answers('/status');
      out.poll = textOf('#poll-label');
      return out;
    });
  });

  // ---- missing data reads as unavailable, never as a zero ---------------

  function withoutData(json) {
    var health = json.panels.health.data;
    delete json.panels.usage;
    delete health.seat_driver.sweeps;
    delete health.pools[0].eligible;
    return json;
  }

  function definition(root, term) {
    var terms = root ? root.querySelectorAll('dt') : [];
    for (var i = 0; i < terms.length; i++) {
      if (terms[i].textContent === term) { return terms[i].nextElementSibling.textContent; }
    }
    return null;
  }

  function accountsReading() {
    var body = document.getElementById('body-accounts');
    var driver = body.querySelector('[data-unit="Background seat driver"]');
    var pools = body.querySelector('[data-unit="Pool seats"]');
    var quota = body.querySelector('[data-source="usage"]');
    return {
      sweeps: definition(driver, 'sweeps'),
      first_pool_eligible: definition(pools, 'eligible'),
      quota_card: quota ? textOf('#body-accounts [data-source="usage"] .unavailable-msg') : null,
      quota_zero_figures: quota ? quota.querySelectorAll('.mag-zero').length : null
    };
  }

  d.scenario('states-missing-data', function () {
    var missing = false;
    rules.push(function (entry) {
      return (missing && isAggregate(entry)) ? { rewrite: withoutData } : null;
    });
    var out = {};
    return settledView('accounts').then(function () {
      out.present = accountsReading();
      missing = true;
      return nextAnswer('/status');
    }).then(function () {
      return d.sleep(200);
    }).then(function () {
      out.missing = accountsReading();
      return out;
    });
  });

  // ---- a range change is pending until its answer applies ---------------

  var TRAFFIC_FIGURES = '#body-traffic .ucard-figs';

  function trafficObservation() {
    var pane = document.getElementById('pane-traffic');
    return {
      busy: pane.getAttribute('aria-busy'),
      pending_class: pane.classList.contains('pane--pending'),
      week_pressed: windowButton('week').getAttribute('aria-pressed'),
      figures: textOf(TRAFFIC_FIGURES),
      cards: document.querySelectorAll('#body-traffic .ucard').length,
      error_cards: errorCards(document.getElementById('body-traffic'))
    };
  }

  function pendingScenario(name, weekRule, afterClick) {
    d.scenario(name, function () {
      var out = {};
      rules.push(function (entry) { return isWeekQuery(entry) ? weekRule : null; });
      return d.waitUntil('traffic figures', function () { return query(TRAFFIC_FIGURES); })
        .then(function () {
          out.before = trafficObservation();
          out.query_answers_at_click = answers('/status/query');
          windowButton('week').click();
          return d.sleep(100);
        })
        .then(function () {
          out.held_early = trafficObservation();
          return afterClick(out);
        })
        .then(function () {
          return d.waitUntil('the pane to stop being busy', function () {
            return !document.getElementById('pane-traffic').hasAttribute('aria-busy');
          }, 20000);
        })
        .then(function () { return d.sleep(500); })
        .then(function () {
          out.applied = trafficObservation();
          return out;
        });
    });
  }

  pendingScenario('states-range-pending-held', { ms: 3000 }, function (out) {
    return d.sleep(2500).then(function () { out.held_late = trafficObservation(); });
  });

  pendingScenario('states-range-pending-fails', { ms: 1500, status: 503 }, function () {
    return Promise.resolve();
  });

  // Week is superseded by Month while both are held. The Week answer is
  // delivered first, even though the page aborted it, and must not end the
  // pending state; Month's answer, delivered later, does.
  function isMonthQuery(entry) {
    return entry.method === 'QUERY' && !!entry.body && entry.body.window === 'month';
  }

  pendingScenario('states-range-pending-superseded', { ms: 1500, ignoreAbort: true }, function (out) {
    rules.push(function (entry) { return isMonthQuery(entry) ? { ms: 4000 } : null; });
    windowButton('month').click();
    return d.waitUntil('the superseded week answer', function () {
      return answers('/status/query') >= out.query_answers_at_click + 1;
    }).then(function () {
      return d.sleep(200);
    }).then(function () {
      out.after_superseded = trafficObservation();
      out.month_pressed = windowButton('month').getAttribute('aria-pressed');
    });
  });

  // ---- a selection changed on another view resets the query sources -------

  function isAliasQuery(entry) {
    return entry.method === 'QUERY' && !!entry.body && entry.body.group_by === 'alias';
  }

  function trafficCards() {
    var names = document.querySelectorAll('#body-traffic .ucard-name');
    return Array.prototype.map.call(names, function (n) { return n.textContent; });
  }

  function paneBusy(view) {
    return document.getElementById('pane-' + view).getAttribute('aria-busy');
  }

  function crossViewObservation() {
    var obs = trafficObservation();
    obs.labels = trafficCards();
    obs.now_busy = paneBusy('now');
    obs.accounts_busy = paneBusy('accounts');
    return obs;
  }

  function isSeriesQuery(entry) {
    return entry.method === 'QUERY' && !!entry.body && !!entry.body.bucket;
  }

  function nowRequestsTile() {
    var tiles = document.querySelectorAll('#body-now .kpi');
    for (var i = 0; i < tiles.length; i++) {
      if (tiles[i].querySelector('.kpi-label').textContent === 'Requests') {
        return tiles[i].querySelector('.kpi-value');
      }
    }
    return null;
  }

  function nowObservation(tile) {
    var current = nowRequestsTile();
    return {
      figure: current ? current.textContent : null,
      same_node: !!tile && tile === current,
      connected: !!tile && tile.isConnected,
      skeleton: !!document.querySelector('#body-now .skeleton'),
      error_cards: errorCards(document.getElementById('body-now')),
      busy: paneBusy('now')
    };
  }

  // Now populated first, then Traffic grouped by model, then Accounts, then an
  // address naming the alias grouping applied there, then Traffic again with
  // the alias answer held, then Now again with its next answer held: the
  // grouping is not in Now's request, so its payload is still the answer.
  d.scenario('states-group-changed-on-another-view', function () {
    var out = {};
    var holdSeries = false;
    var nowTile = null;
    rules.push(function (entry) { return isAliasQuery(entry) ? { ms: 3000 } : null; });
    rules.push(function (entry) {
      return (holdSeries && isSeriesQuery(entry)) ? { ms: 3000 } : null;
    });
    return d.waitUntil('the Now requests figure', nowRequestsTile)
      .then(function () {
        nowTile = nowRequestsTile();
        out.now_before = nowObservation(nowTile);
        document.getElementById('tab-traffic').click();
        return d.waitUntil('traffic cards', function () { return query(TRAFFIC_FIGURES); });
      })
      .then(function () {
        out.before = crossViewObservation();
        document.getElementById('tab-accounts').click();
        return settledView('accounts');
      })
      .then(function () {
        location.hash = '#accounts?group=alias';
        return d.waitUntil('the alias address applied', function () {
          return location.hash === '#accounts?group=alias';
        });
      })
      .then(function () { return d.sleep(200); })
      .then(function () {
        out.on_accounts = crossViewObservation();
        document.getElementById('tab-traffic').click();
        return d.sleep(100);
      })
      .then(function () {
        out.held = crossViewObservation();
        return d.waitUntil('the pane to stop being busy', function () {
          return !paneBusy('traffic');
        }, 20000);
      })
      .then(function () { return d.sleep(300); })
      .then(function () {
        out.applied = crossViewObservation();
        out.pressed_group = textOf('#body-traffic .gbbtn[aria-pressed="true"]');
        out.series_answers_before_return = answers(SERIES_ANSWERS);
        holdSeries = true;
        document.getElementById('tab-now').click();
        return d.sleep(100);
      })
      .then(function () {
        out.now_returned = nowObservation(nowTile);
        out.series_answers_at_return = answers(SERIES_ANSWERS);
        return out;
      });
  });

  // ---- a malformed query payload is an error card, never a zero ----------

  var QUERY_FAULTS = {
    'missing-totals-errors': function (data) { delete data.totals.errors; },
    'group-missing-a-metric': function (data) { delete data.groups[0].metrics.input_tokens; },
    'malformed-series-bucket': function (data) { data.series.buckets[0].metrics = null; }
  };

  function viewReading(view) {
    var body = document.getElementById('body-' + view);
    return {
      error_card: textOf('#body-' + view + ' [data-source] .unavailable-msg'),
      error_cards: errorCards(body),
      figures: body.querySelectorAll('.ucard, .kpi').length
    };
  }

  function queryFaultScenario(fault, view) {
    d.scenario('states-query-' + fault, function () {
      var broken = false;
      rules.push(function (entry) {
        if (!broken || entry.method !== 'QUERY') { return null; }
        return {
          rewrite: function (json) { QUERY_FAULTS[fault](json.data); return json; }
        };
      });
      var out = {};
      return settledView(view).then(function () {
        out.present = viewReading(view);
        broken = true;
        return nextAnswer('/status/query', 15000);
      }).then(function () {
        return d.sleep(300);
      }).then(function () {
        out.broken = viewReading(view);
        return out;
      });
    });
  }

  queryFaultScenario('missing-totals-errors', 'traffic');
  queryFaultScenario('group-missing-a-metric', 'traffic');
  queryFaultScenario('malformed-series-bucket', 'now');

  // ---- a stale section reports its own source's last success -------------

  d.scenario('states-query-stale-time', function () {
    var failing = false;
    rules.push(function (entry) {
      return (failing && entry.method === 'QUERY') ? { fail: true } : null;
    });
    var out = {};
    return d.waitUntil('traffic figures', function () { return query(TRAFFIC_FIGURES); })
      .then(function () {
        failing = true;
        return d.waitUntil('a stale traffic status line', function () {
          return (textOf('#status-traffic') || '').indexOf('stale: ') === 0;
        }, 15000);
      })
      .then(function () {
        out.first = textOf('#status-traffic');
        out.rounds_at_first = answers('/status');
        return nextAnswer('/status', 10000);
      })
      .then(function () { return nextAnswer('/status', 10000); })
      .then(function () { return d.sleep(200); })
      .then(function () {
        out.later = textOf('#status-traffic');
        out.rounds_later = answers('/status');
        out.figures = textOf(TRAFFIC_FIGURES);
        return out;
      });
  });

  // ---- an unreachable daemon marks the query sections stale too ----------

  var TRAFFIC_SECTION = '#body-traffic [data-source="query"]';

  function trafficOutageObservation(figures) {
    var section = query(TRAFFIC_SECTION);
    var body = document.getElementById('body-traffic');
    return {
      figure: textOf(TRAFFIC_FIGURES),
      same_figure_node: !!figures && figures === query(TRAFFIC_FIGURES),
      connected: !!figures && figures.isConnected,
      section_stale: !!section && section.classList.contains('section--stale'),
      status: textOf('#status-traffic'),
      verdict: textOf('#verdict-text'),
      zero_figures: body.querySelectorAll('.mag-zero').length,
      error_cards: errorCards(body)
    };
  }

  d.scenario('states-outage-marks-query-stale', function () {
    var down = false;
    rules.push(function (entry) {
      return (down && (isAggregate(entry) || entry.method === 'QUERY')) ? { fail: true } : null;
    });
    var out = {};
    var figures = null;
    return d.waitUntil('the Now requests figure', nowRequestsTile)
      .then(function () {
        document.getElementById('tab-traffic').click();
        return d.waitUntil('traffic figures', function () { return query(TRAFFIC_FIGURES); });
      })
      .then(function () { return nextAnswer('/status'); })
      .then(function () { return d.sleep(200); })
      .then(function () {
        figures = query(TRAFFIC_FIGURES);
        out.good = trafficOutageObservation(figures);
        down = true;
        return d.waitUntil('the aggregate to fail', function () {
          return document.getElementById('banner').hidden === false;
        }, 15000);
      })
      .then(function () { return d.sleep(500); })
      .then(function () {
        out.down = trafficOutageObservation(figures);
        down = false;
        return d.waitUntil('the query section to recover', function () {
          var section = query(TRAFFIC_SECTION);
          return !!section && !section.classList.contains('section--stale') &&
            textOf(TRAFFIC_FIGURES) !== out.down.figure;
        }, 45000);
      })
      .then(function () { return d.sleep(300); })
      .then(function () {
        out.recovered = trafficOutageObservation(figures);
        return out;
      });
  });
})(window.routectlDriver);
