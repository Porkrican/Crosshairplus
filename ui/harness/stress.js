(() => {
  const errors = [];
  const checks = [];
  const post = (payload) => window.ipc.postMessage(JSON.stringify({ v: 1, type: "StressReport", payload }));
  const check = (name, ok) => { if (!ok) errors.push("check: " + name); checks.push({ name, ok: !!ok }); };
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const click = (selector) => { const el = document.querySelector(selector); if (el) { el.click(); return true; } return false; };
  window.addEventListener("error", (event) => errors.push("error: " + (event.message || event.type)));
  window.addEventListener("unhandledrejection", (event) => errors.push("rejection: " + String((event.reason && event.reason.message) || event.reason)));
  window.__qaFullStates = 0;
  if (typeof window.__crosshair_onMessage === "function") {
    const inner = window.__crosshair_onMessage;
    window.__crosshair_onMessage = (message) => {
      if (message && message.type === "FullState") { window.__qaFullStates++; }
      return inner(message);
    };
  }
  const waitForStates = async (target, ms) => {
    const start = Date.now();
    while (window.__qaFullStates < target && Date.now() - start < ms) { await wait(25); }
    return window.__qaFullStates >= target;
  };
  (async () => {
    const S = window.__crosshairState;
    const send = window.__crosshairSend;
    const drain = async () => {
      const target = window.__qaFullStates + 2;
      send("ToggleMaster", { module: "crosshair" });
      send("ToggleMaster", { module: "crosshair" });
      const ok = await waitForStates(target, 8000);
      await wait(250);
      return ok;
    };
    const cycle = async (round) => {
      const tabs = ["aim", "display", "audio", "slots"];
      for (let i = 0; i < 120; i++) { window.__crosshair_setTab(tabs[i % tabs.length]); }
      for (let i = 0; i < 20; i++) {
        click('[data-action="settings"]'); click('[data-action="close-settings"]');
        click('[data-action="open-backup"]'); click('[data-action="close-backup-modal"]');
        click('[data-action="open-import"]'); click('[data-action="close-import-modal"]');
        click('[data-action="open-preset"]'); click('[data-action="close-preset-modal"]');
        click('[data-action="fix-display"]'); click('[data-action="close-fx-modal"]');
      }
      const base = S.config.crosshair;
      for (let batch = 0; batch < 10; batch++) {
        for (let i = 0; i < 50; i++) {
          const extreme = (batch + i) % 2 === 0
            ? { size: 48, thickness: 10, gap: 24, opacity: 10 }
            : { size: 2, thickness: 1, gap: 0, opacity: 100 };
          send("UpdateCrosshair", { crosshair: Object.assign({}, base, extreme) });
        }
        await wait(20);
      }
      for (let batch = 0; batch < 4; batch++) {
        for (let i = 0; i < 50; i++) {
          send("UpdateDisplay", { display: S.config.display });
          send("UpdateSound", { audio: S.config.audio });
        }
        await wait(20);
      }
      await wait(round === 0 ? 400 : 600);
    };
    try {
      check("bridge", !!S && typeof send === "function" && typeof window.__crosshair_setTab === "function");
      const focus = window.__crosshairStressFocus;
      if (focus) {
        await wait(600);
        const base = S.config.crosshair;
        post({ phase: "baseline", ok: true, errors, checks: [{ name: "focus-" + focus, ok: true }] });
        if (focus === "crosshair") {
          for (let i = 0; i < 300; i++) { send("UpdateCrosshair", { crosshair: Object.assign({}, base, { size: 10 + (i % 30) }) }); }
        } else if (focus === "display") {
          for (let i = 0; i < 300; i++) { send("UpdateDisplay", { display: S.config.display }); }
        } else if (focus === "sound") {
          for (let i = 0; i < 300; i++) { send("UpdateSound", { audio: S.config.audio }); }
        } else if (focus === "tabs") {
          for (let i = 0; i < 600; i++) { window.__crosshair_setTab(["aim", "display", "audio", "slots"][i % 4]); }
        } else if (focus === "modals" || ["settings", "backup", "import", "preset", "fx"].indexOf(focus) >= 0) {
          const pairs = {
            settings: ['[data-action="settings"]', '[data-action="close-settings"]'],
            backup: ['[data-action="open-backup"]', '[data-action="close-backup-modal"]'],
            import: ['[data-action="open-import"]', '[data-action="close-import-modal"]'],
            preset: ['[data-action="open-preset"]', '[data-action="close-preset-modal"]'],
            fx: ['[data-action="fix-display"]', '[data-action="close-fx-modal"]'],
          };
          const chosen = focus === "modals" ? Object.keys(pairs) : [focus];
          for (let i = 0; i < 100; i++) {
            for (const key of chosen) { click(pairs[key][0]); click(pairs[key][1]); }
          }
        }
        await drain();
        await wait(800);
        post({ phase: "final", ok: errors.length === 0, errors, checks: [{ name: "focus-" + focus + "-done", ok: true }] });
        return;
      }
      if (window.__crosshairStressIdle) {
        await wait(15000);
        post({ phase: "baseline", ok: true, errors, checks: [{ name: "idle-baseline", ok: true }] });
        await wait(15000);
        check("idle-alive", !!window.__crosshairReady);
        post({ phase: "final", ok: errors.length === 0, errors, checks });
        return;
      }
      window.__xss = undefined;
      const hostile = '<img src=x onerror="window.__xss=1"><script>window.__xss=2</script>';
      const beforePreset = window.__qaFullStates;
      send("SavePreset", { module: "crosshair", name: hostile });
      const presetHandled = await waitForStates(beforePreset + 1, 3000);
      window.__crosshair_setTab("aim");
      click('[data-action="open-preset"]');
      await wait(200);
      const menu = document.querySelector("#aimPresetMenu");
      const names = (S.crosshairPresets || []).map((preset) => String(preset.name || ""));
      const created = names.find((name) => name.indexOf("<img src=x") >= 0);
      check("xss-preset-created", !!created);
      check("xss-no-handler", window.__xss === undefined);
      check("xss-no-element", !document.querySelector('img[src="x"]') && !document.querySelector("#aimPresetMenu script"));
      check("xss-literal", !!created && !!menu && (menu.textContent || "").indexOf("<img src=x") >= 0);
      click('[data-action="close-preset-modal"]');
      const preset = (S.crosshairPresets || []).find((entry) => String(entry.name || "").indexOf("<img src=x") >= 0);
      if (preset) {
        const beforeDelete = window.__qaFullStates;
        send("DeletePreset", { module: "crosshair", id: preset.id });
        await waitForStates(beforeDelete + 1, 3000);
      }
      await cycle(0);
      const drained = await drain();
      check("drain", drained);
      window.__crosshair_setTab("aim");
      await wait(80);
      check("tabs", document.body.dataset.tab === "aim");
      check("modals-closed", !!document.querySelector("#settingsModal")?.classList.contains("hidden") && !!document.querySelector("#fxModal")?.classList.contains("hidden"));
      check("layout-width", document.documentElement.scrollWidth <= window.innerWidth + 1);
      post({ phase: "baseline", ok: errors.length === 0, errors, checks, debug: { presetFound: !!created, presetHandled, presetCount: names.length, menuLen: (menu && menu.textContent || "").length } });
      await cycle(1);
      await cycle(2);
      const drainedFinal = await drain();
      await wait(1500);
      check("drain-final", drainedFinal);
      check("layout-width-final", document.documentElement.scrollWidth <= window.innerWidth + 1);
      check("ui-alive", !!document.querySelector("#main") && !!window.__crosshairReady);
      post({ phase: "final", ok: errors.length === 0, errors, checks, debug: { presetFound: !!created, presetCount: names.length } });
    } catch (err) {
      errors.push("fatal: " + String((err && err.message) || err));
      post({ phase: "final", ok: false, errors, checks });
    }
  })();
})();