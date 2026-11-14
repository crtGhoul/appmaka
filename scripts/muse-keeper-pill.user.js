// ==UserScript==
// @name         Muse Keeper Pill — Short + Top
// @namespace    appmaka
// @version      1.0.0
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

  function scan(root) {
    var scope = root || document;
    if (!scope.querySelectorAll) return;
    var all = scope.querySelectorAll("body *");
    for (var i = 0; i < all.length; i++) {
      var el = all[i];
      // Only leaf-ish elements that directly hold the label text.
      if (el.children.length === 0 && /Muse Keeper/i.test(el.textContent || "")) {
        restylePill(el);
      }
    }
    // Also catch the pill container itself if the text lives on a parent.
    var pills = scope.querySelectorAll("[class*='keeper' i], [id*='keeper' i]");
    for (var j = 0; j < pills.length; j++) {
      if (/Muse Keeper/i.test(pills[j].textContent || "")) {
        restylePill(pills[j]);
      }
    }
  }

  function start() {
    if (!document.body) {
      setTimeout(start, 300);
      return;
    }
    scan(document);
    // The pill text updates (Allowed count changes, React re-renders),
    // so keep watching and re-apply.
    var observer = new MutationObserver(function (mutations) {
      for (var i = 0; i < mutations.length; i++) {
        var m = mutations[i];
        if (m.type === "childList") {
          for (var j = 0; j < m.addedNodes.length; j++) {
            var n = m.addedNodes[j];
            if (n && n.nodeType === 1) scan(n);
          }
        } else if (m.type === "characterData") {
          var t = m.target && m.target.parentElement;
          if (t && /Muse Keeper/i.test(t.textContent || "")) {
            delete t.dataset.mkPillDone;
            restylePill(t);
          }
        }
      }
    });
    observer.observe(document.body, {
      childList: true,
      subtree: true,
      characterData: true,
    });
  }

  start();
})();
