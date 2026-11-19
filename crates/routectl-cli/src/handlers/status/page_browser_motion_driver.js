// ---- scenarios: a refresh marks the figures whose value moved -------------
//
// Test-only snippet injected after page_browser_driver.js. Every figure,
// numeric cell and meter carries its raw value in `data-v`; an observation
// snapshots each such node in the active view's body and, after a round,
// sorts the nodes the morph kept into those whose value moved and those whose
// value did not, reading the change mark on each.
(function (d) {
  'use strict';

  var CHANGED = 'changed';
  var FADE = 'changefade';

  // The view the address opens. The driver starts before the page's load
  // handler has selected it, so the tab bar cannot say yet.
  function addressView() {
    return location.hash.slice(1).split('?')[0];
  }

  function body(view) {
    return document.getElementById('body-' + view);
  }

  function settled(view) {
    return function () {
      var b = body(view);
      return !!b && !!b.querySelector('[data-v]') && !b.querySelector('.skeleton');
    };
  }

  function snapshot(view) {
    return Array.prototype.map.call(body(view).querySelectorAll('[data-v]'), function (node) {
      return { node: node, v: node.getAttribute('data-v') };
    });
  }

  function marked(view) {
    return body(view).querySelectorAll('.' + CHANGED).length;
  }

  function describe(node) {
    return node.tagName.toLowerCase() + '.' + node.className + '[' + node.getAttribute('data-v') + ']';
  }

  // The kept nodes of `before`, split by whether their value moved, with the
  // mark and computed animation of each.
  function compare(before) {
    var out = { moved: [], unmoved: [], replaced: 0 };
    before.forEach(function (s) {
      if (!s.node.isConnected) { out.replaced += 1; return; }
      var now = s.node.getAttribute('data-v');
      var seen = {
        node: describe(s.node),
        was: s.v,
        now: now,
        text: s.node.textContent,
        pct: s.node.style.getPropertyValue('--pct') || null,
        marked: s.node.classList.contains(CHANGED),
        animation: getComputedStyle(s.node).animationName
      };
      (now === s.v ? out.unmoved : out.moved).push(seen);
    });
    return out;
  }

  function firstMoved(before) {
    for (var i = 0; i < before.length; i++) {
      var s = before[i];
      if (s.node.isConnected && s.node.getAttribute('data-v') !== s.v) { return s.node; }
    }
    return null;
  }

  function fadeOf(node) {
    var running = node.getAnimations().filter(function (a) { return a.animationName === FADE; });
    return running.length ? running[0] : null;
  }

  // The end of `node`'s fade as the browser reports it. Headless Chrome under
  // a virtual-time budget renders too few frames for a CSS animation to reach
  // its end, so the event the page listens for is dispatched here.
  function endFade(node) {
    node.dispatchEvent(new AnimationEvent('animationend', { animationName: FADE, bubbles: true }));
  }

  // One refresh of the view on screen: the figures that moved are marked and
  // the rest are not. Then a probe figure that moved is followed: moving again
  // while its fade still runs restarts the fade (a new animation, not the
  // running one), and the fade's end clears the mark.
  d.scenario('motion-refresh', function () {
    var view = addressView();
    var out = {};
    var probe = null;
    var fade = null;
    return d.waitUntil('the view to settle', settled(view)).then(function () {
      out.first_paint_marked = marked(view);
      var before = snapshot(view);
      return d.waitFor('/status', 1).then(function () {
        out.round = compare(before);
        probe = firstMoved(before);
        if (!probe) { throw new Error('no figure moved across the round'); }
        out.probe = describe(probe);
        fade = fadeOf(probe);
        var again = snapshot(view);
        return d.waitFor('/status', 1).then(function () {
          var next = fadeOf(probe);
          out.second_round = {
            moved: compare(again.filter(function (s) { return s.node === probe; })).moved.length === 1,
            marked: probe.classList.contains(CHANGED),
            fading: !!fade && !!next,
            restarted: !!fade && !!next && next !== fade
          };
          endFade(probe);
          out.after_end = { marked: probe.classList.contains(CHANGED) };
          return out;
        });
      });
    });
  });

  // Leave the view, let its sources move on, and come back: the return is a
  // first paint of values the operator was not watching, so nothing is marked
  // even though values on kept nodes differ from what the pane last showed.
  d.scenario('motion-view-switch', function () {
    var home = addressView();
    var away = window.routectlMotionAway;
    var out = {};
    return d.waitUntil('the view to settle', settled(home)).then(function () {
      var before = snapshot(home);
      document.getElementById('tab-' + away).click();
      return d.waitFor('/status', 2).then(function () {
        document.getElementById('tab-' + home).click();
        out.on_return = compare(before);
        out.on_return_marked = marked(home);
        return out;
      });
    });
  });
})(window.routectlDriver);
