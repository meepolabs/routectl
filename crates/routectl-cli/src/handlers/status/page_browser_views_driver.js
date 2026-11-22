// ---- scenarios: the six views and the hash that opens them ---------------
//
// Test-only snippet injected after page_browser_driver.js. Each scenario
// waits until the active view's body holds no loading skeleton, then reports
// what it shows: the hash after the page canonicalized it, the selected tab,
// the visible panes, the section titles, how many error cards the body
// carries, and the window picker's state.
(function (d) {
  'use strict';

  function activeView() {
    var tab = document.querySelector('#tabbar [aria-selected="true"]');
    return tab ? tab.getAttribute('data-tab') : null;
  }

  function settled() {
    var view = activeView();
    var body = view && document.getElementById('body-' + view);
    return !!body && body.childElementCount > 0 && !body.querySelector('.skeleton');
  }

  function texts(nodes) {
    return Array.prototype.map.call(nodes, function (n) { return n.textContent.trim(); });
  }

  function pickerState() {
    var group = document.getElementById('windowsel');
    var pressed = group.querySelector('button[aria-pressed="true"]');
    var label = document.getElementById('windowless-label');
    return {
      windowless: group.getAttribute('data-windowless'),
      disabled: Array.prototype.map.call(group.querySelectorAll('button'), function (b) {
        return b.disabled;
      }),
      pressed: pressed ? pressed.getAttribute('data-window') : null,
      label: label && !label.hidden ? label.textContent : null
    };
  }

  function viewReport() {
    var view = activeView();
    var body = document.getElementById('body-' + view);
    var panes = document.querySelectorAll('main > section.pane');
    return {
      hash: location.hash,
      view: view,
      visible_panes: Array.prototype.filter.call(panes, function (p) { return !p.hidden; })
        .map(function (p) { return p.id; }),
      titles: texts(body.querySelectorAll('.card-title')),
      error_cards: body.querySelectorAll('.unavailable-msg, .incompatible-msg').length,
      text_length: body.textContent.trim().length,
      picker: pickerState()
    };
  }

  function settledReport() {
    return d.waitUntil('the active view to settle', settled).then(viewReport);
  }

  d.scenario('view-content', settledReport);

  // Each picker button as it reads: its pressed state and the computed
  // marks the pressed rule paints. The fill is transitioned, so running
  // transitions are finished first and the look read is the settled one.
  function pickerLook() {
    var buttons = document.querySelectorAll('#windowsel button');
    return Array.prototype.map.call(buttons, function (b) {
      b.getAnimations().forEach(function (a) { a.finish(); });
      var style = getComputedStyle(b);
      return {
        window: b.getAttribute('data-window'),
        pressed: b.getAttribute('aria-pressed'),
        shadow: style.boxShadow,
        background: style.backgroundColor
      };
    });
  }

  function openView(id) {
    document.getElementById('tab-' + id).click();
    return d.waitUntil('the ' + id + ' view to open', function () {
      return activeView() === id && settled();
    });
  }

  // Week picked on Traffic, then Models, then Traffic again.
  d.scenario('view-range-across-windowless', function () {
    var out = {};
    return d.waitUntil('the active view to settle', settled).then(function () {
      document.querySelector('#windowsel button[data-window="week"]').click();
      return d.waitUntil('week to be pressed', function () {
        return pickerState().pressed === 'week' && settled();
      });
    }).then(function () {
      out.traffic = pickerLook();
      return openView('models');
    }).then(function () {
      out.models = pickerLook();
      out.models_picker = pickerState();
      return openView('traffic');
    }).then(function () {
      out.back = pickerLook();
      out.back_hash = location.hash;
      return out;
    });
  });

  // The positive control for the error-card count: every QUERY answers an
  // envelope with no schema version, so the one section that reads it must
  // show its card.
  d.scenario('view-content-query-broken', function () {
    var inner = window.fetch;
    window.fetch = function (input, init) {
      var url = new URL(String(input), location.href);
      return inner(input, init).then(function (resp) {
        if (url.pathname !== '/status/query') { return resp; }
        return new Response('{}', {
          status: 200,
          headers: { 'Content-Type': 'application/json' }
        });
      });
    };
    return settledReport();
  });
})(window.routectlDriver);
