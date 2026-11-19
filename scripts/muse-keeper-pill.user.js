// ==UserScript==
// @name         Muse Keeper Pill — Short + Top
// @namespace    appmaka
// @version      1.0.1
// @description  Shortens the Muse Keeper status pill ("Muse Keeper • Active" -> "MKA", "Muse Keeper • Allowed: N" -> "MKALL: N") and moves it to the top of the window, next to the account name, instead of floating at the bottom.
// @match        https://muse.ai/*
// @match        https://*.muse.ai/*
// @run-at       document-start
// @grant        none
// ==/UserScript==

(function () {
  "use strict";

  // Shorten the pill text. "Muse Keeper • Active" -> "MKA",
  // "Muse Keeper • Allowed: 5" -> "MKALL: 5".
  function shorten(el) {
    var t = (el.textContent || "").trim();
    var m = t.match(/Muse Keeper\s*[•·]\s*Allowed:\s*(\d+)/i);
    if (m) {
      el.textContent = "MKALL: " + m[1];
      return true;
    }
    if (/Muse Keeper\s*[•·]\s*Active/i.test(t)) {
      el.textContent = "MKA";
      return true;
    }
    return false;
  }

  // Find the pill by its text, shorten it, and pin it to the top-right
  // of the window (next to the account name area) instead of the bottom.
  // We only restyle it with CSS (position: fixed) — the node stays where
  // the page put it in the DOM, so React re-renders don't break.
  function restylePill(el) {
    if (!el || el.dataset.mkPillDone) return;
    if (!shorten(el)) return;
    el.dataset.mkPillDone = "1";
    el.style.position = "fixed";
    el.style.top = "10px";
    el.style.right = "12px";
    el.style.bottom = "auto";
    el.style.zIndex = "9999";
  }

  // The styled pill node, once found. Null until the first successful
  // restyle; cleared when the node leaves the document (SPA navigation).
  var pillEl = null;
  // Whole-body observer: only alive until the pill is found.
  var mainObs = null;
  // Narrow observer on the pill itself: keeps "Allowed: N" shortened
  // when the count changes after the main observer is disconnected.
  var pillObs = null;
  // Cheap body-level observer (childList only, no subtree): re-arms the
  // main observer only if the pill node disappears.
  var watchObs = null;
  var scanTimer = 0;

  function scan(root) {
    var scope = root || document;
    if (!scope.querySelectorAll) return;
    var all = scope.querySelectorAll("body *");
    for (var i = 0; i < all.length; i++) {
      var el = all[i];
      // Only leaf-ish elements that directly hold the label text.
      if (el.children.length === 0 && /Muse Keeper/i.test(el.textContent || "")) {
        restylePill(el);
        if (el.dataset.mkPillDone) pillEl = el;
      }
    }
    // Also catch the pill container itself if the text lives on a parent.
    var pills = scope.querySelectorAll("[class*='keeper' i], [id*='keeper' i]");
    for (var j = 0; j < pills.length; j++) {
      if (/Muse Keeper/i.test(pills[j].textContent || "")) {
        restylePill(pills[j]);
        if (pills[j].dataset.mkPillDone) pillEl = pills[j];
      }
    }
  }

  // Mutation batches arrive in bursts (React re-renders); collapse them
  // into one scan per 100ms instead of scanning the whole body per batch.
  // A hidden tab never needs the pill — skip the scan until visible.
  function scheduleScan() {
    if (scanTimer) return;
    scanTimer = setTimeout(function () {
      scanTimer = 0;
      if (document.hidden) return;
      scan(document);
      if (pillEl && pillEl.isConnected) settle();
    }, 100);
  }

  // The pill is found and styled: stop watching the whole body. The
  // pill-scoped observer keeps count updates shortened, and the
  // body-level watcher re-arms the main observer if the node goes away.
  function settle() {
    if (mainObs) {
      mainObs.disconnect();
      mainObs = null;
    }
    if (!pillEl || !pillEl.isConnected) return;
    if (!pillObs) {
      pillObs = new MutationObserver(function () {
        if (!pillEl || !pillEl.isConnected) return;
        delete pillEl.dataset.mkPillDone;
        restylePill(pillEl);
      });
      pillObs.observe(pillEl, { characterData: true, subtree: true });
    }
    if (!watchObs) {
      watchObs = new MutationObserver(function () {
        if (pillEl && !pillEl.isConnected) {
          pillEl = null;
          if (pillObs) {
            pillObs.disconnect();
            pillObs = null;
          }
          if (watchObs) {
            watchObs.disconnect();
            watchObs = null;
          }
          startMainObserver();
          scheduleScan();
        }
      });
      watchObs.observe(document.body, { childList: true });
    }
  }

  function startMainObserver() {
    if (mainObs) return;
    mainObs = new MutationObserver(function (mutations) {
      for (var i = 0; i < mutations.length; i++) {
        var m = mutations[i];
        if (m.type === "childList") {
          for (var j = 0; j < m.addedNodes.length; j++) {
            var n = m.addedNodes[j];
            if (n && n.nodeType === 1) scheduleScan();
          }
        } else if (m.type === "characterData") {
          var t = m.target && m.target.parentElement;
          if (t && /Muse Keeper/i.test(t.textContent || "")) {
            scheduleScan();
          }
        }
      }
    });
    mainObs.observe(document.body, {
      childList: true,
      subtree: true,
      characterData: true,
    });
  }

  function start() {
    if (!document.body) {
      setTimeout(start, 300);
      return;
    }
    if (!document.hidden) {
      scan(document);
      if (pillEl && pillEl.isConnected) {
        settle();
        return;
      }
    }
    startMainObserver();
    document.addEventListener("visibilitychange", function () {
      if (document.hidden) return;
      if (pillEl && pillEl.isConnected) return;
      scan(document);
      if (pillEl && pillEl.isConnected) settle();
    });
  }

  start();
})();
