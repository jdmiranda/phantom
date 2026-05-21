/* PHANTOM doc-site behaviors — minimal, additive.
 * - phosphor flicker on the body (very subtle)
 * - boot-line typer on index
 * - clock in topbar
 * - konami → switch theme
 */
(function() {
  // ---- clock ----
  const clockEl = document.querySelector('[data-clock]');
  if (clockEl) {
    const tick = () => {
      const d = new Date();
      const pad = n => String(n).padStart(2, '0');
      clockEl.textContent = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
    };
    tick();
    setInterval(tick, 1000);
  }

  // ---- subtle phosphor flicker ----
  const root = document.documentElement;
  let last = performance.now();
  function flicker(t) {
    if (t - last > 600 + Math.random() * 1400) {
      const a = 0.94 + Math.random() * 0.06;
      root.style.setProperty('--phosphor-flicker', a.toString());
      document.body.style.opacity = a;
      last = t;
      setTimeout(() => { document.body.style.opacity = '1'; }, 40);
    }
    requestAnimationFrame(flicker);
  }
  if (!window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
    requestAnimationFrame(flicker);
  }

  // ---- boot typer (on index only) ----
  const bootBox = document.querySelector('[data-boot]');
  if (bootBox) {
    const lines = [
      ['NEURAL CORE',     'ONLINE'],
      ['RENDER ENGINE',   'wgpu/metal · 60fps locked'],
      ['AGENT MESH',      '12 roles registered · capability gate armed'],
      ['MEMORY BANKS',    'per-project · jsonl event log'],
      ['SUPERVISOR LINK', 'heartbeat 10s · auto-restart 5/60s'],
      ['LOOP OVERSEER',   '4 specs loaded · pr_finder · reviewer · implementer'],
      ['BRAIN OODA',      'observe/orient/decide/act · self-improvement OPT-IN'],
      ['SYSTEM',          'READY'],
    ];
    let idx = 0;
    const renderLine = (label, value, complete=false) => {
      const dots = '.'.repeat(Math.max(2, 30 - label.length));
      const cls = complete ? 'bright' : 'muted';
      return `<div class="boot-line"><span class="amber">[ ${label} ]</span> <span class="muted">${dots}</span> <span class="${cls}">${value}</span></div>`;
    };
    const step = () => {
      if (idx >= lines.length) {
        bootBox.innerHTML += `<div class="boot-line" style="margin-top:8px"><span class="bright">SYSTEM READY</span><span class="cursor"></span></div>`;
        return;
      }
      bootBox.innerHTML += renderLine(lines[idx][0], lines[idx][1], true);
      idx++;
      setTimeout(step, 220 + Math.random() * 180);
    };
    setTimeout(step, 240);
  }

  // ---- konami easter egg → cycle theme classes ----
  const themes = ['theme-phosphor','theme-amber','theme-ice','theme-blood','theme-vapor'];
  let kIdx = 0;
  const seq = ['ArrowUp','ArrowUp','ArrowDown','ArrowDown','ArrowLeft','ArrowRight','ArrowLeft','ArrowRight','b','a'];
  let kPos = 0;
  window.addEventListener('keydown', e => {
    if (e.key === seq[kPos]) {
      kPos++;
      if (kPos === seq.length) {
        kPos = 0;
        kIdx = (kIdx + 1) % themes.length;
        themes.forEach(t => document.documentElement.classList.remove(t));
        document.documentElement.classList.add(themes[kIdx]);
        const note = document.createElement('div');
        note.textContent = `>> theme: ${themes[kIdx].replace('theme-','')}`;
        note.style.cssText = 'position:fixed;bottom:24px;right:24px;padding:8px 14px;border:2px solid var(--color-accent);background:var(--color-surface);color:var(--color-accent);text-shadow:var(--glow);z-index:9999;font-size:11px;letter-spacing:0.12em;text-transform:uppercase';
        document.body.appendChild(note);
        setTimeout(() => note.remove(), 1800);
      }
    } else {
      kPos = 0;
    }
  });
})();
