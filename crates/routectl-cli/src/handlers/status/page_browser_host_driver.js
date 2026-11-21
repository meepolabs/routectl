// ---- scenarios: the host owns each section's panel states ----------------
//
// Test-only snippet injected after page_browser_driver.js, on a page whose
// Models view composes one probe section with no guard of its own
// (page_browser_host_tests.rs). The probe reads `window.routectlHostProbe`:
// `mode` picks what its builder does on the next render, and `calls` records
// the state of every record the host handed it.
(function (d) {
  'use strict';

  var probe = { mode: 'ok', calls: [] };
  window.routectlHostProbe = probe;

  function textOf(selector) {
    var node = document.querySelector(selector);
    return node ? node.textContent : null;
  }

  function observe() {
    var body = document.getElementById('body-models');
    var section = body.querySelector('[data-source="models"]');
    return {
      sections: body.querySelectorAll('[data-source]').length,
      skeleton: !!section && !!section.querySelector('.skeleton'),
      cards: section ? section.querySelectorAll('.unavailable-msg, .incompatible-msg').length : null,
      card: textOf('#body-models .unavailable-msg'),
      head: textOf('#body-models [data-source="models"] .card-title'),
      heads: section ? section.querySelectorAll('.card-title').length : null,
      probe: textOf('#host-probe'),
      calls: probe.calls.slice(),
      status: textOf('#status-models')
    };
  }

  function unavailableModels(text) {
    var agg = JSON.parse(text);
    var models = agg.panels.models;
    agg.panels.models = {
      schema_version: models.schema_version,
      as_of: models.as_of,
      unavailable: 'probe_down',
      data: null
    };
    return JSON.stringify(agg);
  }

  // One /status answer per step but the recovery: the first is held so the
  // loading state can be read, the third reports the models panel unavailable, the
  // rest are the fixture.
  d.scenario('host-section-states', function () {
    var asked = 0;
    d.hold(function (url) {
      if (url.pathname !== '/status') { return 0; }
      asked += 1;
      return asked === 1 ? 3000 : 0;
    });
    d.rewrite(function (url, text, n) {
      return (url.pathname === '/status' && n === 2) ? unavailableModels(text) : text;
    });
    var out = {};
    function step(name, mode) {
      return function () {
        probe.mode = mode;
        return d.waitFor('/status', 1).then(function () { out[name] = observe(); });
      };
    }
    return d.waitUntil('the probe section', function () {
      return document.querySelector('#body-models [data-source="models"]');
    }).then(function () {
      out.loading = observe();
    })
      .then(step('success', 'ok'))
      .then(step('throwing', 'throw'))
      .then(function () {
        // A repaint with no new models answer, since a landing payload
        // clears a source's fault by itself: only the clean build can.
        probe.mode = 'ok';
        document.getElementById('tab-config').click();
        document.getElementById('tab-models').click();
        out.recovered = observe();
      })
      .then(step('unavailable', 'ok'))
      .then(step('inner_throwing', 'inner-throw'))
      .then(function () { return out; });
  });
})(window.routectlDriver);
