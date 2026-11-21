  // ---- shared:attention ------------------------------------------------

  // The daemon's needs-attention list as it sent it: which conditions count,
  // their severity, wording, age, raw token and fix all come from the panel,
  // in the panel's order. The page adds only how long ago a cleared item
  // recovered, read against the panel's own as_of. Every string is set as
  // text.
  //
  // More than one view draws this list, so each passes its own id prefix:
  // item ids are `<prefix>-attention-<idKey(key)>`, and two views on one page
  // never share an id.
  var MINUTE_MS = 60000;

  function attentionSection(rec, idPrefix) {
    var data = rec.data;
    if (!Array.isArray(data.items)) {
      throw new Error('attention payload carries no item list');
    }
    var counts = attentionTally(data.counts);
    var wrap = document.createElement('div');
    wrap.className = 'ovsection attn';
    var head = sectionHead('Needs attention', null);
    head.appendChild(attentionCounts(counts));
    wrap.appendChild(head);
    var active = data.items.filter(function (item) { return item.state !== 'recovered'; });
    if (!active.length) { wrap.appendChild(attentionClear()); }
    if (data.items.length) { wrap.appendChild(attentionList(data.items, idPrefix, panelNowMs(rec))); }
    return wrap;
  }

  // The tally of what is active now, as the daemon counted it. Recovered
  // items are listed below but are not part of it.
  function attentionCounts(counts) {
    var strip = document.createElement('span');
    strip.className = 'attn-counts';
    [['broken', counts.broken], ['degraded', counts.degraded]].forEach(function (c) {
      var n = c[1];
      if (n <= 0) { return; }
      var part = document.createElement('span');
      part.className = 'attn-count attn-count--' + c[0];
      part.setAttribute('data-severity', c[0]);
      part.textContent = n + ' ' + c[0];
      strip.appendChild(part);
    });
    return strip;
  }

  function attentionClear() {
    var line = document.createElement('p');
    line.className = 'attn-clear';
    line.textContent = 'Nothing needs attention now.';
    return line;
  }

  function attentionList(items, idPrefix, nowMs) {
    var list = document.createElement('div');
    list.className = 'attn-list';
    var seen = Object.create(null);
    items.forEach(function (item) {
      list.appendChild(attentionItem(item, uniqueItemId(idPrefix + '-attention-' + idKey(item.key), seen), nowMs));
    });
    return list;
  }

  // One item, its own refresh unit: a selection inside its fix or raw token
  // holds this item alone while the rest of the list keeps updating.
  function attentionItem(item, id, nowMs) {
    var recovered = item.state === 'recovered';
    var row = markRefreshUnit(document.createElement('div'), id);
    row.id = id;
    row.className = 'attn-item attn-item--' + (recovered ? 'recovered' : String(item.severity));
    row.appendChild(attentionHead(item, recovered));
    row.appendChild(attentionText('div', 'attn-cause', item.cause));
    if (recovered) {
      row.appendChild(attentionText('div', 'attn-recovered', recoveredText(item.recovered_at_ms, nowMs)));
    }
    if (item.fix) { row.appendChild(attentionFix(item.fix)); }
    if (item.remediation) { row.appendChild(attentionRemediation(item.remediation)); }
    row.appendChild(attentionRaw(item.raw_token));
    return row;
  }

  function attentionHead(item, recovered) {
    var head = document.createElement('div');
    head.className = 'attn-head';
    var dot = document.createElement('span');
    dot.className = 'attn-dot';
    dot.setAttribute('aria-hidden', 'true');
    head.appendChild(dot);
    head.appendChild(attentionText('span', 'attn-sev', recovered ? 'recovered' : item.severity));
    head.appendChild(attentionText('span', 'attn-subject', item.subject));
    var link = attentionTargetLink(item.target);
    if (link) { head.appendChild(link); }
    return head;
  }

  // The fix is the line to run, so it carries the item's emphasis and is a
  // copyable code element: Tab reaches it, and Ctrl+A then Ctrl+C copy it.
  function attentionFix(fix) {
    var wrap = document.createElement('div');
    wrap.className = 'attn-fix';
    wrap.appendChild(attentionText('span', 'attn-label', 'fix'));
    wrap.appendChild(copyableCode('attn-fix-code', fix));
    return wrap;
  }

  // A remediation is an instruction in words, not a line to run, so it is
  // drawn as plain text rather than as a copyable code element.
  function attentionRemediation(text) {
    var wrap = document.createElement('div');
    wrap.className = 'attn-fix attn-remediation';
    wrap.appendChild(attentionText('span', 'attn-label', 'fix'));
    wrap.appendChild(attentionText('span', 'attn-remediation-text', text));
    return wrap;
  }

  function attentionRaw(token) {
    var wrap = document.createElement('div');
    wrap.className = 'attn-raw';
    wrap.appendChild(attentionText('span', 'attn-label', 'raw'));
    wrap.appendChild(copyableCode('attn-raw-code', token));
    return wrap;
  }

  function attentionText(tag, cls, value) {
    var node = document.createElement(tag);
    node.className = cls;
    node.textContent = (value === null || value === undefined) ? '' : String(value);
    return node;
  }

  // A link to the view that shows the item's detail. Only a registered view
  // is linked; any other token gets no link rather than one to nowhere.
  function attentionTargetLink(target) {
    var view = (typeof target === 'string') ? viewFor(target) : null;
    if (!view) { return null; }
    var link = viewLink(view);
    link.className = 'attn-target';
    return link;
  }

  // Whole minutes since the item was first seen cleared, against the panel's
  // own clock, so client skew cannot move it.
  function recoveredText(recoveredAtMs, nowMs) {
    var at = Number(recoveredAtMs);
    if (recoveredAtMs === null || recoveredAtMs === undefined || !isFinite(at) || !isFinite(nowMs)) {
      return 'recovered';
    }
    var min = Math.floor(Math.max(0, nowMs - at) / MINUTE_MS);
    return min < 1 ? 'recovered under 1 min ago' : 'recovered ' + min + ' min ago';
  }

  // ---- end shared:attention --------------------------------------------
