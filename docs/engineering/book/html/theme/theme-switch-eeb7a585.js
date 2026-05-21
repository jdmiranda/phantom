// Phantom Engineering Docs — 6-palette switcher
// Injects a row of theme buttons into mdBook's top menu bar.
// Persists choice in localStorage so the palette survives page navigation.

(function () {
  'use strict';

  const PALETTES = ['phosphor', 'amber', 'ice', 'blood', 'vapor', 'cyber'];
  const LS_KEY = 'pf-engineering-palette';
  const DEFAULT = 'phosphor';

  function applyPalette(name) {
    if (!PALETTES.includes(name)) name = DEFAULT;
    document.documentElement.setAttribute('data-theme', name);
    try { localStorage.setItem(LS_KEY, name); } catch (_) {}
    document.querySelectorAll('.pf-theme-switch .pf-sw').forEach((btn) => {
      btn.classList.toggle('active', btn.getAttribute('data-pf-set') === name);
    });
  }

  function buildSwitcher() {
    const wrap = document.createElement('div');
    wrap.className = 'pf-theme-switch';
    wrap.setAttribute('aria-label', 'Phantom palette');
    PALETTES.forEach((name) => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'pf-sw';
      btn.setAttribute('data-pf-set', name);
      btn.title = name;
      btn.addEventListener('click', () => applyPalette(name));
      wrap.appendChild(btn);
    });
    return wrap;
  }

  function mount() {
    // Don't double-mount on hot-reload / SPA navigation.
    if (document.querySelector('.pf-theme-switch')) return;

    const rightButtons = document.querySelector('.right-buttons');
    if (!rightButtons) {
      // mdBook may not have rendered the menu bar yet; retry once.
      setTimeout(mount, 100);
      return;
    }
    rightButtons.appendChild(buildSwitcher());

    let stored = null;
    try { stored = localStorage.getItem(LS_KEY); } catch (_) {}
    applyPalette(stored || DEFAULT);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', mount);
  } else {
    mount();
  }
})();
