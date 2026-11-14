// ==UserScript==
// @name         Keep Muse Agents Alive
// @namespace    appmaka
// @version      1.0.0
// @description  Keeps Muse agent tabs awake in AppMaka: forces the page to report itself as visible, runs a silent audio pulse + 30s heartbeat so background/minimized tabs never throttle, and auto-presses Allow on agent approval prompts so unattended agents keep working.
// @match        https://muse.ai/*
// @match        https://*.muse.ai/*
// @run-at       document-start
// @grant        none
// ==/UserScript==

(function () {
  "use strict";

  // 1. Never let the page think it is hidden. Muse.ai (like most web apps)
  // throttles timers and pauses work when document.hidden is true, which is
  // exactly what happens when the AppMaka window is in the background or
  // minimized. Force the page to always report visible.
  try {
    Object.defineProperty(document, "visibilityState", {
      get: function () { return "visible"; },
      configurable: true,
    });
    Object.defineProperty(document, "hidden", {
      get: function () { return false; },
      configurable: true,
    });
    // Some apps also gate on hasFocus(); report focused so backgrounded
    // tabs are not treated as idle.
    Object.defineProperty(document, "hasFocus", {
      value: function () { return true; },
      configurable: true,
    });
  } catch (e) {
    console.warn("[keep-alive] could not override visibility:", e);
  }

  // Stop visibilitychange handlers from ever firing with "hidden".
  document.addEventListener("visibilitychange", function (e) { e.stopImmediatePropagation(); }, true);
  window.addEventListener("pagehide", function (e) { e.stopImmediatePropagation(); }, true);

  // 2. Silent Web Audio pulse. A running AudioContext is one of the
  // strongest "this page is alive" signals a browser has; a page with an
  // active audio context is not throttled the way a silent background tab
  // is. The oscillator is inaudible (gain 0) — it only keeps the context
  // alive.
  var audioCtx = null;
  function ensureAudio() {
    try {
      if (!audioCtx) {
        var AC = window.AudioContext || window.webkitAudioContext;
        if (!AC) return;
        audioCtx = new AC();
        var osc = audioCtx.createOscillator();
        var gain = audioCtx.createGain();
        gain.gain.value = 0; // silent
        osc.connect(gain);
        gain.connect(audioCtx.destination);
        osc.start();
      }
      if (audioCtx.state === "suspended") {
        audioCtx.resume().catch(function () {});
      }
    } catch (e) {
      // Audio is best-effort; the visibility override + heartbeat still work.
    }
  }
  // Start audio on first user gesture if autoplay policy blocks it, and
  // retry on every heartbeat.
  ["pointerdown", "keydown"].forEach(function (evt) {
    document.addEventListener(evt, ensureAudio, { once: true });
  });
  ensureAudio();

  // 3. 30-second activity heartbeat. Re-asserts the audio context and
  // touches the page so its own idle timers and WebSocket keep-alives keep
  // running instead of being throttled into the background.
  setInterval(function () {
    ensureAudio();
    // A no-op read that still counts as page activity for idle detection.
    void document.title;
  }, 30000);

  // 4. Auto-press Allow on agent approval prompts. When an agent asks to
  // run something, muse.ai shows an approval card with an Allow button.
  // While you are away, this clicks it so the agent keeps working instead
  // of waiting on you. It only clicks buttons whose visible label is
  // exactly "Allow" (or "Approve"), and each button is clicked once.
  // NOTE: this approves whatever the agent asked for — that is the point
  // when you are away, but review the agent's work when you are back.
  function isAllowButton(el) {
    if (!el || el.tagName !== "BUTTON") return false;
    if (el.dataset.keepAliveClicked) return false;
    var label = (el.textContent || "").trim().toLowerCase();
    return label === "allow" || label === "approve";
  }
  function clickAllowButtons(root) {
    var scope = root || document;
    var btns = scope.querySelectorAll ? scope.querySelectorAll("button") : [];
    for (var i = 0; i < btns.length; i++) {
      var b = btns[i];
      if (isAllowButton(b) && !b.disabled && b.offsetParent !== null) {
        b.dataset.keepAliveClicked = "1";
        try { b.click(); } catch (e) {}
        console.log("[keep-alive] auto-pressed Allow");
      }
    }
  }
  var observer = new MutationObserver(function (mutations) {
    for (var i = 0; i < mutations.length; i++) {
      var m = mutations[i];
      if (m.type === "childList") {
        for (var j = 0; j < m.addedNodes.length; j++) {
          var n = m.addedNodes[j];
          if (n && n.nodeType === 1) {
            if (isAllowButton(n)) clickAllowButtons(document);
            else clickAllowButtons(n);
          }
        }
      }
    }
  });
  function startObserver() {
    if (!document.body) {
      setTimeout(startObserver, 500);
      return;
    }
    observer.observe(document.body, { childList: true, subtree: true });
    clickAllowButtons(document);
  }
  startObserver();

  console.log("[keep-alive] Keep Muse Agents Alive is running");
})();
