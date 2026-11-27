
(async () => {
  const result = { ok: false, steps: [], errors: [], console: [], setup: [] };
  const finish = () => {
    result.errors = (window.__crosshairVerifyErrors || []).slice();
    result.console = (window.__crosshairVerifyConsole || []).slice();
    result.ok = result.steps.length === 7
      && result.steps.every((step) => step.ok === true)
      && result.errors.length === 0
      && result.console.length === 0
      && result.setup.length === 0;
    try {
      window.ipc.postMessage(JSON.stringify({ v: 1, type: "VerifyReport", payload: result }));
    } catch (_) {}
  };
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const waitFor = async (check, timeout = 6000) => {
    const started = Date.now();
    while (Date.now() - started < timeout) {
      const value = check();
      if (value) return value;
      await sleep(50);
    }
    return null;
  };
  window.__crosshairOverlayChecked = 0;
  let overlayCheckSeq = 0;
  const checkOverlay = async (path) => {
    overlayCheckSeq += 1;
    const expected = overlayCheckSeq;
    try {
      window.ipc.postMessage(JSON.stringify({ v: 1, type: "VerifyOverlay", payload: { path } }));
    } catch (_) {
      return false;
    }
    return !!(await waitFor(() => (window.__crosshairOverlayChecked || 0) >= expected, 3000));
  };
  const useStored = async (name) => {
    const events = [];
    const original = window.__crosshair_onMessage;
    window.__crosshair_onMessage = (message) => {
      original(message);
      if (message?.type === "StoredImageResult") {
        events.push({ name: message.payload?.name || "", ok: message.payload?.ok === true });
      }
    };
    document.body.click();
    document.querySelector("#recentTrigger")?.click();
    window.__crosshair_onMessage({ v: 1, type: "RecentImages", payload: { images: [{ name, label: name }] } });
    document.querySelector("#recentMenu .recent-pick")?.click();
    const result = await waitFor(() => events.find((event) => event.name === name && event.ok), 5000);
    const applied = !!(await waitFor(() => (S.config.crosshair.image || "").endsWith(name)
      && S.pendingStoredImage === null && S.aimMode === "image", 5000));
    window.__crosshair_onMessage = original;
    return { result, applied };
  };
  // Remove and delete share the same proof: the action reaches Rust and the
  // resulting FullState carries no image. The action itself only differs.
  const imageClearedAfter = async (act) => {
    const states = [];
    const original = window.__crosshair_onMessage;
    window.__crosshair_onMessage = (message) => {
      original(message);
      if (message?.type === "FullState") {
        states.push(message.payload?.config?.crosshair?.image ?? null);
      }
    };
    try {
      act();
      return !!(await waitFor(() => states.some((image) => image === null), 5000));
    } finally {
      window.__crosshair_onMessage = original;
    }
  };
  const removeImage = () => imageClearedAfter(() => {
    const button = document.createElement("button");
    button.dataset.action = "remove-image";
    document.body.append(button);
    button.click();
    button.remove();
  });
  const deleteStored = (name) => imageClearedAfter(() => {
    document.body.click();
    document.querySelector("#recentTrigger")?.click();
    window.__crosshair_onMessage({ v: 1, type: "RecentImages", payload: { images: [{ name, label: name }] } });
    document.querySelector("#recentMenu .recent-delete")?.click();
  });
  try {
    if (!(await waitFor(() => window.__crosshairReady === true, 10000))) {
      result.setup.push("ui-not-ready");
      return finish();
    }
    const modules = [
      { module: "crosshair", name: "Sandbox Crosshair", trigger: '#aimPresetTrigger', menu: '#aimPresetMenu', del: '#aimPresetDelete' },
      { module: "display", name: "Sandbox Display", trigger: '#displayPresetTrigger', menu: '#displayPresetMenu', del: '#displayPresetDelete' },
      { module: "audio", name: "Sandbox Audio", trigger: '#audioPresetTrigger', menu: '#audioPresetMenu', del: '#audioPresetDelete' }
    ];
    for (const spec of modules) {
      const step = { module: spec.module, ok: false, created: false, saveSent: false, deleteSent: false, removed: false, id: null };
      result.steps.push(step);
      const saveButton = document.querySelector(`[data-action="save-preset"][data-module="${spec.module}"]`);
      if (!saveButton) { step.error = "missing-save-control"; continue; }
      window.__crosshairAutoConfirm = true;
      saveButton.click();
      const nameInput = document.querySelector('#presetName');
      const submit = document.querySelector('#presetModal [data-action="submit-preset"]');
      if (!nameInput || !submit) { step.error = "missing-preset-modal"; continue; }
      nameInput.value = spec.name;
      submit.click();
      step.saveSent = !!(window.__lastSent && window.__lastSent.type === "SavePreset" && window.__lastSent.payload && window.__lastSent.payload.module === spec.module && window.__lastSent.payload.name === spec.name);
      const menu = document.querySelector(spec.menu);
      const item = await waitFor(() => Array.from(menu?.querySelectorAll('.preset-option') || []).find((node) => (node.textContent || "").includes(spec.name)));
      if (!item) { step.error = "custom-preset-not-rendered"; continue; }
      step.created = true;
      step.id = item.dataset.id || null;
      const deleteButton = await waitFor(() => { const button = document.querySelector(spec.del); return button && !button.disabled ? button : null; });
      if (!deleteButton) { step.error = "missing-delete-control"; continue; }
      deleteButton.click();
      // Auto-confirm resolves the confirmation promise asynchronously; wait for
      // the real DeletePreset instead of sampling the previous command.
      step.deleteSent = !!(await waitFor(() => window.__lastSent && window.__lastSent.type === "DeletePreset" && window.__lastSent.payload && window.__lastSent.payload.module === spec.module && window.__lastSent.payload.id === step.id));
      const removed = await waitFor(() => !Array.from(menu?.querySelectorAll('.preset-option') || []).some((node) => node.dataset.id === step.id));
      step.removed = !!removed;
      step.ok = step.created && step.saveSent && step.deleteSent && step.removed;
      if (!step.ok && !step.error) step.error = "delete-not-confirmed";
    }
    const live = "live-probe.png";
    const imageStep = { module: "image-vanished", ok: false, initial: false, gone: false, cleared: false, result: false, banner: false, ordered: false, repainted: false, pendingCleared: false, overlay: false };
    result.steps.push(imageStep);
    window.__crosshair_setTab("aim");
    imageStep.initial = !!(await waitFor(() => (S.config.crosshair.image || "").endsWith(live) && S.previewTick > 0, 5000));
    if (!imageStep.initial) {
      imageStep.error = "initial-image-not-live";
    } else {
      // The Rust verifier deletes the file only after this flag; the page can
      // then fail the real UseStoredImage for the live name without racing it.
      window.__crosshairLiveArmed = true;
      imageStep.gone = !!(await waitFor(() => window.__crosshairLiveGone === true, 5000));
      if (!imageStep.gone) {
        imageStep.error = "live-file-not-deleted";
      } else {
        const before = { tick: S.previewTick, src: document.querySelector("#aimPreview")?.src || "", mode: S.aimMode };
        const events = [];
        const original = window.__crosshair_onMessage;
        window.__crosshair_onMessage = (message) => {
          original(message);
          if (message?.type === "FullState") {
            events.push({ type: "FullState", image: message.payload?.config?.crosshair?.image ?? null, pending: S.pendingStoredImage });
          } else if (message?.type === "StoredImageResult") {
            events.push({ type: "StoredImageResult", name: message.payload?.name || "", ok: message.payload?.ok === true });
          } else if (message?.type === "DiagnosticBanner" && message.payload?.code === "image") {
            events.push({ type: "DiagnosticBanner", message: message.payload?.message || "" });
          }
        };
        document.body.click();
        document.querySelector("#recentTrigger")?.click();
        window.__crosshair_onMessage({ v: 1, type: "RecentImages", payload: { images: [{ name: live, label: live }] } });
        document.querySelector("#recentMenu .recent-pick")?.click();
        const full = await waitFor(() => events.find((event) => event.type === "FullState" && event.image === null), 5000);
        const resultEvent = await waitFor(() => events.find((event) => event.type === "StoredImageResult" && event.name === live && event.ok === false), 5000);
        const banner = await waitFor(() => events.find((event) => event.type === "DiagnosticBanner" && event.message.includes("no longer in the crosshairs folder")), 5000);
        const fullIndex = full ? events.indexOf(full) : -1;
        const resultIndex = resultEvent ? events.indexOf(resultEvent) : -1;
        const bannerIndex = banner ? events.indexOf(banner) : -1;
        imageStep.cleared = !!(full && full.pending === live);
        imageStep.result = !!(resultEvent && resultEvent.name === live && resultEvent.ok === false);
        imageStep.banner = !!banner;
        imageStep.ordered = fullIndex >= 0 && fullIndex < resultIndex && resultIndex < bannerIndex;
        imageStep.repainted = !!(await waitFor(() => S.previewTick > before.tick && (document.querySelector("#aimPreview")?.src || "") !== before.src, 5000));
        imageStep.pendingCleared = S.pendingStoredImage === null;
        imageStep.modeKept = S.aimMode === before.mode;
        imageStep.before = before;
        imageStep.after = { image: S.config.crosshair.image, tick: S.previewTick, src: document.querySelector("#aimPreview")?.src || "" };
        window.__crosshair_onMessage = original;
        imageStep.overlay = await checkOverlay("vanished");
        imageStep.ok = imageStep.initial && imageStep.gone && imageStep.cleared && imageStep.result
          && imageStep.banner && imageStep.ordered && imageStep.repainted
          && imageStep.pendingCleared && imageStep.modeKept && imageStep.overlay
          && S.config.crosshair.image === null;
        if (!imageStep.ok && !imageStep.error) imageStep.error = "image-cleanup-not-confirmed";
      }
    }

    const useStep = { module: "image-use", ok: false, result: false, applied: false, overlay: false };
    result.steps.push(useStep);
    const used = await useStored("use-probe.png");
    useStep.result = !!used.result;
    useStep.applied = !!used.applied;
    useStep.overlay = await checkOverlay("use");
    useStep.ok = useStep.result && useStep.applied && useStep.overlay;
    if (!useStep.ok) useStep.error = "use-overlay-not-confirmed";

    const removeStep = { module: "image-remove", ok: false, applied: false, overlay: false };
    result.steps.push(removeStep);
    removeStep.applied = await removeImage();
    removeStep.overlay = await checkOverlay("remove");
    removeStep.ok = removeStep.applied && removeStep.overlay;
    if (!removeStep.ok) removeStep.error = "remove-overlay-not-confirmed";

    const deleteStep = { module: "image-delete", ok: false, prepared: false, applied: false, overlay: false };
    result.steps.push(deleteStep);
    const prepared = await useStored("use-probe.png");
    deleteStep.prepared = !!(prepared.result && prepared.applied);
    if (deleteStep.prepared) {
      deleteStep.applied = await deleteStored("use-probe.png");
    }
    deleteStep.overlay = await checkOverlay("delete");
    deleteStep.ok = deleteStep.prepared && deleteStep.applied && deleteStep.overlay;
    if (!deleteStep.ok) deleteStep.error = "delete-overlay-not-confirmed";
  } catch (error) {
    result.setup.push("exception: " + String(error));
  }
  finish();
})();
