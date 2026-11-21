// ---- scenarios: the aggregate round applies every GET panel's record -----
//
// Test-only snippet injected after page_browser_driver.js, on a page patched
// to publish its source records as `window.routectlSourceRecords`
// (page_browser_sources_tests.rs). Each observation copies the state of the
// attention and accounts records and the payload fields the test compares
// against the fixture.
(function (d) {
  'use strict';

  function records() {
    var all = window.routectlSourceRecords;
    if (!all) { throw new Error('the page did not publish its source records'); }
    return all;
  }

  // A source the page does not poll has no record at all: reported as
  // `absent` rather than thrown, so the test names what is missing.
  var ABSENT = { state: 'absent', code: null, asOf: null, data: null };

  function observe() {
    var all = records();
    var attention = all.attention || ABSENT;
    var accounts = all.accounts || ABSENT;
    return {
      attention: {
        state: attention.state,
        code: attention.code,
        as_of: attention.asOf,
        verdict: attention.data ? attention.data.verdict : null,
        items: attention.data ? attention.data.items.length : null
      },
      accounts: {
        state: accounts.state,
        code: accounts.code,
        as_of: accounts.asOf,
        rows: accounts.data
          ? accounts.data.accounts.map(function (row) { return row.account; })
          : null
      }
    };
  }

  // The second /status answer carries an attention envelope of another wire
  // version, so the record it lands in must refuse it while accounts, in the
  // same answer, stays live.
  function otherAttentionVersion(text) {
    var agg = JSON.parse(text);
    agg.panels.attention.schema_version += 1;
    return JSON.stringify(agg);
  }

  d.scenario('sources-aggregate-records', function () {
    d.rewrite(function (url, text, n) {
      return (url.pathname === '/status' && n === 1) ? otherAttentionVersion(text) : text;
    });
    var out = {};
    out.before = observe();
    return d.waitFor('/status', 1).then(function () {
      out.applied = observe();
      return d.waitFor('/status', 1);
    }).then(function () {
      out.refused = observe();
      return out;
    });
  });
})(window.routectlDriver);

// ---- scenario: a full cycle through every view ----------------------------
//
// Opens each tab in the tab bar in turn and waits, on each, for one more
// /status answer and a body with no loading skeleton, so every source a view
// polls has had its round. Reports the views visited in order; the request
// log the driver keeps is what the test reads.
(function (d) {
  'use strict';

  function settled(view) {
    var body = document.getElementById('body-' + view);
    return !!body && body.childElementCount > 0 && !body.querySelector('.skeleton');
  }

  function visit(tab, visited) {
    var view = tab.getAttribute('data-tab');
    return function () {
      tab.click();
      return d.waitFor('/status', 1)
        .then(function () {
          return d.waitUntil('the ' + view + ' view to settle', function () { return settled(view); });
        })
        .then(function () { visited.push(view); });
    };
  }

  d.scenario('sources-full-cycle', function () {
    var tabs = Array.prototype.slice.call(document.querySelectorAll('#tabbar [role="tab"]'));
    var visited = [];
    var step = d.waitFor('/status', 1);
    tabs.forEach(function (tab) { step = step.then(visit(tab, visited)); });
    return step.then(function () { return { visited: visited }; });
  });
})(window.routectlDriver);
