// ==UserScript==
// @name         Muse.ai Agent Auto-Allow & Keep-Alive
// @version      1.3.4
// @description  Lag-free auto-allow for muse.ai agents: auto-clicks "A task needs review" banner, "Allow" / "Approve" modals, keep-alive heartbeat, and HUD indicator.
// @match        *://muse.ai/*
// @match        *://*.muse.ai/*
// @match        *muse.ai*
// @run-at       document-start
// @grant        none
// ==/UserScript==

(function () {
  'use strict';

  // Remove any legacy HUDs if present
  const oldHud = document.getElementById('muse-keeper-hud');
  if (oldHud) oldHud.remove();

  if (window.__museKeeperInitialized) {
    return;
  }
  window.__museKeeperInitialized = true;

  const KEEPER_VERSION = '1.3.4';
  let isPaused = false;
  let allowedCount = 0;
  let reviewedCount = 0;
  let lastHeartbeatTime = Date.now();

  const clickedElements = new WeakSet();
  let lastActionTime = 0;

  function log(msg, extra) {
    const ts = new Date().toLocaleTimeString();
    if (extra) {
      console.log(`%c[Muse Keeper ${ts}]%c ${msg}`, 'color: #a855f7; font-weight: bold;', 'color: inherit;', extra);
    } else {
      console.log(`%c[Muse Keeper ${ts}]%c ${msg}`, 'color: #a855f7; font-weight: bold;', 'color: inherit;');
    }
  }

  log(`Loaded v${KEEPER_VERSION} on ${location.href}`);

  // =========================================================================
  // 1. LIGHTWEIGHT KEEP-ALIVE (ZERO AUDIO LAG)
  // =========================================================================

  try {
    Object.defineProperty(document, 'visibilityState', {
      get: () => 'visible',
      configurable: true,
    });
    Object.defineProperty(document, 'hidden', {
      get: () => false,
      configurable: true,
    });
    window.addEventListener(
      'visibilitychange',
      (e) => {
        e.stopImmediatePropagation();
      },
      true
    );
  } catch (_) {}

  async function requestWakeLock() {
    try {
      if ('wakeLock' in navigator && document.visibilityState === 'visible') {
        await navigator.wakeLock.request('screen');
      }
    } catch (_) {}
  }
  requestWakeLock();
  document.addEventListener('visibilitychange', requestWakeLock);

  setInterval(() => {
    lastHeartbeatTime = Date.now();
    try {
      window.dispatchEvent(new Event('focus'));
    } catch (_) {}
    updateHud();
  }, 30000);

  // =========================================================================
  // 2. AUTO-ALLOW & REVIEW ENGINE (v1.3.4)
  // =========================================================================

  const STRICT_ALLOW_REGEX = /^(allow|allow once|always allow|allow all|allow access|grant permission|grant access|approve|approve all|confirm action|proceed|accept)$/i;
  const FORBIDDEN_REGEX = /\b(deny|block|cancel|reject|dismiss|delete|remove|never|refuse|quit|exit|close)\b/i;

  function isVisibleFast(el) {
    if (!el || !el.isConnected) return false;
    return el.offsetWidth > 0 || el.offsetHeight > 0 || el.getClientRects().length > 0;
  }

  function isExcluded(el) {
    if (!el) return true;
    if (location.href.includes('whatsapp.com')) return true;
    if (el.closest('[class*="whatsapp" i], [id*="whatsapp" i], [data-testid*="whatsapp" i], [aria-label*="WhatsApp" i]')) {
      return true;
    }
    return false;
  }

  function triggerTrustedClick(el) {
    if (!el) return;
    const target = el.closest('button, [role="button"], a') || el;
    try { target.focus(); } catch (_) {}

    const mouseEvents = ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click'];
    mouseEvents.forEach((type) => {
      try {
        const evt = new MouseEvent(type, {
          bubbles: true,
          cancelable: true,
          composed: true,
          view: window,
          detail: 1,
          buttons: 1,
        });
        target.dispatchEvent(evt);
        if (target !== el) {
          el.dispatchEvent(evt);
        }
      } catch (_) {}
    });

    if (typeof target.click === 'function') {
      try { target.click(); } catch (_) {}
    }
    if (target !== el && typeof el.click === 'function') {
      try { el.click(); } catch (_) {}
    }
  }

  function triggerReviewBurst() {
    const delays = [80, 200, 450, 800, 1300, 1900];
    delays.forEach((ms) => {
      setTimeout(() => {
        if (!isPaused) scanAndHandle(document);
      }, ms);
    });
  }

  function handleAllowButton(btn) {
    if (isPaused || !btn || clickedElements.has(btn) || btn.disabled) return false;
    if (!isVisibleFast(btn) || isExcluded(btn)) return false;
    if (btn.dataset.museKeeperHandled === 'true') return false;

    const rawText = (btn.innerText || btn.textContent || '').trim();
    const ariaLabel = (btn.getAttribute('aria-label') || '').trim();
    const label = rawText || ariaLabel;
    if (!label) return false;

    if (FORBIDDEN_REGEX.test(rawText) || FORBIDDEN_REGEX.test(ariaLabel)) return false;

    const cleanLabel = label.trim().toLowerCase();
    if (STRICT_ALLOW_REGEX.test(cleanLabel) || cleanLabel === 'allow' || cleanLabel === 'always allow' || cleanLabel === 'approve') {
      clickedElements.add(btn);
      btn.dataset.museKeeperHandled = 'true';
      allowedCount++;
      log(`⚡ Auto-pressed [Allow]: "${label}"`, btn);
      triggerTrustedClick(btn);
      showNotification(`Auto-Allowed: ${label.slice(0, 20)}`);
      updateHud();
      return true;
    }
    return false;
  }

  function handleReviewElement(el) {
    if (isPaused || !el || clickedElements.has(el)) return false;
    if (!isVisibleFast(el) || isExcluded(el)) return false;
    if (el.dataset.museKeeperHandled === 'true') return false;

    // Cooldown check (don't click multiple review buttons within 500ms)
    const now = Date.now();
    if (now - lastActionTime < 500) return false;

    const rawText = (el.innerText || el.textContent || '').trim();
    if (FORBIDDEN_REGEX.test(rawText)) return false;

    clickedElements.add(el);
    el.dataset.museKeeperHandled = 'true';
    lastActionTime = now;
    reviewedCount++;
    log(`⚡ Auto-opened [Review] drawer: "${rawText.slice(0, 30)}"`, el);
    triggerTrustedClick(el);
    showNotification(`Auto-Review: ${rawText.slice(0, 20)}`);
    updateHud();
    triggerReviewBurst();
    return true;
  }

  function scanAndHandle(root) {
    if (isPaused || !root) return;

    // --- STEP 1: Scan for "A task needs review" banner & Review button ---
    const allCandidates = root.querySelectorAll('button, [role="button"], a, span, div, p');
    let reviewHandled = false;

    for (let i = 0; i < allCandidates.length; i++) {
      const el = allCandidates[i];
      if (!isVisibleFast(el) || isExcluded(el) || clickedElements.has(el)) continue;

      const txt = (el.innerText || el.textContent || '').trim().toLowerCase();

      // Case A: Element text is strictly "Review" or "Review changes" or "Needs review"
      if (txt === 'review' || txt === 'review changes' || txt === 'needs review' || txt === 'review task') {
        const clickable = el.closest('button, [role="button"], a') || el;
        if (handleReviewElement(clickable)) {
          reviewHandled = true;
          break;
        }
      }
    }

    if (!reviewHandled) {
      // Case B: Search for container having "needs review" or "task needs review"
      const banners = root.querySelectorAll('div, [role="alert"], [class*="banner" i], [class*="card" i], footer, section');
      for (let i = 0; i < banners.length; i++) {
        const b = banners[i];
        if (!isVisibleFast(b) || isExcluded(b)) continue;
        const bText = (b.innerText || b.textContent || '').toLowerCase();
        if (bText.includes('task needs review') || bText.includes('needs review')) {
          const clickables = b.querySelectorAll('button, [role="button"], a, span, div');
          for (let j = 0; j < clickables.length; j++) {
            const c = clickables[j];
            const cText = (c.innerText || c.textContent || '').trim().toLowerCase();
            if (cText === 'review' || cText.includes('review') || c.classList.contains('text-blue-500') || c.getAttribute('role') === 'button') {
              if (handleReviewElement(c)) {
                reviewHandled = true;
                break;
              }
            }
          }
          if (reviewHandled) break;
        }
      }
    }

    // --- STEP 2: Scan for "Allow" / "Approve" buttons (e.g. in opened modal/drawer) ---
    const buttons = root.querySelectorAll('button, [role="button"], a, input[type="button"], input[type="submit"]');
    for (let i = 0; i < buttons.length; i++) {
      handleAllowButton(buttons[i]);
    }
  }

  // Auto-confirm standard browser dialogs
  try {
    window.confirm = function (msg) {
      if (isPaused) return true;
      log(`Auto-confirming browser dialog: "${msg}"`);
      allowedCount++;
      showNotification(`Auto-confirmed dialog`);
      updateHud();
      return true;
    };
    window.alert = function () { return true; };
  } catch (_) {}

  // =========================================================================
  // 3. LOW-OVERHEAD MUTATION OBSERVER
  // =========================================================================

  let scanScheduled = false;
  function scheduleScan() {
    if (isPaused || scanScheduled) return;
    scanScheduled = true;
    requestAnimationFrame(() => {
      scanScheduled = false;
      scanAndHandle(document);
    });
  }

  const observer = new MutationObserver((mutations) => {
    if (isPaused) return;
    for (let i = 0; i < mutations.length; i++) {
      if (mutations[i].addedNodes.length > 0) {
        scheduleScan();
        break;
      }
    }
  });

  function startObserver() {
    if (document.body) {
      observer.observe(document.body, { childList: true, subtree: true });
      scanAndHandle(document.body);
    } else {
      document.addEventListener('DOMContentLoaded', startObserver, { once: true });
    }
  }
  startObserver();

  // 1-second gentle poll fallback
  setInterval(() => {
    if (!isPaused) scanAndHandle(document);
  }, 1000);

  // =========================================================================
  // 4. TOP-BAR COMPACT HUD (CLICKABLE PAUSE/RESUME)
  // =========================================================================

  let hudEl = null;
  let hudTextEl = null;
  let hudDotEl = null;
  let notifEl = null;

  function togglePause() {
    isPaused = !isPaused;
    updateHud();
    log(isPaused ? '⏸ Paused by user' : '▶ Resumed by user');
  }

  function createHud() {
    let existing = document.getElementById('muse-keeper-hud');
    if (existing) existing.remove();

    hudEl = document.createElement('div');
    hudEl.id = 'muse-keeper-hud';
    hudEl.title = 'Click to Pause / Resume Auto-Allow';
    hudEl.style.cssText = `
      position: fixed;
      top: 10px;
      right: 75px;
      z-index: 2147483647;
      display: flex;
      align-items: center;
      gap: 5px;
      background: rgba(18, 18, 24, 0.92);
      border: 1px solid rgba(168, 85, 247, 0.5);
      border-radius: 12px;
      padding: 2px 7px;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
      font-size: 10px;
      font-weight: 600;
      color: #f1f5f9;
      box-shadow: 0 2px 8px rgba(0, 0, 0, 0.45);
      user-select: none;
      cursor: pointer;
      pointer-events: auto;
      transition: all 0.15s ease;
    `;

    hudEl.addEventListener('mouseenter', () => {
      hudEl.style.borderColor = 'rgba(168, 85, 247, 0.9)';
      hudEl.style.transform = 'scale(1.04)';
    });
    hudEl.addEventListener('mouseleave', () => {
      hudEl.style.borderColor = isPaused ? 'rgba(239, 68, 68, 0.6)' : 'rgba(168, 85, 247, 0.5)';
      hudEl.style.transform = 'scale(1)';
    });
    hudEl.addEventListener('click', (e) => {
      e.stopPropagation();
      togglePause();
    });

    hudDotEl = document.createElement('span');
    hudDotEl.style.cssText = `
      width: 6px;
      height: 6px;
      border-radius: 50%;
      background: #10b981;
      box-shadow: 0 0 5px #10b981;
      flex-shrink: 0;
    `;

    hudTextEl = document.createElement('span');
    hudTextEl.innerText = 'MKALL: 0';

    hudEl.appendChild(hudDotEl);
    hudEl.appendChild(hudTextEl);
    (document.body || document.documentElement).appendChild(hudEl);
  }

  function updateHud() {
    if (!hudTextEl || !hudDotEl) return;
    if (isPaused) {
      hudDotEl.style.background = '#ef4444';
      hudDotEl.style.boxShadow = '0 0 5px #ef4444';
      hudTextEl.innerText = 'MKALL: PAUSED';
      if (hudEl) hudEl.style.borderColor = 'rgba(239, 68, 68, 0.6)';
      return;
    }

    hudDotEl.style.background = '#10b981';
    hudDotEl.style.boxShadow = '0 0 5px #10b981';
    if (hudEl) hudEl.style.borderColor = 'rgba(168, 85, 247, 0.5)';

    const total = allowedCount + reviewedCount;
    if (total > 0) {
      hudTextEl.innerText = reviewedCount > 0
        ? `MKALL: ${allowedCount} (rev: ${reviewedCount})`
        : `MKALL: ${allowedCount}`;
    } else {
      hudTextEl.innerText = 'MKALL: 0';
    }
  }

  function showNotification(text) {
    try {
      if (!notifEl) {
        notifEl = document.createElement('div');
        notifEl.style.cssText = `
          position: fixed;
          bottom: 24px;
          right: 24px;
          background: rgba(15, 23, 42, 0.95);
          border: 1px solid #10b981;
          color: #f8fafc;
          padding: 8px 14px;
          border-radius: 8px;
          font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
          font-size: 12px;
          font-weight: 500;
          box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
          z-index: 2147483647;
          pointer-events: none;
          transition: opacity 0.3s ease, transform 0.3s ease;
          opacity: 0;
          transform: translateY(10px);
        `;
        (document.body || document.documentElement).appendChild(notifEl);
      }
      notifEl.innerText = `⚡ ${text}`;
      notifEl.style.opacity = '1';
      notifEl.style.transform = 'translateY(0)';
      setTimeout(() => {
        if (notifEl) {
          notifEl.style.opacity = '0';
          notifEl.style.transform = 'translateY(10px)';
        }
      }, 2500);
    } catch (_) {}
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', createHud, { once: true });
  } else {
    createHud();
  }
})();
